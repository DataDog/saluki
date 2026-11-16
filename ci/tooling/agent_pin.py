#!/usr/bin/env python3
"""Validate and read `.agent-pin.json`, the Datadog Agent that Saluki builds with and tests against.

The pin holds the Integration version: one Agent build's Linux, Windows, macOS and debug-package artifacts. On a
release branch it also holds `target`, the Agent minor the ADP line supports. This module only reads local files and
Git objects; discovery, which reads registries and hashes packages, lives in `discover_agent_pin.py`.
"""

import argparse
import datetime
import json
import re
import subprocess
import sys
from pathlib import Path


PIN_PATH = ".agent-pin.json"
SCHEMA_VERSION = 1
ADP_CARGO_TOML = "bin/agent-data-plane/Cargo.toml"

NIGHTLY_REPOSITORY = "datadog/agent-dev"
RELEASE_REPOSITORY = "registry.datadoghq.com/agent"
WINDOWS_LTSC2022_OS_PREFIX = "10.0.20348."

NUMBER = r"(?:0|[1-9][0-9]*)"
NIGHTLY_VERSION_RE = re.compile(
    rf"^({NUMBER})\.({NUMBER})\.({NUMBER})-devel\+git\.({NUMBER})\.([0-9a-f]{{7,40}})\.pipeline\.([1-9][0-9]*)$"
)
RC_VERSION_RE = re.compile(rf"^({NUMBER})\.({NUMBER})\.({NUMBER})-rc\.([1-9][0-9]*)$")
RELEASE_VERSION_RE = re.compile(rf"^({NUMBER})\.({NUMBER})\.({NUMBER})$")
TARGET_RE = re.compile(rf"^({NUMBER})\.({NUMBER})$")
RELEASE_BRANCH_RE = re.compile(rf"^releases/({NUMBER})\.({NUMBER})\.x$")
ADP_TAG_RE = re.compile(rf"^({NUMBER})\.({NUMBER})\.({NUMBER})$")
REVISION_RE = re.compile(r"^[0-9a-f]{40}$")
DIGEST_RE = re.compile(r"^sha256:[0-9a-f]{64}$")
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
BUILT_AT_RE = re.compile(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$")
WINDOWS_OS_VERSION_RE = re.compile(rf"^10\.0\.20348\.{NUMBER}$")

MACOS_ARCHITECTURES = ("arm64", "x86_64")
LINUX_ARCHITECTURES = ("amd64", "arm64")
KINDS = ("nightly", "rc", "release")


class PinError(Exception):
    """A pin that is missing, unreadable or invalid."""


def parse_agent_version(version):
    """Return `(kind, major, minor, patch)` for a nightly, RC or stable Agent version, or `None`."""
    for kind, pattern in (("nightly", NIGHTLY_VERSION_RE), ("rc", RC_VERSION_RE), ("release", RELEASE_VERSION_RE)):
        match = pattern.fullmatch(version) if isinstance(version, str) else None
        if match:
            return kind, int(match.group(1)), int(match.group(2)), int(match.group(3))
    return None


def version_minor(version):
    """Return the `major.minor` of an Agent version string, or `None`."""
    parsed = parse_agent_version(version)
    return f"{parsed[1]}.{parsed[2]}" if parsed else None


def target_for_adp_line(adp_major, adp_minor):
    """Map an ADP line to its Agent target minor: ADP X.Y supports Agent (X + 6).(Y + 78)."""
    return f"{adp_major + 6}.{adp_minor + 78}"


def macos_package_url(kind, version, arch):
    """Return the long-lived macOS DMG URL for an Agent version."""
    if kind == "nightly":
        package_version = version.replace("+", ".")
        return f"https://s3.amazonaws.com/dd-agent-macostesting/nightly/datadog-agent-{package_version}-1.{arch}.dmg"
    if kind == "rc":
        return f"https://s3.amazonaws.com/dd-agent-macostesting/beta/datadog-agent-{version}-1.{arch}.dmg"
    return f"https://s3.amazonaws.com/dd-agent/datadog-agent-{version}-1.{arch}.dmg"


def debug_deb_url(kind, version, arch):
    """Return the Agent debug-symbol package URL for an Agent version."""
    if kind == "nightly":
        deb_version = version.replace("+", ".").replace("-devel", "~devel")
        host = "apt.datad0g.com"
    elif kind == "rc":
        deb_version = version.replace("-rc.", "~rc.")
        host = "apt.datad0g.com"
    else:
        deb_version = version
        host = "apt.datadoghq.com"
    return f"https://{host}/pool/d/da/datadog-agent-dbg_{deb_version}-1_{arch}.deb"


def image_repository(kind):
    """Return the image repository that publishes Agent builds of a kind."""
    return NIGHTLY_REPOSITORY if kind == "nightly" else RELEASE_REPOSITORY


def check_keys(value, path, required, optional=()):
    """Return errors for a mapping whose keys differ from the expected ones."""
    if not isinstance(value, dict):
        return [f"{path} must be an object"]
    errors = [f"{path}.{key} is required" for key in required if key not in value]
    errors.extend(f"{path}.{key} is not a known field" for key in value if key not in required and key not in optional)
    return errors


def check_image(value, path, repository):
    """Return errors for an image reference that is not `<repository>@sha256:<digest>`."""
    if not isinstance(value, str) or "@" not in value:
        return [f"{path} must be a digest reference, {repository}@sha256:<64 hex characters>"]
    name, digest = value.split("@", 1)
    errors = []
    if name != repository:
        errors.append(f"{path} must come from {repository}, not {name}")
    if not DIGEST_RE.fullmatch(digest):
        errors.append(f"{path} has a malformed digest {digest!r}")
    return errors


def check_package(value, path, expected_url):
    """Return errors for a package entry whose URL or checksum is wrong."""
    errors = check_keys(value, path, ("url", "sha256"))
    if errors:
        return errors
    url = value["url"]
    if isinstance(url, str) and "/dd-agent-macostesting/ci/" in url:
        errors.append(f"{path}.url uses the expiring macOS ci/ prefix; pin the long-lived package instead")
    elif url != expected_url:
        errors.append(f"{path}.url must be {expected_url}, not {url!r}")
    if not isinstance(value["sha256"], str) or not SHA256_RE.fullmatch(value["sha256"]):
        errors.append(f"{path}.sha256 must be 64 lowercase hexadecimal characters")
    return errors


def validate_integration(integration):
    """Return errors for the `integration` section."""
    keys = (
        "kind", "version", "revision", "pipeline_id", "built_at", "linux", "windows", "macos", "antithesis_debug_debs",
    )
    errors = check_keys(integration, "integration", keys)
    if errors:
        return errors

    kind = integration["kind"]
    version = integration["version"]
    if kind not in KINDS:
        return [f"integration.kind must be one of {', '.join(KINDS)}, not {kind!r}"]
    parsed = parse_agent_version(version)
    if parsed is None or parsed[0] != kind:
        return [f"integration.version {version!r} is not a {kind} Agent version"]

    revision = integration["revision"]
    if not isinstance(revision, str) or not REVISION_RE.fullmatch(revision):
        errors.append("integration.revision must be a full 40-character Agent commit SHA")

    pipeline_id = integration["pipeline_id"]
    if kind == "nightly":
        match = NIGHTLY_VERSION_RE.fullmatch(version)
        if not isinstance(pipeline_id, int) or isinstance(pipeline_id, bool) or pipeline_id <= 0:
            errors.append("integration.pipeline_id must be a positive integer for a nightly")
        elif str(pipeline_id) != match.group(6):
            errors.append(f"integration.pipeline_id {pipeline_id} does not match version pipeline {match.group(6)}")
        if isinstance(revision, str) and not revision.startswith(match.group(5)):
            errors.append(f"integration.revision does not start with the version's commit {match.group(5)}")
    elif pipeline_id is not None:
        errors.append(f"integration.pipeline_id must be null for kind {kind}")

    built_at = integration["built_at"]
    if not isinstance(built_at, str) or not BUILT_AT_RE.fullmatch(built_at):
        errors.append("integration.built_at must be a UTC timestamp such as 2026-11-16T03:40:00Z")
    else:
        try:
            parse_built_at(built_at)
        except ValueError:
            errors.append(f"integration.built_at {built_at!r} is not a valid date and time")

    repository = image_repository(kind)

    linux = integration["linux"]
    linux_errors = check_keys(linux, "integration.linux", ("image", "platform_digests"))
    if not linux_errors:
        linux_errors.extend(check_image(linux["image"], "integration.linux.image", repository))
        digests = linux["platform_digests"]
        linux_errors.extend(check_keys(digests, "integration.linux.platform_digests", LINUX_ARCHITECTURES))
        if isinstance(digests, dict):
            linux_errors.extend(
                f"integration.linux.platform_digests.{arch} has a malformed digest"
                for arch in LINUX_ARCHITECTURES
                if arch in digests and not (isinstance(digests[arch], str) and DIGEST_RE.fullmatch(digests[arch]))
            )
    errors.extend(linux_errors)

    windows = integration["windows"]
    windows_errors = check_keys(windows, "integration.windows", ("image", "source_index_digest", "os_version"))
    if not windows_errors:
        windows_errors.extend(check_image(windows["image"], "integration.windows.image", repository))
        source = windows["source_index_digest"]
        if source is not None and not (isinstance(source, str) and DIGEST_RE.fullmatch(source)):
            windows_errors.append("integration.windows.source_index_digest must be null or a sha256 digest")
        if not isinstance(windows["os_version"], str) or not WINDOWS_OS_VERSION_RE.fullmatch(windows["os_version"]):
            windows_errors.append("integration.windows.os_version must be an ltsc2022 version, 10.0.20348.<build>")
    errors.extend(windows_errors)

    for section, architectures, url_for in (
        ("macos", MACOS_ARCHITECTURES, macos_package_url),
        ("antithesis_debug_debs", LINUX_ARCHITECTURES, debug_deb_url),
    ):
        packages = integration[section]
        section_errors = check_keys(packages, f"integration.{section}", architectures)
        if not section_errors:
            for arch in architectures:
                section_errors.extend(
                    check_package(packages[arch], f"integration.{section}.{arch}", url_for(kind, version, arch))
                )
        errors.extend(section_errors)

    return errors


def validate_pin(pin):
    """Return every error in a pin's shape and values, independent of its branch."""
    errors = check_keys(pin, "pin", ("schema_version", "integration"), ("target",))
    if errors:
        return errors
    if pin["schema_version"] != SCHEMA_VERSION:
        return [f"schema_version {pin['schema_version']!r} is not supported; this reader understands {SCHEMA_VERSION}"]

    errors = validate_integration(pin["integration"])
    if "target" in pin:
        target = pin["target"]
        if not isinstance(target, str) or not TARGET_RE.fullmatch(target):
            errors.append(f"target {target!r} must be an Agent major.minor such as 7.87")
        else:
            minor = version_minor(pin["integration"].get("version"))
            if minor is not None and minor != target:
                errors.append(f"integration.version {pin['integration']['version']} is outside target {target}")
    return errors


def parse_built_at(value):
    """Parse a pin's `built_at` timestamp as an aware UTC datetime."""
    return datetime.datetime.strptime(value, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=datetime.timezone.utc)


def declared_version(pin):
    """Return the `DD_AGENT_VERSION` that the Linux ADP build compiles in."""
    return f"{pin['target']}.0" if "target" in pin else pin["integration"]["version"]


def read_field(pin, field):
    """Return the scalar at a dotted path, such as `integration.linux.image`."""
    value = pin
    for part in field.split("."):
        if not isinstance(value, dict) or part not in value:
            raise PinError(f"{PIN_PATH} has no field {field}")
        value = value[part]
    if value is None or isinstance(value, (dict, list)):
        raise PinError(f"{PIN_PATH} field {field} is not a scalar value")
    return value


def parse_pin_text(text, source):
    """Parse pin JSON, raising `PinError` with the source on failure."""
    try:
        return json.loads(text)
    except json.JSONDecodeError as error:
        raise PinError(f"{source} is not valid JSON: {error}") from error


def load_pin(path):
    """Read and validate a pin file."""
    try:
        text = Path(path).read_text(encoding="utf-8")
    except FileNotFoundError as error:
        raise PinError(f"{path} does not exist") from error
    pin = parse_pin_text(text, path)
    errors = validate_pin(pin)
    if errors:
        raise PinError("\n".join(f"{path}: {error}" for error in errors))
    return pin


def check_change(destination, pin, established):
    """Return errors for a proposed pin against its destination branch.

    `pin` is the proposed file and `established` is the file on the destination branch's tip; either is `None` when
    the file is absent. Rules come from the destination, so a change cannot exempt itself by deleting the pin or
    `target`.
    """
    errors = validate_pin(pin) if pin is not None else []
    proposed_target = pin.get("target") if isinstance(pin, dict) else None
    established_target = established.get("target") if isinstance(established, dict) else None
    line = RELEASE_BRANCH_RE.fullmatch(destination)

    if destination == "main":
        if pin is None:
            errors.append(f"main requires {PIN_PATH}")
        elif isinstance(pin, dict):
            if "target" in pin:
                errors.append("main must not declare target; only release branches do")
            if isinstance(pin.get("integration"), dict) and pin["integration"].get("kind") != "nightly":
                errors.append("main must pin a nightly")
    elif line:
        # A line adopts the pin once its branch has one. Older lines keep their process until a migration adds it.
        if established is None and pin is None:
            return errors
        if pin is None:
            errors.append(f"{destination} has adopted {PIN_PATH}; it cannot be removed")
        expected_target = target_for_adp_line(int(line.group(1)), int(line.group(2)))
        if established_target is not None and proposed_target != established_target:
            errors.append(f"target is immutable on {destination}: it must stay {established_target!r}")
        elif pin is not None and proposed_target is None:
            errors.append(f"{destination} requires target {expected_target!r}")
        if proposed_target is not None and proposed_target != expected_target:
            errors.append(f"target {proposed_target!r} does not match {destination}; it must be {expected_target!r}")
    elif proposed_target != established_target:
        errors.append(f"target must not change on a pull request into {destination}")
    return errors


def check_release_tag(tag, pin, adp_version, established, on_branch):
    """Return `(adopted, errors)` for publishing ADP release `tag`.

    `pin` and `adp_version` come from the tagged commit, `established` from the tip of the line's release branch, and
    `on_branch` says whether that branch contains the tag. Lines with no pin in either place have not adopted it.
    """
    match = ADP_TAG_RE.fullmatch(tag)
    if not match:
        return True, [f"release tag {tag!r} must use the X.Y.Z format"]
    branch = f"releases/{match.group(1)}.{match.group(2)}.x"
    if pin is None and established is None:
        return False, []

    errors = []
    if not on_branch:
        errors.append(f"tag {tag} is not on {branch}; cut the branch, commit target, then tag the branch")
    if adp_version != tag:
        errors.append(f"{ADP_CARGO_TOML} version {adp_version!r} does not match tag {tag}")
    if pin is None:
        errors.append(f"tag {tag} has no {PIN_PATH}, but {branch} has adopted it")
    else:
        errors.extend(validate_pin(pin))
        expected_target = target_for_adp_line(int(match.group(1)), int(match.group(2)))
        target = pin.get("target") if isinstance(pin, dict) else None
        if target != expected_target:
            errors.append(f"tag {tag} must declare target {expected_target!r}, not {target!r}")
    return True, errors


def git(repository, *arguments):
    """Run a Git command and return its completed process."""
    return subprocess.run(["git", "-C", str(repository), *arguments], capture_output=True, text=True)


def read_git_pin(repository, ref):
    """Return the pin at a Git ref, or `None` when the ref has no pin file."""
    if git(repository, "rev-parse", "--verify", "--quiet", f"{ref}^{{commit}}").returncode != 0:
        raise PinError(f"Git ref {ref} does not exist; fetch it before checking")
    if git(repository, "cat-file", "-e", f"{ref}:{PIN_PATH}").returncode != 0:
        return None
    shown = git(repository, "show", f"{ref}:{PIN_PATH}")
    if shown.returncode != 0:
        raise PinError(f"could not read {PIN_PATH} at {ref}: {shown.stderr.strip()}")
    return parse_pin_text(shown.stdout, f"{ref}:{PIN_PATH}")


def read_adp_version(repository):
    """Return the `agent-data-plane` crate version from the working tree."""
    for line in (Path(repository) / ADP_CARGO_TOML).read_text(encoding="utf-8").splitlines():
        match = re.fullmatch(r'version = "([^"]+)"', line.strip())
        if match:
            return match.group(1)
    raise PinError(f"{ADP_CARGO_TOML} has no version")


def read_working_pin(repository):
    """Return the working tree's pin without validating it, or `None` when it is absent."""
    path = Path(repository) / PIN_PATH
    if not path.exists():
        return None
    return parse_pin_text(path.read_text(encoding="utf-8"), str(path))


def report(errors):
    """Print errors and return a process exit code."""
    if errors:
        print("\n".join(f"error: {error}" for error in errors), file=sys.stderr)
        return 1
    return 0


def validate_command(arguments):
    load_pin(arguments.pin)
    print(f"{arguments.pin} is valid")
    return 0


def get_command(arguments):
    print(read_field(load_pin(arguments.pin), arguments.field))
    return 0


def declared_version_command(arguments):
    print(declared_version(load_pin(arguments.pin)))
    return 0


def age_command(arguments):
    pin = load_pin(arguments.pin)
    now = parse_built_at(arguments.now) if arguments.now else datetime.datetime.now(datetime.timezone.utc)
    built_at = pin["integration"]["built_at"]
    days = (now - parse_built_at(built_at)).total_seconds() / 86400
    print(f"Agent pin {pin['integration']['version']} was built at {built_at}: {days:.1f} days old.")
    return 0


def check_change_command(arguments):
    repository = Path(arguments.repository)
    established = read_git_pin(repository, arguments.established_ref) if arguments.established_ref else None
    errors = check_change(arguments.destination, read_working_pin(repository), established)
    if not errors:
        print(f"{PIN_PATH} satisfies the rules for {arguments.destination}")
    return report(errors)


def check_tag_command(arguments):
    repository = Path(arguments.repository)
    match = ADP_TAG_RE.fullmatch(arguments.tag)
    if not match:
        return report([f"release tag {arguments.tag!r} must use the X.Y.Z format"])
    branch = f"releases/{match.group(1)}.{match.group(2)}.x"
    ref = f"refs/remotes/{arguments.remote}/{branch}"

    if arguments.fetch:
        listed = git(repository, "ls-remote", "--exit-code", "--heads", arguments.remote, branch)
        if listed.returncode == 0:
            fetched = git(repository, "fetch", "--no-tags", arguments.remote, f"+refs/heads/{branch}:{ref}")
            if fetched.returncode != 0:
                raise PinError(f"could not fetch {branch}: {fetched.stderr.strip()}")
        elif listed.returncode != 2:
            raise PinError(f"could not list {branch}: {listed.stderr.strip()}")

    branch_exists = git(repository, "rev-parse", "--verify", "--quiet", f"{ref}^{{commit}}").returncode == 0
    established = read_git_pin(repository, ref) if branch_exists else None
    on_branch = branch_exists and git(repository, "merge-base", "--is-ancestor", "HEAD", ref).returncode == 0
    adopted, errors = check_release_tag(
        arguments.tag, read_working_pin(repository), read_adp_version(repository), established, on_branch
    )
    if not adopted:
        if arguments.require_adopted:
            return report([f"{branch} has not adopted {PIN_PATH}; a new release line must declare its target"])
        print(f"{branch} has not adopted {PIN_PATH}; skipping the pin checks for {arguments.tag}")
        return 0
    if not errors:
        print(f"{arguments.tag} is on {branch} and declares the mapped target")
    return report(errors)


def parse_arguments(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    subcommands = parser.add_subparsers(dest="command", required=True)

    def add(name, handler, help_text):
        subparser = subcommands.add_parser(name, help=help_text)
        subparser.set_defaults(handler=handler)
        return subparser

    for name, handler, help_text in (
        ("validate", validate_command, "Validate the pin's shape and values"),
        ("declared-version", declared_version_command, "Print the DD_AGENT_VERSION for the Linux ADP build"),
        ("age", age_command, "Print the pin's age, measured from built_at"),
    ):
        subparser = add(name, handler, help_text)
        subparser.add_argument("--pin", default=PIN_PATH)
        if name == "age":
            subparser.add_argument("--now", help="Measure age at this UTC time instead of now")

    get_parser = add("get", get_command, "Print one field, such as integration.linux.image")
    get_parser.add_argument("field")
    get_parser.add_argument("--pin", default=PIN_PATH)

    change_parser = add("check-change", check_change_command, "Check the working tree's pin against a destination")
    change_parser.add_argument("--destination", required=True, help="Pull request destination branch, such as main")
    change_parser.add_argument("--established-ref", help="Git ref of the destination branch's tip")
    change_parser.add_argument("--repository", default=".")

    tag_parser = add("check-tag", check_tag_command, "Check the checked-out release tag against its release branch")
    tag_parser.add_argument("--tag", required=True)
    tag_parser.add_argument("--remote", default="origin")
    tag_parser.add_argument("--fetch", action="store_true", help="Fetch the release branch from the remote first")
    tag_parser.add_argument("--require-adopted", action="store_true", help="Fail when the line has no pin")
    tag_parser.add_argument("--repository", default=".")
    return parser.parse_args(argv)


def main(argv=None):
    arguments = parse_arguments(argv)
    try:
        return arguments.handler(arguments)
    except PinError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
