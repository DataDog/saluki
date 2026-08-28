import importlib.util
import tempfile
import unittest
from pathlib import Path
from unittest import mock


MODULE_PATH = Path(__file__).with_name("release_notes.py")
spec = importlib.util.spec_from_file_location("release_notes", MODULE_PATH)
subject = importlib.util.module_from_spec(spec)
spec.loader.exec_module(subject)


class ReleaseBodyTest(unittest.TestCase):
    def test_accepts_three_component_release_tags(self):
        self.assertTrue(subject.is_release_tag("1.6.0"))
        self.assertFalse(subject.is_release_tag("v1.6.0"))
        self.assertFalse(subject.is_release_tag("1.6"))
        self.assertFalse(subject.is_release_tag("1.6.0-rc.1"))

    def test_prepends_generated_block_without_changing_github_notes(self):
        existing = "## What's Changed\n* Fix parser\n\n**Full Changelog**: https://example.test"

        merged = subject.merge_release_body(existing, "## Bug Fixes\n- Fix parser behavior")

        self.assertTrue(merged.startswith(subject.START_MARKER + "\n# Release notes"))
        self.assertTrue(merged.endswith(existing))

    def test_replaces_only_the_existing_generated_block(self):
        existing = subject.build_curated_block("## Bug Fixes\n- Old text") + "\n\n## What's Changed\n* PR"

        merged = subject.merge_release_body(existing, "## Bug Fixes\n- New text")

        self.assertIn("- New text", merged)
        self.assertNotIn("- Old text", merged)
        self.assertEqual(merged.count(subject.START_MARKER), 1)
        self.assertTrue(merged.endswith("## What's Changed\n* PR"))

    def test_empty_render_is_a_no_op(self):
        existing = "## What's Changed\n* PR"

        self.assertEqual(subject.merge_release_body(existing, ""), existing)

    def test_rejects_an_unpaired_marker(self):
        with self.assertRaisesRegex(ValueError, "malformed"):
            subject.merge_release_body("<!-- saluki-curated-notes:start -->", "## Bug Fixes\n- Fix")

    def test_rejects_markers_in_reverse_order(self):
        existing = "<!-- saluki-curated-notes:end -->\n<!-- saluki-curated-notes:start -->"

        with self.assertRaisesRegex(ValueError, "malformed"):
            subject.merge_release_body(existing, "## Bug Fixes\n- Fix")


class ReleaseNoteValidationTest(unittest.TestCase):
    def setUp(self):
        self.temporary_directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary_directory.cleanup)
        self.notes_directory = Path(self.temporary_directory.name)

    def write_note(self, name, content):
        path = self.notes_directory / name
        path.write_text(content, encoding="utf-8")
        return path

    def test_accepts_a_valid_note(self):
        path = self.write_note(
            "fix-listener-0123456789abcdef.yaml",
            "fixes:\n  - |\n    Fix a listener shutdown race that could drop telemetry during process exit.\n",
        )

        self.assertEqual(subject.validate_note_file(path), [])

    def test_rejects_malformed_yaml(self):
        path = self.write_note("fix-listener-0123456789abcdef.yaml", "fixes: [\n")

        self.assertTrue(subject.validate_note_file(path))

    def test_rejects_a_non_mapping_document(self):
        path = self.write_note("fix-listener-0123456789abcdef.yaml", "- fixes\n")

        self.assertTrue(subject.validate_note_file(path))

    def test_rejects_unknown_categories(self):
        path = self.write_note("fix-listener-0123456789abcdef.yaml", "unknown:\n  - text\n")

        self.assertTrue(subject.validate_note_file(path))

    def test_rejects_empty_items(self):
        path = self.write_note("fix-listener-0123456789abcdef.yaml", "fixes:\n  - \"\"\n")

        self.assertTrue(subject.validate_note_file(path))

    def test_rejects_non_reno_filenames(self):
        path = self.write_note("fix-listener.yaml", "fixes:\n  - text\n")

        self.assertTrue(subject.validate_note_file(path))


class ReleaseNoteRenderingTest(unittest.TestCase):
    def test_rejects_non_release_tags_before_running_commands(self):
        with mock.patch.object(subject.subprocess, "run") as run:
            with self.assertRaisesRegex(ValueError, "X.Y.Z"):
                subject.render_release_notes("v1.6.0", Path.cwd())
        run.assert_not_called()

    def test_renders_rst_with_reno_and_pandoc(self):
        completed = [
            mock.Mock(stdout="", stderr=""),
            mock.Mock(stdout="fixes:\n", stderr=""),
            mock.Mock(stdout="## Bug Fixes\n- Fix\n", stderr=""),
        ]
        with mock.patch.object(subject.subprocess, "run", side_effect=completed) as run:
            rendered = subject.render_release_notes("1.6.0", Path("/repo"))

        self.assertEqual(rendered, "## Bug Fixes\n- Fix\n")
        self.assertEqual(run.call_args_list[0].args[0], ["git", "rev-parse", "--verify", "refs/tags/1.6.0"])
        self.assertEqual(run.call_args_list[1].args[0], ["reno", "report", "--ignore-cache", "--no-show-source", "--version", "1.6.0"])
        self.assertEqual(run.call_args_list[2].args[0], ["pandoc", "--from", "rst", "--to", "gfm", "--wrap=none"])


if __name__ == "__main__":
    unittest.main()
