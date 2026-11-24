#!/usr/bin/env python3
"""Discover a complete Datadog Agent artifact set for `.datadog-agent-version.json`.

`nightly` looks for an Agent `main` nightly whose Windows, Linux, macOS and debug-package artifacts all come from one
commit and one upstream pipeline. `release` looks for the newest complete RC or stable release within a release
line's target minor. Both read public registries and package buckets, hash every package, and compare the result with
the version file on the branch being updated.

The outcome is written to `--output-dir`:

- `summary.json` always, with a `status` of `candidate`, `skipped` or `error`, and the reason.
- For a candidate, also `datadog-agent-version.json`, `pr-title.txt` and `pr-body.md`, plus
  `merged-datadog-agent-version.json`, a copy of the version file it was compared with, when there is one.

A skip is normal: a newer build isn't complete yet, or isn't newer. It exits 0 and changes nothing. Anything this tool
doesn't expect is an error and exits 1: a registry or bucket that keeps failing past the retry budget, a malformed
response, or an artifact whose name, labels or platforms don't match the format the tool knows. Each error names the
artifact and what was expected, so a format change upstream points at the code to update.
"""

import argparse
import datetime
import hashlib
import http.client
import json
import os
import re
import socket
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

# `agent_version` lives one directory up, in `ci/tooling`.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import agent_version  # noqa: E402


DOCKER_HUB = "registry-1.docker.io"
DATADOG_REGISTRY = "registry.datadoghq.com"
NIGHTLY_REPOSITORY = "datadog/agent-dev"
RELEASE_REPOSITORY = "agent"
# Where the Agent publishes its images. Discovery fails, rather than skips, when these stop matching what's published.
WINDOWS_NIGHTLY_TAG = "nightly-py3-jmx-win-servercore"
LINUX_NIGHTLY_TAG = "nightly-full-main-{sha8}-jmx"
LINUX_RELEASE_TAG = "{version}-full"
WINDOWS_RELEASE_TAG = "{version}-servercore-ltsc2022-jmx"
AGENT_REPOSITORY = "DataDog/datadog-agent"
UPSTREAM_PIPELINE_URL = "https://gitlab.ddbuild.io/DataDog/datadog-agent/-/pipelines/{}"
VERSION_LABEL = "org.opencontainers.image.version"
REVISION_LABEL = "org.opencontainers.image.revision"

# Patch RCs are not proposed: once a line reaches stable it moves only to stable patches.
ACCEPT_PATCH_RCS = False

MANIFEST_MEDIA_TYPES = (
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
)
INDEX_MEDIA_TYPES = MANIFEST_MEDIA_TYPES[:2]
RETRY_STATUSES = {429, 500, 502, 503, 504}
TRANSIENT_ERRORS = (urllib.error.URLError, http.client.HTTPException, socket.timeout, ConnectionError, TimeoutError)
CHUNK_SIZE = 1 << 20
FAILURE_DOCS = "docs/development/agent-version.md#when-an-update-workflow-fails"


class DiscoveryError(Exception):
    """Discovery could not complete; the run fails and the version file is kept."""


class Skip(Exception):
    """A candidate is incomplete or not newer; the run reports it and keeps the version file."""


class Http:
    """HTTP client that retries transient failures until a shared deadline.

    `transport(method, url, headers)` returns `(status, headers, chunks)`, where `headers` has lowercase names and
    `chunks` iterates over the body. Failures while reading the body are retried like failed requests.
    """

    def __init__(self, transport, budget_seconds=900, sleep=time.sleep, clock=time.monotonic):
        self.transport = transport
        self.deadline = clock() + budget_seconds
        self.sleep = sleep
        self.clock = clock

    def request(self, method, url, headers=None, consume=lambda chunks: b"".join(chunks)):
        """Return `(status, headers, consume(chunks))`, retrying 429, 5xx and network errors."""
        delay = 1
        while True:
            try:
                status, response_headers, chunks = self.transport(method, url, headers or {})
                if status not in RETRY_STATUSES:
                    return status, response_headers, consume(chunks)
                problem = f"HTTP {status}"
                retry_after = response_headers.get("retry-after", "")
                if retry_after.isdigit():
                    delay = max(delay, int(retry_after))
            except TRANSIENT_ERRORS as error:
                problem = f"{type(error).__name__}: {error}"
            if self.clock() + delay > self.deadline:
                raise DiscoveryError(f"{method} {url} failed after retries: {problem}")
            print(f"[*] {method} {url}: {problem}; retrying in {delay}s", file=sys.stderr)
            self.sleep(delay)
            delay = min(delay * 2, 60)

    def json(self, url, headers=None):
        status, _, body = self.request("GET", url, headers)
        if status != 200:
            raise DiscoveryError(f"GET {url} returned HTTP {status}")
        return parse_json(body, url)


def urllib_transport(method, url, headers):
    request = urllib.request.Request(url, method=method)
    for name, value in headers.items():
        # Unredirected headers stay on this request, so a registry token never reaches a storage redirect.
        if name.lower() == "authorization":
            request.add_unredirected_header(name, value)
        else:
            request.add_header(name, value)
    try:
        response = urllib.request.urlopen(request, timeout=60)
    except urllib.error.HTTPError as error:
        return error.code, {key.lower(): value for key, value in error.headers.items()}, iter([error.read()])

    def chunks():
        with response:
            expected = response.headers.get("content-length")
            received = 0
            while chunk := response.read(CHUNK_SIZE):
                received += len(chunk)
                yield chunk
            if method != "HEAD" and expected is not None and received != int(expected):
                raise http.client.IncompleteRead(b"", int(expected) - received)

    return response.status, {key.lower(): value for key, value in response.headers.items()}, chunks()


def parse_json(body, source):
    try:
        return json.loads(body)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise DiscoveryError(f"{source} returned malformed JSON: {error}") from error


def sha256_digest(body):
    return "sha256:" + hashlib.sha256(body).hexdigest()


class Registry:
    """Anonymous, read-only client for one repository in an OCI registry."""

    def __init__(self, http, host, repository):
        self.http = http
        self.host = host
        self.repository = repository
        self.token = None

    def get(self, path, accept=None):
        url = f"https://{self.host}/v2/{self.repository}/{path}"
        headers = {"Accept": ", ".join(accept)} if accept else {}
        for _ in range(2):
            if self.token:
                headers["Authorization"] = f"Bearer {self.token}"
            status, response_headers, body = self.http.request("GET", url, headers)
            if status != 401 or self.token:
                return status, response_headers, body
            self.token = self.fetch_token(response_headers.get("www-authenticate", ""))
        return status, response_headers, body

    def fetch_token(self, challenge):
        if not challenge.lower().startswith("bearer "):
            raise DiscoveryError(f"{self.host} asked for unsupported authentication: {challenge!r}")
        parameters = dict(re.findall(r'(\w+)="([^"]*)"', challenge))
        if "realm" not in parameters:
            raise DiscoveryError(f"{self.host} sent a token challenge without a realm")
        query = urllib.parse.urlencode({key: value for key, value in parameters.items() if key != "realm"})
        document = self.http.json(f"{parameters['realm']}?{query}")
        token = document.get("token") or document.get("access_token")
        if not isinstance(token, str):
            raise DiscoveryError(f"{self.host} returned a token response without a token")
        return token

    def manifest(self, reference):
        """Return `(digest, manifest)` for a tag or digest, or `None` when it does not exist."""
        status, headers, body = self.get(f"manifests/{reference}", MANIFEST_MEDIA_TYPES)
        if status == 404:
            return None
        if status != 200:
            raise DiscoveryError(f"{self.repository}:{reference} manifest returned HTTP {status}")
        digest = sha256_digest(body)
        if reference.startswith("sha256:") and digest != reference:
            raise DiscoveryError(f"{self.repository}@{reference} returned content with digest {digest}")
        if headers.get("docker-content-digest", digest) != digest:
            raise DiscoveryError(f"{self.repository}:{reference} returned a manifest that does not match its digest")
        manifest = parse_json(body, f"{self.repository}:{reference}")
        if not isinstance(manifest, dict):
            raise DiscoveryError(f"{self.repository}:{reference} manifest is not an object")
        return digest, manifest

    def config(self, manifest, reference):
        """Return the image config of a single-platform manifest."""
        digest = manifest.get("config", {}).get("digest") if isinstance(manifest.get("config"), dict) else None
        if not isinstance(digest, str) or not agent_version.DIGEST_RE.fullmatch(digest):
            raise DiscoveryError(f"{self.repository}@{reference} is not a single-platform image manifest")
        status, _, body = self.get(f"blobs/{digest}")
        if status != 200:
            raise DiscoveryError(f"{self.repository} config {digest} returned HTTP {status}")
        if sha256_digest(body) != digest:
            raise DiscoveryError(f"{self.repository} config {digest} does not match its digest")
        config = parse_json(body, f"{self.repository} config {digest}")
        if not isinstance(config, dict):
            raise DiscoveryError(f"{self.repository} config {digest} is not an object")
        return config

    def reference(self, digest):
        """Return the image reference for a digest in this repository."""
        repository = self.repository if self.host == DOCKER_HUB else f"{self.host}/{self.repository}"
        return f"{repository}@{digest}"


def is_index(manifest):
    return manifest.get("mediaType") in INDEX_MEDIA_TYPES or "manifests" in manifest


def children(index, name):
    entries = index.get("manifests")
    if not isinstance(entries, list):
        raise DiscoveryError(f"image index {name} has no manifest list")
    return [(entry.get("digest"), entry.get("platform") or {}) for entry in entries if isinstance(entry, dict)]


def describe_platforms(entries):
    """Return the platforms of an index's children, such as `linux/amd64, windows/amd64 10.0.20348.1`."""
    described = [
        " ".join(str(part) for part in (f"{platform.get('os')}/{platform.get('architecture')}",
                                         platform.get("os.version")) if part)
        for _, platform in entries
    ]
    return ", ".join(described) or "none"


def select_linux(index, name):
    """Return the amd64 and arm64 Linux child digests of an index."""
    entries = children(index, name)
    selected = {}
    for digest, platform in entries:
        arch = platform.get("architecture")
        if platform.get("os") == "linux" and arch in agent_version.LINUX_ARCHITECTURES:
            if arch in selected:
                raise DiscoveryError(f"image index {name} has more than one linux/{arch} image")
            selected[arch] = digest
    missing = [arch for arch in agent_version.LINUX_ARCHITECTURES if arch not in selected]
    if missing:
        raise DiscoveryError(
            f"image index {name} has no linux/{' or linux/'.join(missing)} image; it has {describe_platforms(entries)}"
        )
    return selected


def select_windows(index, name):
    """Return `(digest, os_version)` for the index's amd64 ltsc2022 child."""
    entries = children(index, name)
    matches = [
        (digest, platform.get("os.version"))
        for digest, platform in entries
        if platform.get("os") == "windows"
        and platform.get("architecture") == "amd64"
        and str(platform.get("os.version", "")).startswith(agent_version.WINDOWS_LTSC2022_OS_PREFIX)
    ]
    if not matches:
        raise DiscoveryError(
            f"image index {name} has no windows/amd64 image with OS version "
            f"{agent_version.WINDOWS_LTSC2022_OS_PREFIX}* (ltsc2022); it has {describe_platforms(entries)}"
        )
    if len(matches) > 1:
        raise DiscoveryError(f"image index {name} has more than one windows/amd64 ltsc2022 image")
    return matches[0]


def labels(config):
    container_config = config.get("config")
    found = container_config.get("Labels") if isinstance(container_config, dict) else None
    return found if isinstance(found, dict) else {}


def normalize_created(created):
    """Convert an image config's `created` timestamp to whole UTC seconds."""
    pattern = r"(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2})(?:\.\d+)?(Z|[+-]\d{2}:\d{2})"
    match = re.fullmatch(pattern, created) if isinstance(created, str) else None
    if not match:
        raise DiscoveryError(f"image config has a malformed created timestamp {created!r}")
    offset = "+00:00" if match.group(2) == "Z" else match.group(2)
    moment = datetime.datetime.fromisoformat(match.group(1) + offset).astimezone(datetime.timezone.utc)
    return moment.strftime("%Y-%m-%dT%H:%M:%SZ")


def resolve_windows_child(registry, digest, source_index_digest, os_version=None):
    """Return the Windows section and labels for an ltsc2022 child digest."""
    resolved = registry.manifest(digest)
    if resolved is None:
        raise DiscoveryError(f"Windows image {registry.reference(digest)} does not exist")
    if is_index(resolved[1]):
        raise DiscoveryError(f"{registry.reference(digest)} is an index; supply its amd64 ltsc2022 child digest")
    config = registry.config(resolved[1], digest)
    if config.get("os") != "windows" or config.get("architecture") != "amd64":
        raise DiscoveryError(f"{registry.reference(digest)} is not a windows/amd64 image")
    os_version = os_version or config.get("os.version")
    if not str(os_version).startswith(agent_version.WINDOWS_LTSC2022_OS_PREFIX):
        raise DiscoveryError(f"{registry.reference(digest)} is not an ltsc2022 image (OS {os_version})")
    section = {
        "image": registry.reference(digest), "source_index_digest": source_index_digest, "os_version": os_version,
    }
    return section, labels(config)


def resolve_linux(registry, reference, version, revision, missing=Skip):
    """Return the Linux section and built_at of an index whose children both carry `version` and `revision`.

    A missing image raises `missing`, with an explanation.
    """
    resolved = registry.manifest(reference)
    if resolved is None:
        raise missing(f"Linux image {registry.repository}:{reference} does not exist")
    index_digest, index = resolved
    if not is_index(index):
        raise DiscoveryError(f"{registry.repository}:{reference} is not a multi-platform image index")
    platform_digests = select_linux(index, f"{registry.repository}:{reference}")
    built_at = None
    for arch, digest in platform_digests.items():
        child = registry.manifest(digest)
        if child is None:
            raise DiscoveryError(f"Linux {arch} image {digest} is listed in its index but does not exist")
        config = registry.config(child[1], digest)
        found = labels(config)
        if found.get(VERSION_LABEL) != version or found.get(REVISION_LABEL) != revision:
            raise Skip(
                f"Linux {arch} image is {found.get(VERSION_LABEL)} at {found.get(REVISION_LABEL)}, "
                f"not {version} at {revision}"
            )
        if arch == "amd64":
            built_at = normalize_created(config.get("created"))
    section = {"image": registry.reference(index_digest), "platform_digests": platform_digests}
    return section, built_at


def planned_packages(kind, version):
    """Return `(section, arch, url)` for each package of an Agent version."""
    return [
        ("macos", arch, agent_version.macos_package_url(kind, version, arch))
        for arch in agent_version.MACOS_ARCHITECTURES
    ] + [
        ("antithesis_debug_debs", arch, agent_version.debug_deb_url(kind, version, arch))
        for arch in agent_version.LINUX_ARCHITECTURES
    ]


def check_packages_published(http, kind, version):
    """Raise Skip unless every package of an Agent version is published, checking each with a HEAD request."""
    for _, _, url in planned_packages(kind, version):
        status, _, _ = http.request("HEAD", url)
        if status in (403, 404):
            raise Skip(f"package {url} is not published")
        if status != 200:
            raise DiscoveryError(f"HEAD {url} returned HTTP {status}")


def hash_packages(http, kind, version):
    """Return the macOS and debug-package sections, after a HEAD check and a full download of each file."""
    check_packages_published(http, kind, version)
    sections = {"macos": {}, "antithesis_debug_debs": {}}
    for section, arch, url in planned_packages(kind, version):
        print(f"[*] Downloading and hashing {url}...")

        def consume(chunks):
            digest = hashlib.sha256()
            for chunk in chunks:
                digest.update(chunk)
            return digest.hexdigest()

        status, _, sha256 = http.request("GET", url, consume=consume)
        if status in (403, 404):
            raise Skip(f"package {url} disappeared during discovery")
        if status != 200:
            raise DiscoveryError(f"GET {url} returned HTTP {status}")
        sections[section][arch] = {"url": url, "sha256": sha256}
    return sections


class GitHub:
    """Read-only client for the Agent repository's tags and commit history."""

    def __init__(self, http, token=None):
        self.http = http
        self.headers = {"Accept": "application/vnd.github+json", "X-GitHub-Api-Version": "2022-11-28"}
        if token:
            self.headers["Authorization"] = f"Bearer {token}"

    def get(self, path):
        return self.http.json(f"https://api.github.com/repos/{AGENT_REPOSITORY}/{path}", self.headers)

    def is_at_or_ahead(self, base, head):
        """Return whether commit `head` equals `base` or descends from it."""
        status = self.get(f"compare/{base}...{head}").get("status")
        if status not in ("ahead", "behind", "diverged", "identical"):
            raise DiscoveryError(f"GitHub compare {base}...{head} returned status {status!r}")
        return status in ("ahead", "identical")

    def release_tags(self, target):
        """Return `{version: ref object}` for exact `X.Y.Z` and `X.Y.Z-rc.N` tags in the target minor."""
        refs = self.get(f"git/matching-refs/tags/{target}.")
        if not isinstance(refs, list):
            raise DiscoveryError("GitHub returned malformed tag refs")
        tags = {}
        for ref in refs:
            name = ref.get("ref", "").removeprefix("refs/tags/") if isinstance(ref, dict) else ""
            parsed = agent_version.parse_agent_version(name)
            if parsed and parsed[0] in ("rc", "release") and agent_version.version_minor(name) == target:
                tags[name] = ref.get("object", {})
        return tags

    def peel(self, ref_object):
        """Return the commit a tag ref points at, following annotated tags."""
        for _ in range(5):
            kind, sha = ref_object.get("type"), ref_object.get("sha")
            if kind == "commit" and isinstance(sha, str) and agent_version.REVISION_RE.fullmatch(sha):
                return sha
            if kind != "tag":
                raise DiscoveryError(f"tag object {sha} points at a {kind}, not a commit")
            ref_object = self.get(f"git/tags/{sha}").get("object", {})
        raise DiscoveryError("tag chain is too deep")


def release_key(version):
    """Order RC and stable versions numerically, with a stable release above its RCs."""
    match = agent_version.RC_VERSION_RE.fullmatch(version) or agent_version.RELEASE_VERSION_RE.fullmatch(version)
    numbers = tuple(int(group) for group in match.groups()[:3])
    rc = int(match.group(4)) if match.re is agent_version.RC_VERSION_RE else None
    return numbers + ((0, rc) if rc is not None else (1, 0))


def eligible_versions(versions, current_kind, current_version):
    """Return newer versions the line may move to, newest first.

    A nightly is the line's starting point, so every RC or stable release ranks above it. Patch RCs are excluded, and
    a line already on stable moves only to newer stable releases.
    """
    eligible = []
    for version in versions:
        kind, _, _, patch = agent_version.parse_agent_version(version)
        if kind == "rc" and patch > 0 and not ACCEPT_PATCH_RCS:
            continue
        if current_kind == "release" and kind == "rc":
            continue
        if current_kind != "nightly" and release_key(version) <= release_key(current_version):
            continue
        eligible.append(version)
    return sorted(eligible, key=release_key, reverse=True)


def discover_nightly(hub, http, github, merged, windows_digest=None):
    """Return a new nightly version file, or raise Skip when no newer complete set exists."""
    if windows_digest:
        if not agent_version.DIGEST_RE.fullmatch(windows_digest):
            raise DiscoveryError(f"--windows-digest {windows_digest!r} is not a sha256 digest")
        windows, windows_labels = resolve_windows_child(hub, windows_digest, None)
    else:
        resolved = hub.manifest(WINDOWS_NIGHTLY_TAG)
        if resolved is None:
            raise DiscoveryError(f"{NIGHTLY_REPOSITORY}:{WINDOWS_NIGHTLY_TAG} does not exist")
        index_digest, index = resolved
        if not is_index(index):
            raise DiscoveryError(f"{NIGHTLY_REPOSITORY}:{WINDOWS_NIGHTLY_TAG} is not an image index")
        child_digest, os_version = select_windows(index, f"{NIGHTLY_REPOSITORY}:{WINDOWS_NIGHTLY_TAG}")
        windows, windows_labels = resolve_windows_child(hub, child_digest, index_digest, os_version)

    version = windows_labels.get(VERSION_LABEL)
    revision = windows_labels.get(REVISION_LABEL)
    match = agent_version.NIGHTLY_VERSION_RE.fullmatch(version or "")
    if not match:
        if agent_version.parse_agent_version(version) is not None:
            # Release-branch pipelines sometimes push the same floating tag. The next main nightly replaces it.
            raise Skip(f"the Windows image is {version}, an Agent release build, not an Agent main nightly")
        raise DiscoveryError(
            f"the Windows image {windows['image']} has {VERSION_LABEL} {version!r}, which is not an Agent version this "
            "tool understands. A main nightly looks like 7.87.0-devel+git.394.462b537.pipeline.151347634. If the "
            "Agent changed its version format, update NIGHTLY_VERSION_RE in ci/tooling/agent_version.py."
        )
    if not isinstance(revision, str) or not agent_version.REVISION_RE.fullmatch(revision):
        raise DiscoveryError(f"the Windows image has a malformed revision label {revision!r}")
    pipeline_id = int(match.group(6))
    print(f"[*] Windows ltsc2022 image {windows['image']} is {version} at {revision}")

    merged_integration = merged["integration"] if merged else None
    if merged_integration and merged_integration["kind"] == "nightly":
        if pipeline_id <= merged_integration["pipeline_id"]:
            current = merged_integration["pipeline_id"]
            raise Skip(f"pipeline {pipeline_id} is not newer than the current pipeline {current}")

    # Release-branch pipelines can push the floating Windows tag with a nightly version too.
    if not github.is_at_or_ahead(revision, "main"):
        raise Skip(f"Agent commit {revision} is not on Agent main; the Windows image is likely a release-branch build")
    if merged_integration and merged_integration["git_revision"] != revision:
        if not github.is_at_or_ahead(merged_integration["git_revision"], revision):
            current = merged_integration["git_revision"]
            raise Skip(f"Agent commit {revision} is not ahead of the current commit {current}")

    # The packages are published only after the whole upstream pipeline passes, and the Linux images well before that.
    # So once the packages exist, a missing Linux image means its tag name has changed.
    check_packages_published(http, "nightly", version)
    linux_tag = LINUX_NIGHTLY_TAG.format(sha8=revision[:8])

    def linux_missing(message):
        return DiscoveryError(
            f"{message}, but the packages for {version} are published, so the Linux image should be. If the Agent "
            f"renamed its Linux nightly tags, update LINUX_NIGHTLY_TAG in {Path(__file__).name}."
        )

    linux, built_at = resolve_linux(hub, linux_tag, version, revision, missing=linux_missing)

    integration = {
        "kind": "nightly",
        "version": version,
        "git_revision": revision,
        "pipeline_id": pipeline_id,
        "built_at": built_at,
        "linux": linux,
        "windows": windows,
        **hash_packages(http, "nightly", version),
    }
    return finish_version_file(merged, integration)


def discover_release(registry, http, github, merged):
    """Return the newest complete eligible release version file, or raise Skip."""
    if merged is None or "target" not in merged:
        raise DiscoveryError("the release updater needs a version file with a target")
    target = merged["target"]
    current = merged["integration"]
    if current["kind"] != "nightly" and agent_version.version_minor(current["version"]) != target:
        raise DiscoveryError(f"the current {current['version']} is outside target {target}")
    tags = github.release_tags(target)
    candidates = eligible_versions(tags, current["kind"], current["version"])
    if not candidates:
        raise Skip(f"no Agent {target} release is newer than {current['version']}")

    reasons = []
    for version in candidates:
        try:
            revision = github.peel(tags[version])
            linux, built_at = resolve_linux(registry, LINUX_RELEASE_TAG.format(version=version), version, revision)
            windows = resolve_release_windows(registry, version, revision)
            kind = "rc" if agent_version.RC_VERSION_RE.fullmatch(version) else "release"
            integration = {
                "kind": kind,
                "version": version,
                "git_revision": revision,
                "pipeline_id": None,
                "built_at": built_at,
                "linux": linux,
                "windows": windows,
                **hash_packages(http, kind, version),
            }
            return finish_version_file(merged, integration)
        except Skip as skip:
            print(f"[*] Agent {version} is incomplete: {skip}")
            reasons.append(f"{version}: {skip}")
    raise Skip("no newer complete release; " + "; ".join(reasons))


def resolve_release_windows(registry, version, revision):
    reference = WINDOWS_RELEASE_TAG.format(version=version)
    resolved = registry.manifest(reference)
    if resolved is None:
        raise Skip(f"Windows image {registry.repository}:{reference} does not exist")
    digest, manifest = resolved
    if is_index(manifest):
        child_digest, os_version = select_windows(manifest, f"{registry.repository}:{reference}")
        windows, found = resolve_windows_child(registry, child_digest, digest, os_version)
    else:
        windows, found = resolve_windows_child(registry, digest, None)
    if found.get(VERSION_LABEL) != version or found.get(REVISION_LABEL) != revision:
        raise Skip(f"Windows image is {found.get(VERSION_LABEL)} at {found.get(REVISION_LABEL)}")
    return windows


def finish_version_file(merged, integration):
    version_file = {"schema_version": agent_version.SCHEMA_VERSION}
    if merged and "target" in merged:
        version_file["target"] = merged["target"]
    version_file["integration"] = integration
    errors = agent_version.validate_version_file(version_file)
    if errors:
        raise DiscoveryError("discovered version file is invalid: " + "; ".join(errors))
    return version_file


def describe(version_file, merged, base_branch):
    """Return the pull request title and body for a discovered version file."""
    new = version_file["integration"]
    old = merged["integration"] if merged else None
    if new["kind"] == "nightly":
        title = f"chore(ci): update nightly Datadog Agent to {new['version']}"
    else:
        title = f"chore(ci): update Datadog Agent to {new['version']} on {base_branch}"

    lines = [
        "## Summary",
        "",
        f"Updates the Datadog Agent that Saluki builds with and tests against on `{base_branch}`.",
        "",
        f"- **Current:** `{old['version']}`, built at {old['built_at']}" if old else "- **Current:** none",
        f"- **New:** `{new['version']}`, built at {new['built_at']}",
    ]
    if old and old["git_revision"] != new["git_revision"]:
        lines.append(
            f"- **Agent changes:** "
            f"https://github.com/{AGENT_REPOSITORY}/compare/{old['git_revision']}...{new['git_revision']}"
        )
    else:
        lines.append(f"- **Agent commit:** https://github.com/{AGENT_REPOSITORY}/commit/{new['git_revision']}")
    if new["pipeline_id"] is not None:
        lines.append(f"- **Upstream pipeline:** {UPSTREAM_PIPELINE_URL.format(new['pipeline_id'])}")
    if "target" in version_file:
        lines.append(f"- **Target:** `{version_file['target']}` (unchanged)")
    lines += [
        "",
        "### Artifacts",
        "",
        f"- Linux: `{new['linux']['image']}` (amd64 `{new['linux']['platform_digests']['amd64']}`, "
        f"arm64 `{new['linux']['platform_digests']['arm64']}`)",
        f"- Windows ltsc2022 {new['windows']['os_version']}: `{new['windows']['image']}`",
    ]
    lines += [f"- macOS {arch}: {new['macos'][arch]['url']}" for arch in agent_version.MACOS_ARCHITECTURES]
    lines += [
        f"- Debug package {arch}: {new['antithesis_debug_debs'][arch]['url']}"
        for arch in agent_version.LINUX_ARCHITECTURES
    ]
    lines += [
        "",
        f"The SHA-256 checksums are in `{agent_version.VERSION_FILE}`. CI on this pull request tests ADP against these "
        + "artifacts.",
        "",
        "If this Agent needs an accompanying ADP change, push it to this branch. The automation then keeps this "
        + "candidate and stops proposing newer ones until this pull request merges or closes.",
        "",
        "_This PR was automatically generated by the Agent version update workflow._",
        "",
        "## Change Type",
        "",
        "- [ ] Bug fix",
        "- [ ] New feature",
        "- [x] Non-functional (chore, refactoring, docs)",
        "- [ ] Performance",
    ]
    return title, "\n".join(lines) + "\n"


def write_outcome(output_dir, summary, version_file=None, title=None, body=None, merged_text=None):
    output_dir.mkdir(parents=True, exist_ok=True)
    (output_dir / "summary.json").write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")
    if version_file is not None:
        text = json.dumps(version_file, indent=2) + "\n"
        (output_dir / "datadog-agent-version.json").write_text(text, encoding="utf-8")
        (output_dir / "pr-title.txt").write_text(title + "\n", encoding="utf-8")
        (output_dir / "pr-body.md").write_text(body, encoding="utf-8")
    if merged_text is not None:
        (output_dir / "merged-datadog-agent-version.json").write_text(merged_text, encoding="utf-8")
    github_output = os.environ.get("GITHUB_OUTPUT")
    if github_output:
        with open(github_output, "a", encoding="utf-8") as file:
            file.write(f"status={summary['status']}\n")


def load_merged_version_file(path, target=None):
    """Load the version file being updated, or `None` when there is none.

    With `target`, the version file is a release cut in progress: it may lack `target`, and its nightly may come from
    another minor, since the first complete RC of the target replaces it.
    """
    if not Path(path).exists():
        return None
    if target is None:
        return agent_version.load_version_file(path)
    version_file = agent_version.parse_version_file_text(Path(path).read_text(encoding="utf-8"), path)
    if not isinstance(version_file, dict) or version_file.get("target", target) != target:
        raise agent_version.VersionFileError(f"{path} already declares a different target")
    untargeted = {key: value for key, value in version_file.items() if key != "target"}
    errors = agent_version.validate_version_file(untargeted)
    if errors:
        raise agent_version.VersionFileError("\n".join(f"{path}: {error}" for error in errors))
    return {**untargeted, "target": target}


def run(arguments, http=None):
    """Discover a candidate and write the outcome. Returns the process exit code."""
    try:
        return discover(arguments, http)
    except (DiscoveryError, agent_version.VersionFileError) as error:
        report_failure(arguments.output_dir, error)
        return 1


def report_failure(output_dir, error):
    """Explain a failed discovery on stderr, in the GitHub Actions run summary, and in `summary.json`."""
    message = f"{error}\nThe Agent version file was not changed. See {FAILURE_DOCS}."
    print(f"error: {message}", file=sys.stderr)
    if os.environ.get("GITHUB_ACTIONS") == "true":
        # A workflow command is one line; GitHub decodes `%0A` back into a newline.
        encoded = message.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")
        print(f"::error title=Agent version discovery failed::{encoded}")
    write_outcome(output_dir, {"status": "error", "reason": str(error)})


def discover(arguments, http=None):
    http = http or Http(urllib_transport, arguments.budget_seconds)
    merged = load_merged_version_file(arguments.merged_file, arguments.target)
    github = GitHub(http, os.environ.get("GITHUB_TOKEN"))
    current = merged["integration"]["version"] if merged else None
    try:
        if arguments.mode == "nightly":
            version_file = discover_nightly(
                Registry(http, DOCKER_HUB, NIGHTLY_REPOSITORY), http, github, merged, arguments.windows_digest
            )
        else:
            version_file = discover_release(Registry(http, DATADOG_REGISTRY, RELEASE_REPOSITORY), http, github, merged)
    except Skip as skip:
        print(f"[*] Keeping {current}: {skip}")
        write_outcome(arguments.output_dir, {"status": "skipped", "reason": str(skip), "current_version": current})
        return 0

    title, body = describe(version_file, merged, arguments.base_branch)
    new = version_file["integration"]["version"]
    print(f"[*] Found a complete Agent set: {new}")
    summary = {"status": "candidate", "current_version": current, "candidate_version": new}
    merged_path = Path(arguments.merged_file)
    merged_text = merged_path.read_text(encoding="utf-8") if merged_path.exists() else None
    write_outcome(arguments.output_dir, summary, version_file, title, body, merged_text)
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("mode", choices=("nightly", "release"))
    parser.add_argument(
        "--merged-file", default=agent_version.VERSION_FILE, help="Version file on the branch being updated"
    )
    parser.add_argument(
        "--base-branch", default="main", help="Branch the version file is proposed for, used in the PR body"
    )
    parser.add_argument("--windows-digest", help="ltsc2022 child digest from an upstream build log (nightly only)")
    parser.add_argument("--target", help="Target minor for a release cut, when the version file doesn't declare it yet")
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--budget-seconds", type=int, default=900, help="Total time allowed for retries")
    arguments = parser.parse_args(argv)
    if arguments.windows_digest and arguments.mode != "nightly":
        parser.error("--windows-digest only applies to nightly discovery")
    if arguments.target and (arguments.mode != "release" or not agent_version.TARGET_RE.fullmatch(arguments.target)):
        parser.error("--target takes a major.minor such as 7.87, for release discovery")
    return run(arguments)


if __name__ == "__main__":
    raise SystemExit(main())
