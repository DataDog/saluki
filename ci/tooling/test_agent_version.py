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


MODULE_PATH = Path(__file__).with_name("agent_version.py")
spec = importlib.util.spec_from_file_location("agent_version", MODULE_PATH)
subject = importlib.util.module_from_spec(spec)
sys.modules["agent_version"] = subject
spec.loader.exec_module(subject)

# Test-only values: these digests, checksums and commits do not identify real Agent builds.
REVISION = "0123456789abcdef0123456789abcdef01234567"
NIGHTLY_VERSION = "7.87.0-devel+git.42.0123456.pipeline.98765"


RELEASE_LINES_TEXT = '{"agent_data_plane": "1.8", "agent": "7.86"}\n{"agent_data_plane": "1.9", "agent": "7.87"}\n'
RELEASE_LINES = subject.parse_release_lines(RELEASE_LINES_TEXT)


def digest(character):
    return "sha256:" + character * 64


def make_version_file(kind="nightly", version=NIGHTLY_VERSION, target=None):
    """Build a complete, valid test version file for an Agent version."""
    repository = subject.image_repository(kind)
    version_file = {
        "schema_version": 1,
        "integration": {
            "kind": kind,
            "version": version,
            "git_revision": REVISION,
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
        version_file["target"] = target
    return version_file


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



class ReleaseLinesTest(unittest.TestCase):
    def errors(self, *lines):
        return subject.parse_release_lines("".join(line + "\n" for line in lines))[1]

    def assert_rejected(self, message, *lines):
        errors = self.errors(*lines)
        self.assertTrue(any(message in error for error in errors), f"{message!r} not in {errors}")

    def test_reads_each_line_in_order(self):
        text = (
            '{"agent_data_plane": "1.9", "agent": "7.87"}\n{"agent_data_plane": "1.10", "agent": "7.88"}\n'
            '{"agent_data_plane": "2.0", "agent": "7.89"}\n{"agent_data_plane": "2.1", "agent": "8.0"}\n'
        )
        lines, errors = subject.parse_release_lines(text)
        self.assertEqual(errors, [])
        self.assertEqual(lines, {"1.9": "7.87", "1.10": "7.88", "2.0": "7.89", "2.1": "8.0"})

    def test_the_committed_file_is_valid(self):
        path = MODULE_PATH.parents[2] / subject.RELEASE_LINES_FILE
        self.assertEqual(subject.parse_release_lines(path.read_text(encoding="utf-8"))[1], [])

    def test_requires_the_canonical_form(self):
        for line in (
            '{"agent": "7.87", "agent_data_plane": "1.9"}',
            '{"agent_data_plane":"1.9","agent":"7.87"}',
            '{"agent_data_plane": "1.9",  "agent": "7.87"}',
            ' {"agent_data_plane": "1.9", "agent": "7.87"}',
        ):
            with self.subTest(line=line):
                self.assert_rejected('must be written exactly as {"agent_data_plane": "1.9", "agent": "7.87"}', line)

    def test_rejects_unknown_keys_values_and_blank_lines(self):
        self.assert_rejected("must be {", '{"agent-data-plane": "1.9", "agent": "7.87"}')
        self.assert_rejected("must be {", '{"agent_data_plane": "1.9", "agent": "7.87", "note": "x"}')
        self.assert_rejected("must be {", '{"agent_data_plane": "1.9", "agent": "7.87"}', "")
        self.assert_rejected("major.minor", '{"agent_data_plane": "1.9.0", "agent": "7.87"}')
        self.assert_rejected("major.minor", '{"agent_data_plane": "1.9", "agent": 7.87}')

    def test_requires_a_final_newline(self):
        _, errors = subject.parse_release_lines('{"agent_data_plane": "1.9", "agent": "7.87"}')
        self.assertIn("must end with a newline", errors[0])

    def test_sorts_numerically_by_adp_line(self):
        self.assertEqual(
            self.errors(
                '{"agent_data_plane": "1.9", "agent": "7.87"}', '{"agent_data_plane": "1.10", "agent": "7.88"}'
            ),
            [],
        )
        self.assert_rejected(
            "ADP line 1.9 must come after 1.10",
            '{"agent_data_plane": "1.10", "agent": "7.88"}', '{"agent_data_plane": "1.9", "agent": "7.87"}',
        )
        self.assert_rejected(
            "ADP line 1.9 must come after 1.9",
            '{"agent_data_plane": "1.9", "agent": "7.87"}', '{"agent_data_plane": "1.9", "agent": "7.88"}',
        )

    def test_agent_minors_strictly_increase(self):
        self.assert_rejected(
            "Agent 7.87 for ADP 1.10 must be newer than 7.87",
            '{"agent_data_plane": "1.9", "agent": "7.87"}', '{"agent_data_plane": "1.10", "agent": "7.87"}',
        )
        self.assertEqual(
            self.errors('{"agent_data_plane": "2.0", "agent": "7.89"}', '{"agent_data_plane": "2.1", "agent": "8.0"}'),
            [],
        )

    def test_the_default_next_entry_is_the_newest_agent_minor_plus_one(self):
        self.assertEqual(
            subject.next_release_line({"1.9": "7.87"}, "1.10"), '{"agent_data_plane": "1.10", "agent": "7.88"}'
        )


class ValidateVersionFileTest(unittest.TestCase):
    def assert_rejected(self, version_file, message):
        errors = subject.validate_version_file(version_file)
        self.assertTrue(any(message in error for error in errors), f"{message!r} not in {errors}")

    def test_accepts_complete_version_files_of_every_kind(self):
        for version_file in (
            make_version_file(),
            make_version_file(target="7.87"),
            make_version_file("rc", "7.87.0-rc.2", target="7.87"),
            make_version_file("release", "7.87.1", target="7.87"),
        ):
            with self.subTest(version=version_file["integration"]["version"]):
                self.assertEqual(subject.validate_version_file(version_file), [])

    def test_rejects_unknown_schema_version(self):
        version_file = make_version_file()
        version_file["schema_version"] = 2
        self.assert_rejected(version_file, "schema_version 2 is not supported")

    def test_rejects_unknown_fields(self):
        version_file = make_version_file()
        version_file["integration"]["linux"]["tag"] = "latest"
        self.assert_rejected(version_file, "integration.linux.tag is not a known field")

    def test_rejects_missing_architectures(self):
        cases = (
            ("linux", "platform_digests", "arm64", "integration.linux.platform_digests.arm64 is required"),
            ("macos", None, "x86_64", "integration.macos.x86_64 is required"),
            ("antithesis_debug_debs", None, "amd64", "integration.antithesis_debug_debs.amd64 is required"),
        )
        for section, subsection, arch, message in cases:
            with self.subTest(section=section, arch=arch):
                version_file = make_version_file()
                container = version_file["integration"][section]
                if subsection:
                    container = container[subsection]
                del container[arch]
                self.assert_rejected(version_file, message)

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
                version_file = make_version_file()
                mutate(version_file)
                self.assert_rejected(version_file, message)

    def test_rejects_expiring_macos_ci_packages(self):
        version_file = make_version_file()
        version_file["integration"]["macos"]["arm64"]["url"] = (
            "https://s3.amazonaws.com/dd-agent-macostesting/ci/datadog-agent-7.87.0-devel.git.42-1.arm64.dmg"
        )
        self.assert_rejected(version_file, "expiring macOS ci/ prefix")

    def test_rejects_packages_from_another_build(self):
        version_file = make_version_file()
        debs = version_file["integration"]["antithesis_debug_debs"]
        debs["amd64"]["url"] = subject.debug_deb_url("release", "7.85.4", "amd64")
        self.assert_rejected(version_file, "integration.antithesis_debug_debs.amd64.url must be")

    def test_rejects_images_from_the_wrong_repository(self):
        version_file = make_version_file("rc", "7.87.0-rc.1", target="7.87")
        version_file["integration"]["windows"]["image"] = f"datadog/agent-dev@{digest('d')}"
        self.assert_rejected(version_file, "must come from registry.datadoghq.com/agent")

    def test_requires_an_ltsc2022_windows_image(self):
        version_file = make_version_file()
        version_file["integration"]["windows"]["os_version"] = "10.0.17763.1"
        self.assert_rejected(version_file, "ltsc2022")

    def test_nightly_pipeline_and_revision_agree_with_the_version(self):
        version_file = make_version_file()
        version_file["integration"]["pipeline_id"] = 98766
        self.assert_rejected(version_file, "does not match version pipeline 98765")

        version_file = make_version_file()
        version_file["integration"]["git_revision"] = "f" * 40
        self.assert_rejected(version_file, "does not start with the version's commit 0123456")

        version_file = make_version_file()
        version_file["integration"]["pipeline_id"] = None
        self.assert_rejected(version_file, "pipeline_id must be a positive integer")

    def test_release_pins_have_no_pipeline(self):
        version_file = make_version_file("release", "7.87.0", target="7.87")
        version_file["integration"]["pipeline_id"] = 1
        self.assert_rejected(version_file, "pipeline_id must be null for kind release")

    def test_kind_and_version_agree(self):
        version_file = make_version_file()
        version_file["integration"]["kind"] = "rc"
        self.assert_rejected(version_file, "is not a rc Agent version")

    def test_rejects_versions_outside_the_target_minor(self):
        self.assert_rejected(make_version_file(target="7.86"), "is outside target 7.86")
        self.assert_rejected(make_version_file("rc", "7.88.0-rc.1", target="7.87"), "is outside target 7.87")

    def test_rejects_malformed_targets_and_timestamps(self):
        self.assert_rejected(make_version_file(target="7.87.0"), "must be an Agent major.minor")
        version_file = make_version_file()
        version_file["integration"]["built_at"] = "2026-11-16T03:40:00.123+00:00"
        self.assert_rejected(version_file, "built_at must be a UTC timestamp")


class DeclaredVersionTest(unittest.TestCase):
    def test_main_declares_the_nightly_and_release_lines_declare_their_target(self):
        self.assertEqual(subject.declared_version(make_version_file()), NIGHTLY_VERSION)
        self.assertEqual(subject.declared_version(make_version_file(target="7.87")), "7.87.0")
        self.assertEqual(subject.declared_version(make_version_file("release", "7.87.3", target="7.87")), "7.87.0")


class CiDeclaredVersionTest(unittest.TestCase):
    def test_the_linux_adp_build_resolves_the_same_declared_version(self):
        jq = shutil.which("jq")
        if not jq:
            self.skipTest("jq is not installed")
        build = (MODULE_PATH.parents[2] / ".gitlab/build.yml").read_text(encoding="utf-8")
        expression = re.search(r"jq -er '([^']*)' \\\s+\.datadog-agent-version\.json", build).group(1)
        version_files = (
            make_version_file(), make_version_file(target="7.87"), make_version_file("release", "7.87.2", target="7.87")
        )
        for version_file in version_files:
            resolved = subprocess.run(
                [jq, "-er", expression], input=json.dumps(version_file), capture_output=True, text=True, check=True
            ).stdout.strip()
            self.assertEqual(resolved, subject.declared_version(version_file))


class CheckChangeTest(unittest.TestCase):
    def assert_errors(self, destination, version_file, established, *messages, lines=RELEASE_LINES,
                      established_lines=RELEASE_LINES, adp_version="1.9.0"):
        errors = subject.check_change(destination, version_file, established, lines, established_lines, adp_version)
        if not messages:
            self.assertEqual(errors, [])
        for message in messages:
            self.assertTrue(any(message in error for error in errors), f"{message!r} not in {errors}")

    def test_main_takes_a_nightly_without_target(self):
        self.assert_errors("main", make_version_file(), make_version_file())
        self.assert_errors("main", make_version_file(), None)
        established = make_version_file()
        self.assert_errors("main", make_version_file(target="7.87"), established, "main must not declare target")
        self.assert_errors("main", make_version_file("release", "7.87.0"), established, "main must use a nightly")
        self.assert_errors("main", None, make_version_file(), "main requires .datadog-agent-version.json")

    def test_main_keeps_every_release_line_and_maps_its_own(self):
        established = make_version_file()
        self.assert_errors("main", established, established, "main requires .datadog-agent-release-lines.jsonl",
                           lines=None)
        self.assert_errors("main", established, established, established_lines=None)
        only_1_9 = subject.parse_release_lines('{"agent_data_plane": "1.9", "agent": "7.87"}\n')
        self.assert_errors("main", established, established, "must keep ADP 1.8 mapped to Agent 7.86", lines=only_1_9)
        changed = subject.parse_release_lines(
            '{"agent_data_plane": "1.8", "agent": "7.86"}\n{"agent_data_plane": "1.9", "agent": "7.88"}\n'
        )
        self.assert_errors("main", established, established, "must keep ADP 1.9 mapped to Agent 7.87", lines=changed)
        self.assert_errors("main", established, established, "has no entry for ADP 1.10", adp_version="1.10.0")
        unsorted = subject.parse_release_lines(
            '{"agent_data_plane": "1.9", "agent": "7.87"}\n{"agent_data_plane": "1.8", "agent": "7.86"}\n'
        )
        self.assert_errors("main", established, established, "must come after 1.9", lines=unsorted)

    def test_release_cut_adds_the_mapped_target(self):
        self.assert_errors("releases/1.9.x", make_version_file(target="7.87"), make_version_file())
        self.assert_errors("releases/1.9.x", make_version_file("rc", "7.87.0-rc.1", target="7.87"), make_version_file())
        self.assert_errors("releases/1.9.x", make_version_file(target="7.86"), make_version_file(), "it must be '7.87'")
        self.assert_errors("releases/1.9.x", make_version_file(), make_version_file(), "requires target '7.87'")

    def test_a_release_cut_cannot_remap_the_inherited_line(self):
        remapped = subject.parse_release_lines(
            '{"agent_data_plane": "1.8", "agent": "7.86"}\n{"agent_data_plane": "1.9", "agent": "7.88"}\n'
        )
        self.assert_errors(
            "releases/1.9.x", make_version_file("rc", "7.88.0-rc.1", target="7.88"), make_version_file(),
            "must keep ADP 1.9 mapped to Agent 7.87", lines=remapped,
        )

    def test_release_branches_fail_without_an_entry_for_their_line(self):
        for lines in (None, subject.parse_release_lines('{"agent_data_plane": "1.8", "agent": "7.86"}\n')):
            with self.subTest(lines=lines):
                self.assert_errors(
                    "releases/1.9.x", make_version_file(target="7.87"), make_version_file(target="7.87"),
                    "has no entry for ADP 1.9; add it on main and to releases/1.9.x", lines=lines,
                )

    def test_release_pull_requests_cannot_edit_target(self):
        established = make_version_file(target="7.87")
        self.assert_errors("releases/1.9.x", make_version_file("rc", "7.87.0-rc.2", target="7.87"), established)
        self.assert_errors("releases/1.9.x", make_version_file(), established, "target is immutable")
        changed = make_version_file("rc", "7.88.0-rc.1", target="7.88")
        self.assert_errors("releases/1.9.x", changed, established, "target is immutable", "it must be '7.87'")

    def test_adopted_lines_keep_the_version_file(self):
        self.assert_errors("releases/1.9.x", None, make_version_file(target="7.87"), "cannot be removed")
        self.assert_errors("releases/1.9.x", None, make_version_file(), "cannot be removed")

    def test_lines_without_the_version_file_keep_their_process(self):
        self.assert_errors("releases/1.8.x", None, None)

    def test_a_copied_main_version_file_cannot_start_an_old_line(self):
        self.assert_errors("releases/1.8.x", make_version_file(), None, "requires target '7.86'")
        self.assert_errors("releases/1.8.x", make_version_file(target="7.87"), None, "it must be '7.86'")
        self.assert_errors("releases/1.7.x", make_version_file(), None, "has no entry for ADP 1.7")

    def test_other_destinations_keep_target(self):
        self.assert_errors("feature/stack-base", make_version_file(), make_version_file())
        self.assert_errors(
            "feature/stack-base", make_version_file(target="7.87"), make_version_file(), "target must not change"
        )


class CheckReleaseTagTest(unittest.TestCase):
    def check(self, tag="1.9.0", version_file=None, adp_version="1.9.0", established=None, on_branch=True,
              lines=RELEASE_LINES):
        return subject.check_release_tag(tag, version_file, adp_version, established, on_branch, lines)

    def test_accepts_a_tag_on_its_release_branch(self):
        version_file = make_version_file(target="7.87")
        self.assertEqual(self.check(version_file=version_file, established=version_file), (True, []))
        stable = make_version_file("release", "7.87.2", target="7.87")
        self.assertEqual(self.check("1.9.3", stable, "1.9.3", stable), (True, []))

    def test_rejects_a_tag_off_the_expected_branch(self):
        adopted, errors = self.check(version_file=make_version_file(target="7.87"), established=None, on_branch=False)
        self.assertTrue(adopted)
        self.assertIn("tag 1.9.0 is not on releases/1.9.x", errors[0])

    def test_rejects_a_target_that_disagrees_with_the_adp_line(self):
        version_file = make_version_file("release", "7.86.3", target="7.86")
        _, errors = self.check(version_file=version_file, established=version_file)
        self.assertTrue(any("must declare target '7.87'" in error for error in errors), errors)

    def test_rejects_a_line_without_a_release_lines_entry(self):
        version_file = make_version_file(target="7.87")
        for lines in (None, subject.parse_release_lines('{"agent_data_plane": "1.8", "agent": "7.86"}\n')):
            with self.subTest(lines=lines):
                _, errors = self.check(version_file=version_file, established=version_file, lines=lines)
                self.assertTrue(any("has no entry for ADP 1.9" in error for error in errors), errors)

    def test_rejects_an_adp_version_that_disagrees_with_the_tag(self):
        version_file = make_version_file(target="7.87")
        _, errors = self.check(version_file=version_file, adp_version="1.9.1-dev", established=version_file)
        self.assertTrue(any("does not match tag 1.9.0" in error for error in errors), errors)

    def test_removing_the_version_file_does_not_exempt_an_adopted_line(self):
        _, errors = self.check(version_file=None, established=make_version_file(target="7.87"))
        self.assertTrue(any("has no .datadog-agent-version.json" in error for error in errors), errors)
        _, errors = self.check(version_file=make_version_file(), established=make_version_file(target="7.87"))
        self.assertTrue(any("must declare target '7.87', not None" in error for error in errors), errors)

    def test_skips_lines_that_have_not_adopted_the_version_file(self):
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

    def write_version_file(self, version_file):
        text = json.dumps(version_file, indent=2) + "\n"
        (self.root / ".datadog-agent-version.json").write_text(text, encoding="utf-8")

    def write_release_lines(self, text=RELEASE_LINES_TEXT):
        (self.root / ".datadog-agent-release-lines.jsonl").write_text(text, encoding="utf-8")

    def test_validate_checks_both_files(self):
        self.write_version_file(make_version_file())
        self.write_release_lines()
        version = str(self.root / ".datadog-agent-version.json")
        lines = str(self.root / ".datadog-agent-release-lines.jsonl")
        self.assertEqual(self.run_main("validate", "--file", version, "--release-lines", lines)[0], 0)
        self.write_release_lines('{"agent_data_plane":"1.9","agent":"7.87"}\n')
        code, _, stderr = self.run_main("validate", "--file", version, "--release-lines", lines)
        self.assertEqual(code, 1)
        self.assertIn("must be written exactly as", stderr)

    def test_add_release_line_appends_the_next_entry_once(self):
        self.write_release_lines()
        path = str(self.root / ".datadog-agent-release-lines.jsonl")
        self.assertEqual(self.run_main("add-release-line", "--adp-version", "1.10.0", "--release-lines", path)[0], 0)
        self.assertEqual(self.run_main("add-release-line", "--adp-version", "1.10.0", "--release-lines", path)[0], 0)
        expected = RELEASE_LINES_TEXT + '{"agent_data_plane": "1.10", "agent": "7.88"}\n'
        self.assertEqual(Path(path).read_text(encoding="utf-8"), expected)

    def test_get_and_declared_version_read_a_valid_version_file(self):
        self.write_version_file(make_version_file(target="7.87"))
        path = str(self.root / ".datadog-agent-version.json")
        self.assertEqual(
            self.run_main("get", "integration.linux.image", "--file", path),
            (0, f"datadog/agent-dev@{digest('a')}\n", ""),
        )
        self.assertEqual(self.run_main("declared-version", "--file", path), (0, "7.87.0\n", ""))

    def test_readers_refuse_an_invalid_version_file(self):
        version_file = make_version_file()
        del version_file["integration"]["macos"]["x86_64"]
        self.write_version_file(version_file)
        path = str(self.root / ".datadog-agent-version.json")
        code, stdout, stderr = self.run_main("get", "integration.version", "--file", path)
        self.assertEqual((code, stdout), (1, ""))
        self.assertIn("integration.macos.x86_64 is required", stderr)

    def test_get_rejects_missing_and_null_fields(self):
        version_file = make_version_file()
        version_file["integration"]["windows"]["source_index_digest"] = None
        self.write_version_file(version_file)
        path = str(self.root / ".datadog-agent-version.json")
        self.assertIn("has no field target", self.run_main("get", "target", "--file", path)[2])
        null_field = self.run_main("get", "integration.windows.source_index_digest", "--file", path)
        self.assertIn("is not a scalar", null_field[2])

    def test_age_is_measured_from_built_at(self):
        self.write_version_file(make_version_file())
        code, stdout, _ = self.run_main(
            "age", "--file", str(self.root / ".datadog-agent-version.json"), "--now", "2026-11-20T15:40:00Z"
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
        self.write_version_file(make_version_file())
        self.write_release_lines()
        self.commit("main")

    def test_check_change_reads_the_destination_release_lines_from_git(self):
        self.init_repository()
        self.write_release_lines('{"agent_data_plane": "1.9", "agent": "7.87"}\n')
        code, _, stderr = self.run_main(
            "check-change", "--destination", "main", "--established-ref", "main", "--repository", str(self.root)
        )
        self.assertEqual(code, 1)
        self.assertIn("must keep ADP 1.8 mapped to Agent 7.86", stderr)

    def test_check_change_reads_the_destination_branch_from_git(self):
        self.init_repository()
        self.git("branch", "releases/1.9.x")
        self.write_version_file(make_version_file(target="7.87"))
        self.assertEqual(
            self.run_main("check-change", "--destination", "releases/1.9.x", "--established-ref", "releases/1.9.x",
                          "--repository", str(self.root))[0],
            0,
        )
        (self.root / ".datadog-agent-version.json").unlink()
        code, _, stderr = self.run_main(
            "check-change", "--destination", "releases/1.9.x", "--established-ref", "releases/1.9.x",
            "--repository", str(self.root),
        )
        self.assertEqual(code, 1)
        self.assertIn("cannot be removed", stderr)

    def test_check_tag_requires_the_tag_on_its_release_branch(self):
        self.init_repository()
        self.git("update-ref", "refs/remotes/origin/releases/1.9.x", "HEAD")
        self.write_version_file(make_version_file(target="7.87"))
        self.commit("release cut")

        # The cut commit is not yet on the release branch.
        code, _, stderr = self.run_main("check-tag", "--tag", "1.9.0", "--repository", str(self.root))
        self.assertEqual(code, 1)
        self.assertIn("is not on releases/1.9.x", stderr)

        self.git("update-ref", "refs/remotes/origin/releases/1.9.x", "HEAD")
        self.assertEqual(self.run_main("check-tag", "--tag", "1.9.0", "--repository", str(self.root))[0], 0)

    def test_check_tag_can_require_an_adopted_line(self):
        self.init_repository()
        (self.root / ".datadog-agent-version.json").unlink()
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
