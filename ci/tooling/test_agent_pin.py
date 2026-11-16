import contextlib
import importlib.util
import io
import json
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("agent_pin.py")
spec = importlib.util.spec_from_file_location("agent_pin", MODULE_PATH)
subject = importlib.util.module_from_spec(spec)
sys.modules["agent_pin"] = subject
spec.loader.exec_module(subject)

# Test-only values: these digests, checksums and commits do not identify real Agent builds.
REVISION = "0123456789abcdef0123456789abcdef01234567"
NIGHTLY_VERSION = "7.87.0-devel+git.42.0123456.pipeline.98765"


def digest(character):
    return "sha256:" + character * 64


def make_pin(kind="nightly", version=NIGHTLY_VERSION, target=None):
    """Build a complete, valid test pin for an Agent version."""
    repository = subject.image_repository(kind)
    pin = {
        "schema_version": 1,
        "integration": {
            "kind": kind,
            "version": version,
            "revision": REVISION,
            "pipeline_id": 98765 if kind == "nightly" else None,
            "built_at": "2026-11-16T03:40:00Z",
            "linux": {
                "image": f"{repository}@{digest('a')}",
                "platform_digests": {"amd64": digest("b"), "arm64": digest("c")},
            },
            "windows": {
                "image": f"{repository}@{digest('d')}",
                "source_index_digest": digest("e"),
                "os_version": "10.0.20348.4529",
            },
            "macos": {
                arch: {"url": subject.macos_package_url(kind, version, arch), "sha256": "1" * 64}
                for arch in ("arm64", "x86_64")
            },
            "antithesis_debug_debs": {
                arch: {"url": subject.debug_deb_url(kind, version, arch), "sha256": "2" * 64}
                for arch in ("amd64", "arm64")
            },
        },
    }
    if target is not None:
        pin["target"] = target
    return pin


class PackageUrlTest(unittest.TestCase):
    def test_nightly_urls_use_long_lived_locations(self):
        self.assertEqual(
            subject.macos_package_url("nightly", NIGHTLY_VERSION, "arm64"),
            "https://s3.amazonaws.com/dd-agent-macostesting/nightly/"
            "datadog-agent-7.87.0-devel.git.42.0123456.pipeline.98765-1.arm64.dmg",
        )
        self.assertEqual(
            subject.debug_deb_url("nightly", NIGHTLY_VERSION, "amd64"),
            "https://apt.datad0g.com/pool/d/da/"
            "datadog-agent-dbg_7.87.0~devel.git.42.0123456.pipeline.98765-1_amd64.deb",
        )

    def test_release_candidate_and_stable_urls(self):
        self.assertEqual(
            subject.macos_package_url("rc", "7.87.0-rc.2", "x86_64"),
            "https://s3.amazonaws.com/dd-agent-macostesting/beta/datadog-agent-7.87.0-rc.2-1.x86_64.dmg",
        )
        self.assertEqual(
            subject.debug_deb_url("rc", "7.87.0-rc.10", "arm64"),
            "https://apt.datad0g.com/pool/d/da/datadog-agent-dbg_7.87.0~rc.10-1_arm64.deb",
        )
        self.assertEqual(
            subject.macos_package_url("release", "7.87.1", "arm64"),
            "https://s3.amazonaws.com/dd-agent/datadog-agent-7.87.1-1.arm64.dmg",
        )
        self.assertEqual(
            subject.debug_deb_url("release", "7.87.1", "amd64"),
            "https://apt.datadoghq.com/pool/d/da/datadog-agent-dbg_7.87.1-1_amd64.deb",
        )

    def test_adp_lines_map_to_agent_minors(self):
        self.assertEqual(subject.target_for_adp_line(1, 8), "7.86")
        self.assertEqual(subject.target_for_adp_line(1, 9), "7.87")


class ValidatePinTest(unittest.TestCase):
    def assert_rejected(self, pin, message):
        errors = subject.validate_pin(pin)
        self.assertTrue(any(message in error for error in errors), f"{message!r} not in {errors}")

    def test_accepts_complete_pins_of_every_kind(self):
        for pin in (
            make_pin(),
            make_pin(target="7.87"),
            make_pin("rc", "7.87.0-rc.2", target="7.87"),
            make_pin("release", "7.87.1", target="7.87"),
        ):
            with self.subTest(version=pin["integration"]["version"]):
                self.assertEqual(subject.validate_pin(pin), [])

    def test_rejects_unknown_schema_version(self):
        pin = make_pin()
        pin["schema_version"] = 2
        self.assert_rejected(pin, "schema_version 2 is not supported")

    def test_rejects_unknown_fields(self):
        pin = make_pin()
        pin["integration"]["linux"]["tag"] = "latest"
        self.assert_rejected(pin, "integration.linux.tag is not a known field")

    def test_rejects_missing_architectures(self):
        cases = (
            ("linux", "platform_digests", "arm64", "integration.linux.platform_digests.arm64 is required"),
            ("macos", None, "x86_64", "integration.macos.x86_64 is required"),
            ("antithesis_debug_debs", None, "amd64", "integration.antithesis_debug_debs.amd64 is required"),
        )
        for section, subsection, arch, message in cases:
            with self.subTest(section=section, arch=arch):
                pin = make_pin()
                container = pin["integration"][section]
                if subsection:
                    container = container[subsection]
                del container[arch]
                self.assert_rejected(pin, message)

    def test_rejects_malformed_digests_and_checksums(self):
        cases = (
            (lambda p: p["integration"]["linux"].update(image="datadog/agent-dev:nightly"), "digest reference"),
            (lambda p: p["integration"]["linux"].update(image="datadog/agent-dev@sha256:abc"), "malformed digest"),
            (lambda p: p["integration"]["linux"]["platform_digests"].update(amd64="sha1:00"), "platform_digests.amd64"),
            (lambda p: p["integration"]["windows"].update(source_index_digest="abc"), "source_index_digest"),
            (lambda p: p["integration"]["macos"]["arm64"].update(sha256="A" * 64), "macos.arm64.sha256"),
            (lambda p: p["integration"]["antithesis_debug_debs"]["arm64"].update(sha256="0" * 63), "arm64.sha256"),
        )
        for mutate, message in cases:
            with self.subTest(message=message):
                pin = make_pin()
                mutate(pin)
                self.assert_rejected(pin, message)

    def test_rejects_expiring_macos_ci_packages(self):
        pin = make_pin()
        pin["integration"]["macos"]["arm64"]["url"] = (
            "https://s3.amazonaws.com/dd-agent-macostesting/ci/datadog-agent-7.87.0-devel.git.42-1.arm64.dmg"
        )
        self.assert_rejected(pin, "expiring macOS ci/ prefix")

    def test_rejects_packages_from_another_build(self):
        pin = make_pin()
        debs = pin["integration"]["antithesis_debug_debs"]
        debs["amd64"]["url"] = subject.debug_deb_url("release", "7.85.4", "amd64")
        self.assert_rejected(pin, "integration.antithesis_debug_debs.amd64.url must be")

    def test_rejects_images_from_the_wrong_repository(self):
        pin = make_pin("rc", "7.87.0-rc.1", target="7.87")
        pin["integration"]["windows"]["image"] = f"datadog/agent-dev@{digest('d')}"
        self.assert_rejected(pin, "must come from registry.datadoghq.com/agent")

    def test_requires_an_ltsc2022_windows_image(self):
        pin = make_pin()
        pin["integration"]["windows"]["os_version"] = "10.0.17763.1"
        self.assert_rejected(pin, "ltsc2022")

    def test_nightly_pipeline_and_revision_agree_with_the_version(self):
        pin = make_pin()
        pin["integration"]["pipeline_id"] = 98766
        self.assert_rejected(pin, "does not match version pipeline 98765")

        pin = make_pin()
        pin["integration"]["revision"] = "f" * 40
        self.assert_rejected(pin, "does not start with the version's commit 0123456")

        pin = make_pin()
        pin["integration"]["pipeline_id"] = None
        self.assert_rejected(pin, "pipeline_id must be a positive integer")

    def test_release_pins_have_no_pipeline(self):
        pin = make_pin("release", "7.87.0", target="7.87")
        pin["integration"]["pipeline_id"] = 1
        self.assert_rejected(pin, "pipeline_id must be null for kind release")

    def test_kind_and_version_agree(self):
        pin = make_pin()
        pin["integration"]["kind"] = "rc"
        self.assert_rejected(pin, "is not a rc Agent version")

    def test_rejects_versions_outside_the_target_minor(self):
        self.assert_rejected(make_pin(target="7.86"), "is outside target 7.86")
        self.assert_rejected(make_pin("rc", "7.88.0-rc.1", target="7.87"), "is outside target 7.87")

    def test_rejects_malformed_targets_and_timestamps(self):
        self.assert_rejected(make_pin(target="7.87.0"), "must be an Agent major.minor")
        pin = make_pin()
        pin["integration"]["built_at"] = "2026-11-16T03:40:00.123+00:00"
        self.assert_rejected(pin, "built_at must be a UTC timestamp")


class DeclaredVersionTest(unittest.TestCase):
    def test_main_declares_the_nightly_and_release_lines_declare_their_target(self):
        self.assertEqual(subject.declared_version(make_pin()), NIGHTLY_VERSION)
        self.assertEqual(subject.declared_version(make_pin(target="7.87")), "7.87.0")
        self.assertEqual(subject.declared_version(make_pin("release", "7.87.3", target="7.87")), "7.87.0")


class CiDeclaredVersionTest(unittest.TestCase):
    def test_the_linux_adp_build_resolves_the_same_declared_version(self):
        jq = shutil.which("jq")
        if not jq:
            self.skipTest("jq is not installed")
        build = (MODULE_PATH.parents[2] / ".gitlab/build.yml").read_text(encoding="utf-8")
        expression = re.search(r"jq -er '([^']*)' \\\s+\.agent-pin\.json", build).group(1)
        for pin in (make_pin(), make_pin(target="7.87"), make_pin("release", "7.87.2", target="7.87")):
            resolved = subprocess.run(
                [jq, "-er", expression], input=json.dumps(pin), capture_output=True, text=True, check=True
            ).stdout.strip()
            self.assertEqual(resolved, subject.declared_version(pin))


class CheckChangeTest(unittest.TestCase):
    def assert_errors(self, destination, pin, established, *messages):
        errors = subject.check_change(destination, pin, established)
        if not messages:
            self.assertEqual(errors, [])
        for message in messages:
            self.assertTrue(any(message in error for error in errors), f"{message!r} not in {errors}")

    def test_main_takes_a_nightly_without_target(self):
        self.assert_errors("main", make_pin(), make_pin())
        self.assert_errors("main", make_pin(), None)
        self.assert_errors("main", make_pin(target="7.87"), make_pin(), "main must not declare target")
        self.assert_errors("main", make_pin("release", "7.87.0"), make_pin(), "main must pin a nightly")
        self.assert_errors("main", None, make_pin(), "main requires .agent-pin.json")

    def test_release_cut_adds_the_mapped_target(self):
        self.assert_errors("releases/1.9.x", make_pin(target="7.87"), make_pin())
        self.assert_errors("releases/1.9.x", make_pin("rc", "7.87.0-rc.1", target="7.87"), make_pin())
        self.assert_errors("releases/1.9.x", make_pin(target="7.86"), make_pin(), "it must be '7.87'")
        self.assert_errors("releases/1.9.x", make_pin(), make_pin(), "requires target '7.87'")

    def test_release_pull_requests_cannot_edit_target(self):
        established = make_pin(target="7.87")
        self.assert_errors("releases/1.9.x", make_pin("rc", "7.87.0-rc.2", target="7.87"), established)
        self.assert_errors("releases/1.9.x", make_pin(), established, "target is immutable")
        changed = make_pin("rc", "7.88.0-rc.1", target="7.88")
        self.assert_errors("releases/1.9.x", changed, established, "target is immutable", "it must be '7.87'")

    def test_adopted_lines_keep_the_pin(self):
        self.assert_errors("releases/1.9.x", None, make_pin(target="7.87"), "cannot be removed")
        self.assert_errors("releases/1.9.x", None, make_pin(), "cannot be removed")

    def test_lines_without_the_pin_keep_their_process(self):
        self.assert_errors("releases/1.8.x", None, None)

    def test_a_copied_main_pin_cannot_start_an_old_line(self):
        self.assert_errors("releases/1.8.x", make_pin(), None, "requires target '7.86'")
        self.assert_errors("releases/1.8.x", make_pin(target="7.87"), None, "it must be '7.86'")

    def test_other_destinations_keep_target(self):
        self.assert_errors("feature/stack-base", make_pin(), make_pin())
        self.assert_errors("feature/stack-base", make_pin(target="7.87"), make_pin(), "target must not change")


class CheckReleaseTagTest(unittest.TestCase):
    def check(self, tag="1.9.0", pin=None, adp_version="1.9.0", established=None, on_branch=True):
        return subject.check_release_tag(tag, pin, adp_version, established, on_branch)

    def test_accepts_a_tag_on_its_release_branch(self):
        pin = make_pin(target="7.87")
        self.assertEqual(self.check(pin=pin, established=pin), (True, []))
        stable = make_pin("release", "7.87.2", target="7.87")
        self.assertEqual(self.check("1.9.3", stable, "1.9.3", stable), (True, []))

    def test_rejects_a_tag_off_the_expected_branch(self):
        adopted, errors = self.check(pin=make_pin(target="7.87"), established=None, on_branch=False)
        self.assertTrue(adopted)
        self.assertIn("tag 1.9.0 is not on releases/1.9.x", errors[0])

    def test_rejects_a_target_that_disagrees_with_the_adp_line(self):
        pin = make_pin("release", "7.86.3", target="7.86")
        _, errors = self.check(pin=pin, established=pin)
        self.assertTrue(any("must declare target '7.87'" in error for error in errors), errors)

    def test_rejects_an_adp_version_that_disagrees_with_the_tag(self):
        pin = make_pin(target="7.87")
        _, errors = self.check(pin=pin, adp_version="1.9.1-dev", established=pin)
        self.assertTrue(any("does not match tag 1.9.0" in error for error in errors), errors)

    def test_removing_the_pin_does_not_exempt_an_adopted_line(self):
        _, errors = self.check(pin=None, established=make_pin(target="7.87"))
        self.assertTrue(any("has no .agent-pin.json" in error for error in errors), errors)
        _, errors = self.check(pin=make_pin(), established=make_pin(target="7.87"))
        self.assertTrue(any("must declare target '7.87', not None" in error for error in errors), errors)

    def test_skips_lines_that_have_not_adopted_the_pin(self):
        self.assertEqual(self.check("1.8.2", None, "1.8.2", None, on_branch=True), (False, []))


class CommandTest(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)
        self.addCleanup(self.directory.cleanup)

    def run_main(self, *arguments):
        stdout, stderr = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            code = subject.main(list(arguments))
        return code, stdout.getvalue(), stderr.getvalue()

    def write_pin(self, pin):
        (self.root / ".agent-pin.json").write_text(json.dumps(pin, indent=2) + "\n", encoding="utf-8")

    def test_get_and_declared_version_read_a_valid_pin(self):
        self.write_pin(make_pin(target="7.87"))
        path = str(self.root / ".agent-pin.json")
        self.assertEqual(
            self.run_main("get", "integration.linux.image", "--pin", path),
            (0, f"datadog/agent-dev@{digest('a')}\n", ""),
        )
        self.assertEqual(self.run_main("declared-version", "--pin", path), (0, "7.87.0\n", ""))

    def test_readers_refuse_an_invalid_pin(self):
        pin = make_pin()
        del pin["integration"]["macos"]["x86_64"]
        self.write_pin(pin)
        code, stdout, stderr = self.run_main("get", "integration.version", "--pin", str(self.root / ".agent-pin.json"))
        self.assertEqual((code, stdout), (1, ""))
        self.assertIn("integration.macos.x86_64 is required", stderr)

    def test_get_rejects_missing_and_null_fields(self):
        pin = make_pin()
        pin["integration"]["windows"]["source_index_digest"] = None
        self.write_pin(pin)
        path = str(self.root / ".agent-pin.json")
        self.assertIn("has no field target", self.run_main("get", "target", "--pin", path)[2])
        null_field = self.run_main("get", "integration.windows.source_index_digest", "--pin", path)
        self.assertIn("is not a scalar", null_field[2])

    def test_age_is_measured_from_built_at(self):
        self.write_pin(make_pin())
        code, stdout, _ = self.run_main(
            "age", "--pin", str(self.root / ".agent-pin.json"), "--now", "2026-11-20T15:40:00Z"
        )
        self.assertEqual(code, 0)
        self.assertIn("4.5 days old", stdout)

    def git(self, *arguments):
        subprocess.run(["git", "-C", str(self.root), *arguments], check=True, capture_output=True)

    def commit(self, message):
        self.git("add", "-A")
        self.git("-c", "user.name=test", "-c", "user.email=test@example.test", "commit", "-q", "-m", message)

    def init_repository(self):
        self.git("init", "-q", "-b", "main")
        (self.root / "bin/agent-data-plane").mkdir(parents=True)
        (self.root / "bin/agent-data-plane/Cargo.toml").write_text('[package]\nversion = "1.9.0"\n', encoding="utf-8")
        self.write_pin(make_pin())
        self.commit("main")

    def test_check_change_reads_the_destination_branch_from_git(self):
        self.init_repository()
        self.git("branch", "releases/1.9.x")
        self.write_pin(make_pin(target="7.87"))
        self.assertEqual(
            self.run_main("check-change", "--destination", "releases/1.9.x", "--established-ref", "releases/1.9.x",
                          "--repository", str(self.root))[0],
            0,
        )
        (self.root / ".agent-pin.json").unlink()
        code, _, stderr = self.run_main(
            "check-change", "--destination", "releases/1.9.x", "--established-ref", "releases/1.9.x",
            "--repository", str(self.root),
        )
        self.assertEqual(code, 1)
        self.assertIn("cannot be removed", stderr)

    def test_check_tag_requires_the_tag_on_its_release_branch(self):
        self.init_repository()
        self.git("update-ref", "refs/remotes/origin/releases/1.9.x", "HEAD")
        self.write_pin(make_pin(target="7.87"))
        self.commit("release cut")

        # The cut commit is not yet on the release branch.
        code, _, stderr = self.run_main("check-tag", "--tag", "1.9.0", "--repository", str(self.root))
        self.assertEqual(code, 1)
        self.assertIn("is not on releases/1.9.x", stderr)

        self.git("update-ref", "refs/remotes/origin/releases/1.9.x", "HEAD")
        self.assertEqual(self.run_main("check-tag", "--tag", "1.9.0", "--repository", str(self.root))[0], 0)

    def test_check_tag_can_require_an_adopted_line(self):
        self.init_repository()
        (self.root / ".agent-pin.json").unlink()
        (self.root / "bin/agent-data-plane/Cargo.toml").write_text('[package]\nversion = "1.8.2"\n', encoding="utf-8")
        self.commit("old line")
        self.assertEqual(self.run_main("check-tag", "--tag", "1.8.2", "--repository", str(self.root))[0], 0)
        code, _, stderr = self.run_main(
            "check-tag", "--tag", "1.8.2", "--require-adopted", "--repository", str(self.root)
        )
        self.assertEqual(code, 1)
        self.assertIn("has not adopted", stderr)


if __name__ == "__main__":
    unittest.main()
