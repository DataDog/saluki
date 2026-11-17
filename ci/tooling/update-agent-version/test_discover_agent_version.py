import contextlib
import hashlib
import http.client
import importlib.util
import io
import json
import sys
import tempfile
import unittest
import unittest.mock
from pathlib import Path


HERE = Path(__file__).resolve().parent
# `agent_version` and its tests live one directory up, in `ci/tooling`.
sys.path.insert(0, str(HERE.parent))
spec = importlib.util.spec_from_file_location("discover_agent_version", HERE / "discover_agent_version.py")
subject = importlib.util.module_from_spec(spec)
spec.loader.exec_module(subject)
agent_version = subject.agent_version

from test_agent_version import make_version_file  # noqa: E402

# Test-only commits and versions; none of these identify real Agent builds.
OLD_REVISION = "0123456789abcdef0123456789abcdef01234567"
REVISION = "89abcdef0123456789abcdef0123456789abcdef"
VERSION = "7.87.0-devel+git.50.89abcde.pipeline.99000"
WINDOWS_OS = "10.0.20348.4529"
HUB = "https://registry-1.docker.io/v2/datadog/agent-dev"
DATADOG = "https://registry.datadoghq.com/v2/agent"
GITHUB = "https://api.github.com/repos/DataDog/datadog-agent"


class FakeWeb:
    """An in-memory registry, package bucket and GitHub API."""

    def __init__(self):
        self.routes = {}
        self.requests = []

    def add(self, method, url, *responses):
        self.routes.setdefault((method, url), []).extend(responses)

    def json(self, url, document):
        self.add("GET", url, (200, {}, json.dumps(document).encode()))

    def blob(self, base, document):
        body = json.dumps(document).encode()
        digest = "sha256:" + hashlib.sha256(body).hexdigest()
        self.add("GET", f"{base}/blobs/{digest}", (200, {}, body))
        return digest

    def manifest(self, base, document, *tags):
        body = json.dumps(document).encode()
        digest = "sha256:" + hashlib.sha256(body).hexdigest()
        for reference in (digest, *tags):
            self.add("GET", f"{base}/manifests/{reference}", (200, {"docker-content-digest": digest}, body))
        return digest

    def image(self, base, os, arch, labels, created="2026-11-16T03:40:12.123456789Z", os_version=None):
        config = {"os": os, "architecture": arch, "created": created, "config": {"Labels": labels}}
        if os_version:
            config["os.version"] = os_version
        config_digest = self.blob(base, config)
        return self.manifest(
            base, {"mediaType": "application/vnd.oci.image.manifest.v1+json", "config": {"digest": config_digest}}
        )

    def index(self, base, entries, *tags):
        manifests = [{"digest": digest, "platform": platform} for digest, platform in entries]
        index = {"mediaType": "application/vnd.oci.image.index.v1+json", "manifests": manifests}
        return self.manifest(base, index, *tags)

    def package(self, url, contents):
        self.add("HEAD", url, (200, {}, b""))
        self.add("GET", url, (200, {}, contents))

    def transport(self, method, url, headers):
        self.requests.append((method, url))
        responses = self.routes.get((method, url))
        if not responses:
            return 404, {}, iter([b""])
        response = responses[0] if len(responses) == 1 else responses.pop(0)
        if isinstance(response, Exception):
            raise response
        status, response_headers, body = response
        return status, response_headers, body() if callable(body) else iter([body])


def labels(version, revision):
    return {subject.VERSION_LABEL: version, subject.REVISION_LABEL: revision}


def publish_nightly(web, version=VERSION, revision=REVISION, linux_version=None, skip=(), on_main=True):
    """Publish a nightly's Windows, Linux and package artifacts; return the Windows child digest."""
    web.json(f"{GITHUB}/compare/{revision}...main", {"status": "ahead" if on_main else "diverged"})
    windows = web.image(HUB, "windows", "amd64", labels(version, revision), os_version=WINDOWS_OS)
    nanoserver = web.image(HUB, "windows", "amd64", labels(version, revision), os_version="10.0.17763.1")
    web.index(
        HUB,
        [
            (nanoserver, {"os": "windows", "architecture": "amd64", "os.version": "10.0.17763.1"}),
            (windows, {"os": "windows", "architecture": "amd64", "os.version": WINDOWS_OS}),
        ],
        subject.WINDOWS_NIGHTLY_TAG,
    )
    if "linux" not in skip:
        publish_linux(web, HUB, f"nightly-full-main-{revision[:8]}-jmx", linux_version or version, revision)
    publish_packages(web, "nightly", version, skip)
    return windows


def publish_linux(web, base, tag, version, revision, windows_child=None):
    entries = [
        (web.image(base, "linux", arch, labels(version, revision)), {"os": "linux", "architecture": arch})
        for arch in ("amd64", "arm64")
    ]
    entries.append(("sha256:" + "0" * 64, {"os": "unknown", "architecture": "unknown"}))
    if windows_child:
        entries.append((windows_child, {"os": "windows", "architecture": "amd64", "os.version": WINDOWS_OS}))
    return web.index(base, entries, tag)


def publish_packages(web, kind, version, skip=()):
    for arch in agent_version.MACOS_ARCHITECTURES:
        web.package(agent_version.macos_package_url(kind, version, arch), f"dmg {arch} {version}".encode())
    for arch in agent_version.LINUX_ARCHITECTURES:
        if f"deb-{arch}" not in skip:
            web.package(agent_version.debug_deb_url(kind, version, arch), f"deb {arch} {version}".encode())


class FakeClock:
    def __init__(self):
        self.now = 0.0

    def __call__(self):
        return self.now

    def sleep(self, seconds):
        self.now += seconds


class DiscoveryTest(unittest.TestCase):
    def setUp(self):
        self.web = FakeWeb()
        self.clock = FakeClock()
        self.http = subject.Http(self.web.transport, budget_seconds=120, sleep=self.clock.sleep, clock=self.clock)
        self.github = subject.GitHub(self.http)
        stderr = contextlib.redirect_stderr(io.StringIO())
        stdout = contextlib.redirect_stdout(io.StringIO())
        stderr.__enter__()
        stdout.__enter__()
        self.addCleanup(stderr.__exit__, None, None, None)
        self.addCleanup(stdout.__exit__, None, None, None)

    def nightly(self, merged=None, windows_digest=None):
        hub = subject.Registry(self.http, subject.DOCKER_HUB, subject.NIGHTLY_REPOSITORY)
        return subject.discover_nightly(hub, self.http, self.github, merged, windows_digest)

    def release(self, merged):
        registry = subject.Registry(self.http, subject.DATADOG_REGISTRY, subject.RELEASE_REPOSITORY)
        return subject.discover_release(registry, self.http, self.github, merged)


class NightlyDiscoveryTest(DiscoveryTest):
    def test_selects_a_complete_matching_set(self):
        windows_child = publish_nightly(self.web)
        self.web.json(f"{GITHUB}/compare/{OLD_REVISION}...{REVISION}", {"status": "ahead"})

        version_file = self.nightly(make_version_file())

        self.assertEqual(agent_version.validate_version_file(version_file), [])
        integration = version_file["integration"]
        self.assertEqual((integration["version"], integration["git_revision"], integration["pipeline_id"]),
                         (VERSION, REVISION, 99000))
        self.assertEqual(integration["built_at"], "2026-11-16T03:40:12Z")
        self.assertEqual(integration["windows"]["image"], f"datadog/agent-dev@{windows_child}")
        self.assertEqual(integration["windows"]["os_version"], WINDOWS_OS)
        self.assertIsNotNone(integration["windows"]["source_index_digest"])
        self.assertEqual(
            integration["macos"]["arm64"]["sha256"], hashlib.sha256(f"dmg arm64 {VERSION}".encode()).hexdigest()
        )
        self.assertNotIn("target", version_file)

    def test_skips_a_nightly_whose_packages_are_not_published(self):
        publish_nightly(self.web, skip=("linux", "deb-amd64"))
        with self.assertRaisesRegex(subject.Skip, "_amd64.deb is not published"):
            self.nightly()

    def test_fails_when_the_packages_exist_but_the_linux_image_does_not(self):
        publish_nightly(self.web, skip=("linux",))
        expected = "nightly-full-main-89abcdef-jmx does not exist.*LINUX_NIGHTLY_TAG"
        with self.assertRaisesRegex(subject.DiscoveryError, expected):
            self.nightly()

    def test_skips_a_nightly_from_an_agent_release_branch(self):
        publish_nightly(self.web, on_main=False)
        with self.assertRaisesRegex(subject.Skip, "is not on Agent main"):
            self.nightly()

    def test_selects_the_windows_build_when_windows_is_behind_linux(self):
        publish_nightly(self.web)
        newer_revision = "f" * 40
        publish_linux(self.web, HUB, f"nightly-full-main-{newer_revision[:8]}-jmx",
                      "7.87.0-devel+git.51.fffffff.pipeline.99100", newer_revision)
        self.assertEqual(self.nightly()["integration"]["git_revision"], REVISION)

    def test_fails_on_a_windows_version_it_does_not_understand(self):
        publish_nightly(self.web, version="7.87.0-nightly.20261116", skip=("linux",))
        with self.assertRaisesRegex(subject.DiscoveryError, "update NIGHTLY_VERSION_RE"):
            self.nightly()

    def test_fails_on_a_linux_index_without_both_architectures(self):
        publish_nightly(self.web, skip=("linux",))
        amd64 = self.web.image(HUB, "linux", "amd64", labels(VERSION, REVISION))
        tag = f"nightly-full-main-{REVISION[:8]}-jmx"
        self.web.index(HUB, [(amd64, {"os": "linux", "architecture": "amd64"})], tag)
        with self.assertRaisesRegex(subject.DiscoveryError, "has no linux/arm64 image; it has linux/amd64"):
            self.nightly()

    def test_fails_on_a_windows_index_without_ltsc2022(self):
        nanoserver = self.web.image(HUB, "windows", "amd64", labels(VERSION, REVISION), os_version="10.0.17763.1")
        self.web.index(
            HUB, [(nanoserver, {"os": "windows", "architecture": "amd64", "os.version": "10.0.17763.1"})],
            subject.WINDOWS_NIGHTLY_TAG,
        )
        expected = "no windows/amd64 image .* it has windows/amd64 10.0.17763.1"
        with self.assertRaisesRegex(subject.DiscoveryError, expected):
            self.nightly()

    def test_skips_a_windows_image_from_a_release_branch(self):
        publish_nightly(self.web, version="7.86.2-rc.1", skip=("linux",))
        with self.assertRaisesRegex(subject.Skip, "not an Agent main nightly"):
            self.nightly()

    def test_skips_mismatched_pipelines(self):
        publish_nightly(self.web, linux_version="7.87.0-devel+git.50.89abcde.pipeline.99001")
        with self.assertRaisesRegex(subject.Skip, "Linux amd64 image is .*pipeline.99001"):
            self.nightly()

    def test_reads_the_windows_tag_once(self):
        publish_nightly(self.web)
        moved = publish_nightly(self.web, version="7.87.0-devel+git.52.89abcde.pipeline.99200", skip=("linux",))
        self.assertNotEqual(moved, None)

        version_file = self.nightly()

        tag_url = f"{HUB}/manifests/{subject.WINDOWS_NIGHTLY_TAG}"
        self.assertEqual(self.web.requests.count(("GET", tag_url)), 1)
        self.assertEqual(version_file["integration"]["version"], VERSION)

    def test_skips_missing_packages(self):
        publish_nightly(self.web, skip=("deb-arm64",))
        with self.assertRaisesRegex(subject.Skip, "datadog-agent-dbg_.*_arm64.deb is not published"):
            self.nightly()
        self.assertFalse(any(method == "GET" and url.endswith(".dmg") for method, url in self.web.requests))

    def test_retries_rate_limits_within_the_budget(self):
        tag_url = f"{HUB}/manifests/{subject.WINDOWS_NIGHTLY_TAG}"
        self.web.add("GET", tag_url, (429, {"retry-after": "5"}, b""), (503, {}, b""))
        publish_nightly(self.web)
        self.assertEqual(self.nightly()["integration"]["version"], VERSION)
        self.assertGreaterEqual(self.clock.now, 5)

    def test_fails_when_rate_limits_outlast_the_budget(self):
        self.web.add("GET", f"{HUB}/manifests/{subject.WINDOWS_NIGHTLY_TAG}", (429, {}, b""))
        with self.assertRaisesRegex(subject.DiscoveryError, "failed after retries: HTTP 429"):
            self.nightly()

    def test_retries_an_interrupted_download(self):
        publish_nightly(self.web)
        url = agent_version.macos_package_url("nightly", VERSION, "arm64")
        contents = f"dmg arm64 {VERSION}".encode()

        def interrupted():
            yield contents[:3]
            raise http.client.IncompleteRead(contents[:3], len(contents) - 3)

        self.web.routes[("GET", url)] = [(200, {}, interrupted), (200, {}, contents)]
        version_file = self.nightly()
        self.assertEqual(version_file["integration"]["macos"]["arm64"]["sha256"], hashlib.sha256(contents).hexdigest())

    def test_fails_when_downloads_keep_failing(self):
        publish_nightly(self.web)
        url = agent_version.debug_deb_url("nightly", VERSION, "amd64")
        self.web.routes[("GET", url)] = [ConnectionResetError("reset")]
        with self.assertRaisesRegex(subject.DiscoveryError, "ConnectionResetError"):
            self.nightly()

    def test_rejects_malformed_registry_responses(self):
        self.web.add("GET", f"{HUB}/manifests/{subject.WINDOWS_NIGHTLY_TAG}", (200, {}, b"{not json"))
        with self.assertRaisesRegex(subject.DiscoveryError, "malformed JSON"):
            self.nightly()

    def test_rejects_content_that_does_not_match_its_digest(self):
        windows_child = publish_nightly(self.web)
        self.web.routes[("GET", f"{HUB}/manifests/{windows_child}")] = [(200, {}, b"{}")]
        with self.assertRaisesRegex(subject.DiscoveryError, "returned content with digest"):
            self.nightly()

    def test_skips_older_candidates(self):
        publish_nightly(self.web)
        merged = make_version_file()
        merged["integration"]["pipeline_id"] = 99000
        merged["integration"]["version"] = "7.87.0-devel+git.42.0123456.pipeline.99000"
        with self.assertRaisesRegex(subject.Skip, "not newer than the current pipeline 99000"):
            self.nightly(merged)

        self.web.json(f"{GITHUB}/compare/{OLD_REVISION}...{REVISION}", {"status": "behind"})
        with self.assertRaisesRegex(subject.Skip, "is not ahead of the current commit"):
            self.nightly(make_version_file())

    def test_accepts_a_supplied_windows_child_digest(self):
        windows_child = publish_nightly(self.web)
        self.web.routes.pop(("GET", f"{HUB}/manifests/{subject.WINDOWS_NIGHTLY_TAG}"))
        version_file = self.nightly(windows_digest=windows_child)
        self.assertIsNone(version_file["integration"]["windows"]["source_index_digest"])
        self.assertEqual(version_file["integration"]["windows"]["image"], f"datadog/agent-dev@{windows_child}")

    def test_authenticates_with_the_registry_token_challenge(self):
        publish_nightly(self.web)
        tag_url = f"{HUB}/manifests/{subject.WINDOWS_NIGHTLY_TAG}"
        challenge = 'Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:x:pull"'
        self.web.routes[("GET", tag_url)].insert(0, (401, {"www-authenticate": challenge}, b""))
        self.web.json("https://auth.docker.io/token?service=registry.docker.io&scope=repository%3Ax%3Apull",
                      {"token": "test-token"})
        self.assertEqual(self.nightly()["integration"]["version"], VERSION)


def publish_release(web, version, revision=REVISION, skip=()):
    windows = web.image(DATADOG, "windows", "amd64", labels(version, revision), os_version=WINDOWS_OS)
    if "windows" not in skip:
        web.index(DATADOG, [(windows, {"os": "windows", "architecture": "amd64", "os.version": WINDOWS_OS})],
                  f"{version}-servercore-ltsc2022-jmx")
    publish_linux(web, DATADOG, f"{version}-full", version, revision, windows_child=windows)
    publish_packages(web, "rc" if "-rc." in version else "release", version)


class ReleaseDiscoveryTest(DiscoveryTest):
    def tags(self, *versions, annotated=()):
        refs = []
        for version in versions:
            if version in annotated:
                tag_sha = hashlib.sha1(version.encode()).hexdigest()
                self.web.json(f"{GITHUB}/git/tags/{tag_sha}", {"object": {"type": "commit", "sha": REVISION}})
                refs.append({"ref": f"refs/tags/{version}", "object": {"type": "tag", "sha": tag_sha}})
            else:
                refs.append({"ref": f"refs/tags/{version}", "object": {"type": "commit", "sha": REVISION}})
        self.web.json(f"{GITHUB}/git/matching-refs/tags/7.87.", refs)

    def test_orders_release_candidates_numerically(self):
        versions = ["7.87.0-rc.2", "7.87.0-rc.10", "7.87.0", "7.87.1", "7.87.1-rc.1"]
        self.assertEqual(
            subject.eligible_versions(versions, "nightly", "x"), ["7.87.1", "7.87.0", "7.87.0-rc.10", "7.87.0-rc.2"]
        )
        self.assertEqual(subject.eligible_versions(versions, "rc", "7.87.0-rc.2"), ["7.87.1", "7.87.0", "7.87.0-rc.10"])
        self.assertEqual(subject.eligible_versions(versions, "release", "7.87.0"), ["7.87.1"])
        self.assertEqual(subject.eligible_versions(["7.87.0-rc.2"], "rc", "7.87.0-rc.10"), [])

    def test_first_complete_release_candidate_replaces_the_inherited_nightly(self):
        self.tags("7.87.0-rc.1", "7.87.0-installer-0.1", "7.87.0-devel", annotated=("7.87.0-rc.1",))
        publish_release(self.web, "7.87.0-rc.1")

        version_file = self.release(make_version_file(target="7.87"))

        self.assertEqual(agent_version.validate_version_file(version_file), [])
        self.assertEqual(version_file["target"], "7.87")
        integration = version_file["integration"]
        self.assertEqual((integration["kind"], integration["version"]), ("rc", "7.87.0-rc.1"))
        self.assertIsNone(integration["pipeline_id"])
        self.assertTrue(integration["linux"]["image"].startswith("registry.datadoghq.com/agent@sha256:"))

    def test_skips_a_release_candidate_with_missing_artifacts(self):
        self.tags("7.87.0-rc.2", "7.87.0-rc.10")
        publish_release(self.web, "7.87.0-rc.2")
        publish_release(self.web, "7.87.0-rc.10", skip=("windows",))
        self.assertEqual(self.release(make_version_file(target="7.87"))["integration"]["version"], "7.87.0-rc.2")

    def test_moves_from_stable_to_stable_patches_only(self):
        self.tags("7.87.0", "7.87.1-rc.1", "7.87.1")
        publish_release(self.web, "7.87.1")
        current = make_version_file("release", "7.87.0", target="7.87")
        self.assertEqual(self.release(current)["integration"]["version"], "7.87.1")

    def test_never_moves_backwards(self):
        self.tags("7.87.0-rc.2", "7.87.0-rc.10")
        with self.assertRaisesRegex(subject.Skip, "no Agent 7.87 release is newer than 7.87.0-rc.10"):
            self.release(make_version_file("rc", "7.87.0-rc.10", target="7.87"))

    def test_keeps_the_current_version_when_every_newer_version_is_incomplete(self):
        self.tags("7.87.0-rc.1")
        publish_release(self.web, "7.87.0-rc.1", skip=("windows",))
        with self.assertRaisesRegex(subject.Skip, "no newer complete release"):
            self.release(make_version_file(target="7.87"))

    def test_a_release_cut_can_replace_a_nightly_from_another_minor(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "version.json"
            other_minor = make_version_file(version="7.88.0-devel+git.1.0123456.pipeline.98765")
            path.write_text(json.dumps(other_minor), encoding="utf-8")
            merged = subject.load_merged_version_file(path, "7.87")
            with self.assertRaisesRegex(agent_version.VersionFileError, "different target"):
                path.write_text(json.dumps(make_version_file(target="7.87")), encoding="utf-8")
                subject.load_merged_version_file(path, "7.86")
        self.tags("7.87.0-rc.3")
        publish_release(self.web, "7.87.0-rc.3")
        self.assertEqual(self.release(merged)["integration"]["version"], "7.87.0-rc.3")

    def test_refuses_to_cross_minors_from_a_release(self):
        merged = make_version_file("rc", "7.86.0-rc.1", target="7.86")
        merged["target"] = "7.87"
        with self.assertRaisesRegex(subject.DiscoveryError, "outside target 7.87"):
            self.release(merged)

    def test_rejects_images_whose_revision_differs_from_the_tag(self):
        self.tags("7.87.0-rc.1")
        publish_release(self.web, "7.87.0-rc.1", revision="e" * 40)
        with self.assertRaisesRegex(subject.Skip, "Linux amd64 image is 7.87.0-rc.1 at e+"):
            self.release(make_version_file(target="7.87"))


class CommandTest(unittest.TestCase):
    def setUp(self):
        environment = unittest.mock.patch.dict(subject.os.environ)
        environment.start()
        self.addCleanup(environment.stop)
        subject.os.environ.pop("GITHUB_OUTPUT", None)
        subject.os.environ.pop("GITHUB_TOKEN", None)

    def test_a_skip_writes_only_the_summary(self):
        web = FakeWeb()
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "out"
            arguments = subject.argparse.Namespace(
                mode="nightly", merged_file=str(Path(directory) / "missing.json"), base_branch="main",
                windows_digest=None, target=None, output_dir=output, budget_seconds=1,
            )
            publish_nightly(web, skip=("linux", "deb-arm64"))
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(subject.run(arguments, subject.Http(web.transport)), 0)
            self.assertEqual(sorted(path.name for path in output.iterdir()), ["summary.json"])
            self.assertEqual(json.loads((output / "summary.json").read_text())["status"], "skipped")

    def test_a_failed_discovery_says_why_and_writes_no_version_file(self):
        web = FakeWeb()
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "out"
            arguments = subject.argparse.Namespace(
                mode="nightly", merged_file=str(Path(directory) / "missing.json"), base_branch="main",
                windows_digest=None, target=None, output_dir=output, budget_seconds=1,
            )
            stderr = io.StringIO()
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(stderr):
                with unittest.mock.patch.dict(subject.os.environ, {"GITHUB_ACTIONS": "true"}):
                    self.assertEqual(subject.run(arguments, subject.Http(web.transport)), 1)
            self.assertEqual(sorted(path.name for path in output.iterdir()), ["summary.json"])
            summary = json.loads((output / "summary.json").read_text())
            self.assertEqual(summary["status"], "error")
            self.assertIn(f"{subject.WINDOWS_NIGHTLY_TAG} does not exist", summary["reason"])
            self.assertIn("was not changed", stderr.getvalue())

    def test_a_candidate_writes_the_version_file_and_pull_request_text(self):
        web = FakeWeb()
        publish_nightly(web)
        web.json(f"{GITHUB}/compare/{OLD_REVISION}...{REVISION}", {"status": "ahead"})
        with tempfile.TemporaryDirectory() as directory:
            merged = Path(directory) / "version.json"
            merged.write_text(json.dumps(make_version_file()), encoding="utf-8")
            output = Path(directory) / "out"
            arguments = subject.argparse.Namespace(
                mode="nightly", merged_file=str(merged), base_branch="main", windows_digest=None, target=None,
                output_dir=output, budget_seconds=1,
            )
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(subject.run(arguments, subject.Http(web.transport)), 0)
            candidate = agent_version.load_version_file(output / "datadog-agent-version.json")
            self.assertEqual(candidate["integration"]["version"], VERSION)
            body = (output / "pr-body.md").read_text()
            self.assertIn(f"compare/{OLD_REVISION}...{REVISION}", body)
            self.assertIn("/-/pipelines/99000", body)
            self.assertIn(VERSION, (output / "pr-title.txt").read_text())
            self.assertEqual((output / "merged-datadog-agent-version.json").read_text(), merged.read_text())


if __name__ == "__main__":
    unittest.main()
