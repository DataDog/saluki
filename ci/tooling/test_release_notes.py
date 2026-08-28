import importlib.util
import unittest
from pathlib import Path


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


if __name__ == "__main__":
    unittest.main()
