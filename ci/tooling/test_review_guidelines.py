import contextlib
import importlib.util
import io
import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
import unittest.mock
from pathlib import Path
from types import SimpleNamespace


MODULE_PATH = Path(__file__).with_name("review_guidelines.py")
spec = importlib.util.spec_from_file_location("review_guidelines", MODULE_PATH)
subject = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = subject
spec.loader.exec_module(subject)


def page(text: str) -> str:
    return textwrap.dedent(text).lstrip("\n")


class ParseRegionsTest(unittest.TestCase):
    def test_reads_region_body_and_enclosing_heading(self):
        regions = subject.parse_regions(
            "conventions.md",
            page(
                """
                # Conventions

                ## Hot paths

                Intro text that isn't part of the region.

                <!-- review:hot-paths -->
                - Avoid allocations.

                - Avoid clones.
                <!-- /review -->
                """
            ),
        )

        self.assertEqual(len(regions), 1)
        self.assertEqual(regions[0].key, "conventions.md#hot-paths")
        self.assertEqual(regions[0].heading, "Hot paths")
        self.assertEqual(regions[0].body, "- Avoid allocations.\n\n- Avoid clones.")

    def test_reads_setext_and_closed_atx_headings(self):
        regions = subject.parse_regions(
            "page.md",
            "Notes on C#\n===\n\n<!-- review:a -->\nx\n<!-- /review -->\n\n## Closed ##\n\n<!-- review:b -->\ny\n<!-- /review -->\n",
        )

        self.assertEqual([region.heading for region in regions], ["Notes on C#", "Closed"])

    def test_ignores_markers_and_headings_inside_code_fences(self):
        regions = subject.parse_regions(
            "style-guide.md",
            page(
                """
                ## Example

                ```markdown
                <!-- review:not-a-region -->
                ```

                <!-- review:real -->
                ```rust
                # fn hidden_rustdoc_line() {}
                ```
                <!-- /review -->
                """
            ),
        )

        self.assertEqual([region.name for region in regions], ["real"])
        self.assertIn("# fn hidden_rustdoc_line() {}", regions[0].body)

    def test_keeps_longer_fences_open_across_shorter_ones(self):
        regions = subject.parse_regions(
            "style-guide.md",
            page(
                """
                ````markdown
                ```rust
                ```
                <!-- review:still-in-the-example -->
                ````

                <!-- review:real -->
                Text.
                <!-- /review -->
                """
            ),
        )

        self.assertEqual([region.name for region in regions], ["real"])

    def test_inline_code_with_three_backticks_does_not_open_a_fence(self):
        regions = subject.parse_regions(
            "contributing.md",
            "``` ``dogstatsd_port`` ``` instead.\n\n<!-- review:a -->\nText.\n<!-- /review -->\n",
        )

        self.assertEqual([region.name for region in regions], ["a"])

    def test_allows_markers_named_inside_code_spans(self):
        regions = subject.parse_regions("code-review.md", "Text between `<!-- review:NAME -->` markers.\n")

        self.assertEqual(regions, [])

    def test_ignores_carriage_returns_and_byte_order_marks(self):
        regions = subject.parse_regions("page.md", "﻿## Title\r\n\r\n<!-- review:a -->\r\nText.\r\n<!-- /review -->\r\n")

        self.assertEqual(regions[0].heading, "Title")
        self.assertEqual(regions[0].body, "Text.")

    def test_rejects_malformed_markers(self):
        cases = {
            "nested": "<!-- review:a -->\n<!-- review:b -->\ntext\n<!-- /review -->\n",
            "unclosed": "<!-- review:a -->\ntext\n",
            "stray end": "text\n<!-- /review -->\n",
            "duplicate": "<!-- review:a -->\nx\n<!-- /review -->\n<!-- review:a -->\ny\n<!-- /review -->\n",
            "inline": "Some text <!-- review:a --> more text.\n",
            "indented": "    <!-- review:a -->\n    text\n    <!-- /review -->\n",
            "atx heading inside": "<!-- review:a -->\n### Heading\ntext\n<!-- /review -->\n",
            "setext heading inside": "<!-- review:a -->\nHeading\n---\n<!-- /review -->\n",
            "empty": "<!-- review:a -->\n\n<!-- /review -->\n",
            "unclosed fence": "<!-- review:a -->\ntext\n<!-- /review -->\n```rust\nfn main() {}\n",
        }
        for name, text in cases.items():
            with self.subTest(name), self.assertRaises(subject.GuidelineError):
                subject.parse_regions("page.md", text)


class StripRelativeLinksTest(unittest.TestCase):
    def test_turns_relative_links_into_text(self):
        cases = {
            "See [Testing](./testing.md#names) and [above](#scope).": "See Testing and above.",
            "See [`release_notes.py`](../../ci/tooling/release_notes.py).": "See `release_notes.py`.",
            'See [Testing](./testing.md "Title").': "See Testing.",
            "See [the [x] section](./t.md).": "See the [x] section.",
            "See [x](./foo_(bar).md).": "See x.",
        }
        for text, expected in cases.items():
            with self.subTest(text):
                self.assertEqual(subject.strip_relative_links(text), expected)

    def test_keeps_absolute_links_code_spans_escapes_and_fences(self):
        cases = [
            "Read [RFC 2119](https://www.rfc-editor.org/rfc/rfc2119).",
            "Text `[x](./y.md)` and a span that\nwraps `[z](./z.md)`.",
            r"Escaped \[x\](y) text.",
            "```rust\ncallbacks[index](event);\n```",
            "````markdown\nSee [Testing](./testing.md).\n````",
        ]
        for text in cases:
            with self.subTest(text):
                self.assertEqual(subject.strip_relative_links(text), text)

    def test_strips_links_after_a_code_span_that_wraps(self):
        text = "Use `&'static\nstr` rather than [Testing](./testing.md)."

        self.assertEqual(subject.strip_relative_links(text), "Use `&'static\nstr` rather than Testing.")

    def test_rejects_links_that_cannot_work_in_a_guideline_file(self):
        for text in ["See ![diagram](./diagram.png).", "See [Testing][ref].", "[ref]: ./testing.md"]:
            with self.subTest(text), self.assertRaises(subject.GuidelineError):
                subject.strip_relative_links(text)


class BacktickedTest(unittest.TestCase):
    def test_reads_wrapped_spans_and_skips_fences(self):
        text = "Use `&'static\nstr` and `lib/x`.\n\n```rust\nlet a = `not/a/span`;\n```\n"

        self.assertEqual(subject.backticked(text), {"&'static str", "lib/x"})


class RenderTest(unittest.TestCase):
    def setUp(self):
        self.regions = {
            region.key: region
            for region in subject.parse_regions(
                "code-review.md",
                page(
                    """
                    ## General guidelines

                    <!-- review:preamble -->
                    You review pull requests.
                    <!-- /review -->

                    ## Hot paths

                    <!-- review:hot-paths -->
                    - Avoid allocations. See [Conventions](./conventions.md).
                    <!-- /review -->
                    """
                ),
            )
        }

    def test_renders_header_title_headings_and_bodies(self):
        entry = {
            "path": "codereview_guideline.md",
            "title": "Review guidelines",
            "sections": [
                {"region": "code-review.md#preamble"},
                {"region": "code-review.md#hot-paths", "level": 3},
            ],
        }

        rendered = subject.render_file(entry, self.regions)

        self.assertEqual(
            rendered,
            f"{subject.HEADER}\n\n## Review guidelines\n\nYou review pull requests.\n\n### Hot paths\n\n"
            "- Avoid allocations. See Conventions.\n",
        )

    def test_uses_heading_override(self):
        entry = {
            "path": "lib/example/codereview_guideline.md",
            "title": "Example review checklist",
            "sections": [{"region": "code-review.md#hot-paths", "level": 2, "heading": "Allocation"}],
        }

        rendered = subject.render_file(entry, self.regions)

        self.assertTrue(rendered.startswith(f"{subject.HEADER}\n\n## Example review checklist\n\n## Allocation\n\n"))

    def test_rejects_bad_sections(self):
        cases = {
            "unknown region": [{"region": "code-review.md#nope"}],
            "region used twice": [{"region": "code-review.md#preamble"}, {"region": "code-review.md#preamble"}],
            "bad level": [{"region": "code-review.md#preamble", "level": 1}],
            "heading without level": [{"region": "code-review.md#preamble", "heading": "Preamble"}],
            "level without any heading": [{"region": "code-review.md#preamble", "level": 2}],
        }
        regions = dict(self.regions)
        regions["code-review.md#preamble"].heading = None
        for name, sections in cases.items():
            entry = {"path": "codereview_guideline.md", "title": "Review guidelines", "sections": sections}
            with self.subTest(name), self.assertRaises(subject.GuidelineError):
                subject.render_file(entry, regions)


class RepositoryTest(unittest.TestCase):
    """Runs the commands against a scratch git repository."""

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)
        environment = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
        environment["GIT_CEILING_DIRECTORIES"] = str(self.root.parent)
        patcher = unittest.mock.patch.dict(os.environ, environment, clear=True)
        patcher.start()
        self.addCleanup(patcher.stop)
        subprocess.run(["git", "init", "--quiet", str(self.root)], check=True)

        docs = self.root / "docs" / "development"
        docs.mkdir(parents=True)
        (self.root / "lib" / "example" / "src").mkdir(parents=True)
        (self.root / "lib" / "example" / "src" / "lib.rs").write_text("pub struct RealType;\n")
        (docs / "code-review.md").write_text(
            page(
                """
                # Code review

                ## General

                <!-- review:preamble -->
                Read `lib/example`.
                <!-- /review -->

                ## Example crate

                <!-- review:example -->
                Apply these to `src/lib.rs` and `RealType`.
                <!-- /review -->
                """
            )
        )
        self.manifest = self.root / "manifest.yaml"
        self.write_manifest(
            """
            files:
              - path: codereview_guideline.md
                title: Review guidelines
                sections:
                  - region: code-review.md#preamble
              - path: lib/example/codereview_guideline.md
                title: Example review checklist
                sections:
                  - region: code-review.md#example
            """
        )
        self.arguments = SimpleNamespace(repo_root=self.root, manifest=self.manifest)

    def tearDown(self):
        self.directory.cleanup()

    def write_manifest(self, text: str) -> None:
        self.manifest.write_text(page(text))

    def edit_docs(self, old: str, new: str) -> None:
        docs = self.root / "docs/development/code-review.md"
        docs.write_text(docs.read_text().replace(old, new))

    def run_command(self, command) -> tuple[int, str]:
        stderr = io.StringIO()
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(stderr):
            status = command(self.arguments)
        return status, stderr.getvalue()

    def test_generate_then_check_passes_and_is_idempotent(self):
        self.assertEqual(self.run_command(subject.generate_command)[0], 0)
        before = (self.root / "lib/example/codereview_guideline.md").stat().st_mtime_ns

        self.assertEqual(self.run_command(subject.generate_command)[0], 0)
        status, errors = self.run_command(subject.check_command)

        self.assertEqual(status, 0, errors)
        self.assertEqual(errors, "")
        self.assertEqual((self.root / "lib/example/codereview_guideline.md").stat().st_mtime_ns, before)

    def test_check_reports_out_of_date_missing_and_carriage_return_files(self):
        self.run_command(subject.generate_command)
        root_file = self.root / "codereview_guideline.md"
        root_file.write_bytes(root_file.read_bytes().replace(b"\n", b"\r\n"))
        (self.root / "lib/example/codereview_guideline.md").unlink()

        status, errors = self.run_command(subject.check_command)

        self.assertEqual(status, 1)
        self.assertIn("codereview_guideline.md: out of date", errors)
        self.assertIn("lib/example/codereview_guideline.md: missing", errors)

    def test_rejects_guideline_files_missing_from_manifest(self):
        (self.root / "lib" / "other").mkdir()
        (self.root / "lib/other/codereview_guideline.md").write_text("Orphan.\n")

        status, errors = self.run_command(subject.generate_command)

        self.assertEqual(status, 1)
        self.assertIn("lib/other/codereview_guideline.md: this guideline file isn't in the manifest", errors)

    def test_ignores_deleted_and_ignored_guideline_files(self):
        (self.root / "lib" / "other").mkdir()
        orphan = self.root / "lib/other/codereview_guideline.md"
        orphan.write_text("Orphan.\n")
        subprocess.run(["git", "add", "-A"], cwd=self.root, check=True)
        orphan.unlink()
        (self.root / ".gitignore").write_text("scratch/\n")
        (self.root / "scratch").mkdir()
        (self.root / "scratch/codereview_guideline.md").write_text("Ignored.\n")

        status, errors = self.run_command(subject.generate_command)

        self.assertEqual(status, 0, errors)

    def test_walks_the_tree_when_git_fails(self):
        nested = self.root / "lib" / "worktree"
        (nested / ".git").mkdir(parents=True)
        (nested / "codereview_guideline.md").write_text("Inside a nested repository.\n")
        (self.root / "lib" / "other").mkdir()
        (self.root / "lib/other/codereview_guideline.md").write_text("Orphan.\n")

        with unittest.mock.patch.object(subject.subprocess, "run", side_effect=OSError("no git")):
            status, errors = self.run_command(subject.generate_command)

        self.assertEqual(status, 1)
        self.assertIn("notice: git can't list the repository files", errors)
        self.assertIn("lib/other/codereview_guideline.md: this guideline file isn't in the manifest", errors)
        self.assertNotIn("lib/worktree", errors)

    def test_checks_repository_and_scoped_paths(self):
        self.edit_docs("Read `lib/example`.", "Read `lib/missing/file.rs`, `lib/example/src/lib.rs:10`, and `lib/saluki-*`.")
        self.edit_docs("Apply these to `src/lib.rs`", "Apply these to `src/missing.rs`, `docs/development/code-review.md#general`")

        status, errors = self.run_command(subject.generate_command)

        self.assertEqual(status, 1)
        self.assertIn("codereview_guideline.md: `lib/missing/file.rs` doesn't exist", errors)
        self.assertIn("lib/example/codereview_guideline.md: `src/missing.rs` doesn't exist", errors)
        self.assertNotIn("lib.rs:10", errors)
        self.assertNotIn("lib/saluki-*", errors)
        self.assertNotIn("code-review.md#general", errors)

    def test_warns_about_unknown_rust_names_unless_ignored(self):
        self.run_command(subject.generate_command)
        subprocess.run(["git", "add", "-A"], cwd=self.root, check=True)
        self.edit_docs("and `RealType`.", "and `RealType`, `MissingType`, and `IgnoredType`.")
        self.write_manifest(self.manifest.read_text() + "ignore_names:\n  - IgnoredType\n")
        self.run_command(subject.generate_command)

        status, errors = self.run_command(subject.check_command)

        self.assertEqual(status, 0, errors)
        self.assertIn("`MissingType` doesn't appear under lib/ or bin/", errors)
        self.assertNotIn("RealType", errors)
        self.assertNotIn("IgnoredType", errors)

    def test_rejects_unused_regions(self):
        self.write_manifest(
            """
            files:
              - path: codereview_guideline.md
                title: Review guidelines
                sections:
                  - region: code-review.md#preamble
            """
        )

        with self.assertRaisesRegex(subject.GuidelineError, "code-review.md#example"):
            subject.render_all(self.root, subject.load_manifest(self.manifest))

    def test_rejects_bad_manifests(self):
        entry = "  - path: codereview_guideline.md\n    title: Review guidelines\n    sections:\n      - region: code-review.md#preamble\n"
        cases = {
            "unknown top-level key": "files: []\nextra: 1\n",
            "unknown file key": entry.replace("    title:", "    titel: x\n    title:"),
            "unknown section key": entry + "        levle: 3\n",
            "section as a string": entry.replace("      - region: code-review.md#preamble", "      - code-review.md#preamble"),
            "missing title": entry.replace("    title: Review guidelines\n", ""),
            "path outside the repository": entry.replace("path: codereview_guideline.md", "path: ../codereview_guideline.md"),
            "absolute path": entry.replace("path: codereview_guideline.md", "path: /tmp/codereview_guideline.md"),
            "wrong file name": entry.replace("path: codereview_guideline.md", "path: README.md"),
        }
        for name, files in cases.items():
            text = files if name == "unknown top-level key" else "files:\n" + files
            self.manifest.write_text(text)
            with self.subTest(name), self.assertRaises(subject.GuidelineError):
                subject.load_manifest(self.manifest)

    def test_rejects_duplicate_paths_and_missing_directories(self):
        cases = {
            "duplicate path": """
                files:
                  - path: codereview_guideline.md
                    title: A
                    sections:
                      - region: code-review.md#preamble
                  - path: codereview_guideline.md
                    title: B
                    sections:
                      - region: code-review.md#example
                """,
            "missing directory": """
                files:
                  - path: codereview_guideline.md
                    title: A
                    sections:
                      - region: code-review.md#preamble
                  - path: lib/nowhere/codereview_guideline.md
                    title: B
                    sections:
                      - region: code-review.md#example
                """,
        }
        for name, text in cases.items():
            self.write_manifest(textwrap.dedent(text))
            with self.subTest(name), self.assertRaises(subject.GuidelineError):
                subject.render_all(self.root, subject.load_manifest(self.manifest))


if __name__ == "__main__":
    unittest.main()
