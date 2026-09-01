import contextlib
import importlib.util
import io
import subprocess
import sys
import tempfile
import unittest
import unittest.mock
from pathlib import Path

import yaml


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

    def test_rejects_markdown_where_reno_renders_restructuredtext(self):
        markdown_entries = (
            "See [the docs](https://example.test) for details.",
            "Set `dogstatsd_port` to 8125.",
            "This is __important__ to note.",
            "# Heading",
            "```yaml",
            "> Quoted text.",
            "![diagram](https://example.test/diagram.png)",
            "Point it at `<host>:8125` to send metrics.",
            "Set `value > 0` to enable the check.",
        )
        for entry in markdown_entries:
            with self.subTest(entry=entry):
                path = self.write_note("fix-listener-0123456789abcdef.yaml", f"fixes:\n  - |\n    {entry}\n")

                self.assertTrue(any("uses Markdown" in error for error in subject.validate_note_file(path)))

    def test_accepts_restructuredtext_markup(self):
        entries = (
            "Set ``dogstatsd_port`` to 8125.",
            "See `the docs <https://example.test>`_ for details.",
            "This is **important** to note.",
            "Rename metric_name_prefix to metric_prefix.",
            "Set :code:`dogstatsd_port` to 8125.",
            "The ``foo__bar__baz`` identifier is unchanged.",
            "Use ``[a](b)`` to spell a Markdown link.",
            "See `the docs <https://example.test>`__ for details.",
        )
        for entry in entries:
            with self.subTest(entry=entry):
                path = self.write_note("fix-listener-0123456789abcdef.yaml", f"fixes:\n  - |\n    {entry}\n")

                self.assertEqual(subject.validate_note_file(path), [])

    def test_rejects_notes_sharing_a_reno_unique_identifier(self):
        notes = [
            self.write_note("fix-listener-0123456789abcdef.yaml", "fixes:\n  - First.\n"),
            self.write_note("fix-forwarder-0123456789abcdef.yaml", "fixes:\n  - Second.\n"),
        ]

        errors = subject.find_duplicate_note_ids(notes)

        self.assertEqual(len(errors), 1)
        self.assertIn("0123456789abcdef", errors[0])
        self.assertIn("fix-forwarder-0123456789abcdef.yaml", errors[0])
        self.assertIn("fix-listener-0123456789abcdef.yaml", errors[0])

    def test_accepts_notes_with_distinct_reno_unique_identifiers(self):
        notes = [
            self.write_note("fix-listener-0123456789abcdef.yaml", "fixes:\n  - First.\n"),
            self.write_note("fix-forwarder-fedcba9876543210.yaml", "fixes:\n  - Second.\n"),
        ]

        self.assertEqual(subject.find_duplicate_note_ids(notes), [])


class ReleaseNoteRenderingTest(unittest.TestCase):
    def setUp(self):
        self.temporary_directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary_directory.cleanup)
        self.repository = Path(self.temporary_directory.name)
        self.real_run = subprocess.run
        self.run_git("init", "--quiet")
        self.run_git("config", "user.name", "DADP test")
        self.run_git("config", "user.email", "dadp-test@example.invalid")
        self.run_git("symbolic-ref", "HEAD", "refs/heads/main")
        (self.repository / "releasenotes/notes").mkdir(parents=True)
        (self.repository / "releasenotes/config.yaml").write_text(
            (MODULE_PATH.parents[2] / "releasenotes/config.yaml").read_text(encoding="utf-8"), encoding="utf-8"
        )

    def run_git(self, *arguments, input=None):
        return self.real_run(
            ["git", *arguments], cwd=self.repository, input=input, capture_output=True, check=True, text=True
        )

    def commit(self, message, parent=None):
        self.run_git("add", "--all")
        tree = self.run_git("write-tree").stdout.strip()
        arguments = ["commit-tree", tree]
        if parent:
            arguments.extend(("-p", parent))
        commit = self.run_git(*arguments, input=f"{message}\n").stdout.strip()
        self.run_git("update-ref", "refs/heads/main", commit)
        return commit

    def add_note(self, version, name, contents, parent):
        (self.repository / "releasenotes/notes" / name).write_text(contents, encoding="utf-8")
        commit = self.commit(f"Add notes for {version}", parent)
        self.run_git("update-ref", f"refs/tags/{version}", commit)
        return commit

    def test_rejects_non_release_tags_before_running_commands(self):
        with unittest.mock.patch.object(subject.subprocess, "run") as run:
            with self.assertRaisesRegex(ValueError, "X.Y.Z"):
                subject.render_release_notes("v1.6.0", Path.cwd())
        run.assert_not_called()

    def test_rejects_tags_without_release_note_configuration(self):
        pre_adoption_directory = tempfile.TemporaryDirectory(dir=self.temporary_directory.name)
        self.addCleanup(pre_adoption_directory.cleanup)
        pre_adoption_repository = Path(pre_adoption_directory.name)
        self.real_run(["git", "init", "--quiet"], cwd=pre_adoption_repository, check=True)
        self.real_run(["git", "config", "user.name", "DADP test"], cwd=pre_adoption_repository, check=True)
        self.real_run(["git", "config", "user.email", "dadp-test@example.invalid"], cwd=pre_adoption_repository, check=True)
        self.real_run(["git", "symbolic-ref", "HEAD", "refs/heads/main"], cwd=pre_adoption_repository, check=True)
        tree = self.real_run(["git", "write-tree"], cwd=pre_adoption_repository, capture_output=True, check=True, text=True).stdout.strip()
        commit = self.real_run(
            ["git", "commit-tree", tree], cwd=pre_adoption_repository, input="Initial release\n", capture_output=True, check=True, text=True
        ).stdout.strip()
        self.real_run(["git", "update-ref", "refs/heads/main", commit], cwd=pre_adoption_repository, check=True)
        self.real_run(["git", "update-ref", "refs/tags/9.9.9", commit], cwd=pre_adoption_repository, check=True)

        with self.assertRaisesRegex(RuntimeError, "does not contain releasenotes/config.yaml"):
            subject.render_release_notes("9.9.9", pre_adoption_repository)

        command = [
            sys.executable,
            str(MODULE_PATH),
            "render",
            "--version",
            "9.9.9",
            "--repository",
            str(pre_adoption_repository),
            "--output",
            str(pre_adoption_repository / "notes.md"),
        ]
        result = self.real_run(command, capture_output=True, text=True)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("does not contain releasenotes/config.yaml", result.stderr)
        self.assertNotIn("Traceback", result.stderr)

    def test_returns_empty_render_for_a_tag_without_new_notes(self):
        first_release = self.commit("Add release note configuration")
        self.add_note("9.9.8", "first-0123456789abcdef.yaml", "fixes:\n  - First release note.\n", first_release)
        second_release = self.commit("Change without a release note", self.run_git("rev-parse", "HEAD").stdout.strip())
        self.run_git("update-ref", "refs/tags/9.9.9", second_release)

        self.assertEqual(subject.render_release_notes("9.9.9", self.repository), "")

    def test_renders_only_notes_belonging_to_the_requested_tag(self):
        initial_commit = self.commit("Add release note configuration")
        first_release = self.add_note("9.9.8", "first-0123456789abcdef.yaml", "fixes:\n  - First release note.\n", initial_commit)
        self.add_note("9.9.9", "second-fedcba9876543210.yaml", "fixes:\n  - Second release note.\n", first_release)

        def run_except_pandoc(command, **kwargs):
            if command[0] == "pandoc":
                self.assertIn("First release note.", kwargs["input"])
                self.assertNotIn("Second release note.", kwargs["input"])
                return subprocess.CompletedProcess(command, 0, "## Bug Fixes\n\n- First release note.\n", "")
            return self.real_run(command, **kwargs)

        with unittest.mock.patch.object(subject.subprocess, "run", side_effect=run_except_pandoc):
            rendered = subject.render_release_notes("9.9.8", self.repository)

        self.assertEqual(rendered, "## Bug Fixes\n\n- First release note.\n")

    def test_includes_subprocess_stderr_in_render_errors(self):
        errors = (
            ([subprocess.CalledProcessError(1, ["git"], stderr="tag lookup failed")], "tag lookup failed"),
            (
                [
                    unittest.mock.Mock(stdout="", stderr=""),
                    unittest.mock.Mock(stdout="", stderr=""),
                    subprocess.CalledProcessError(1, ["reno"], stderr="reno report failed"),
                ],
                "reno report failed",
            ),
            (
                [
                    unittest.mock.Mock(stdout="", stderr=""),
                    unittest.mock.Mock(stdout="", stderr=""),
                    unittest.mock.Mock(stdout="Release notes\n=============\n", stderr=""),
                    subprocess.CalledProcessError(1, ["pandoc"], stderr="pandoc conversion failed"),
                ],
                "pandoc conversion failed",
            ),
        )
        for completed, error_text in errors:
            with self.subTest(error_text=error_text):
                with unittest.mock.patch.object(subject.subprocess, "run", side_effect=completed):
                    with self.assertRaisesRegex(RuntimeError, error_text):
                        subject.render_release_notes("1.6.0", Path("/repo"))

    def test_prefers_reno_found_on_the_path(self):
        completed = [
            unittest.mock.Mock(stdout="", stderr=""),
            unittest.mock.Mock(stdout="", stderr=""),
            unittest.mock.Mock(stdout="", stderr=""),
        ]
        with unittest.mock.patch.object(subject.shutil, "which", return_value="/home/runner/.local/bin/reno"):
            with unittest.mock.patch.object(subject.subprocess, "run", side_effect=completed) as run:
                self.assertEqual(subject.render_release_notes("1.6.0", Path("/repo")), "")

        self.assertEqual(run.call_args_list[2].args[0][0], "/home/runner/.local/bin/reno")

    def test_writes_render_to_standard_output_for_dash_output(self):
        arguments = unittest.mock.Mock(version="1.6.0", repository=Path("/repo"), output=Path("-"))
        with unittest.mock.patch.object(subject, "render_release_notes", return_value="## Bug Fixes\n- Fix\n"):
            with contextlib.redirect_stdout(io.StringIO()) as standard_output:
                subject.render_command(arguments)

        self.assertEqual(standard_output.getvalue(), "## Bug Fixes\n- Fix\n")


class ReleaseWorkflowContractTest(unittest.TestCase):
    def test_release_workflow_limits_write_credentials_to_trusted_release_contexts(self):
        workflow_path = MODULE_PATH.parents[2] / ".github/workflows/release-notes.yml"
        workflow = yaml.load(workflow_path.read_text(encoding="utf-8"), Loader=yaml.BaseLoader)

        validation_paths = workflow["on"]["pull_request"]["paths"]
        self.assertIn("releasenotes/**", validation_paths)
        self.assertIn(".github/workflows/release-notes.yml", validation_paths)
        self.assertIn(".github/chainguard/self.release-notes.*.sts.yaml", validation_paths)
        self.assertEqual(workflow["on"]["release"]["types"], ["published"])
        self.assertEqual(workflow["on"]["workflow_dispatch"]["inputs"]["tag"]["required"], "true")
        validate = workflow["jobs"]["validate"]
        validate_checkout = next(step for step in validate["steps"] if step.get("uses", "").startswith("actions/checkout@"))
        self.assertEqual(validate_checkout["with"]["fetch-depth"], "0")
        publish = workflow["jobs"]["publish"]
        self.assertIn("github.ref == 'refs/heads/main'", publish["if"])
        self.assertEqual(publish["permissions"]["contents"], "read")
        source_checkout = next(
            step
            for step in publish["steps"]
            if step.get("uses", "").startswith("actions/checkout@") and step.get("with", {}).get("ref") == "main"
        )
        tag_checkout = next(
            step
            for step in publish["steps"]
            if step.get("uses", "").startswith("actions/checkout@")
            and step.get("with", {}).get("ref") == "${{ env.RELEASE_TAG }}"
        )
        self.assertEqual(source_checkout["with"]["token"], "${{ steps.octo-sts.outputs.token }}")
        self.assertEqual(tag_checkout["with"]["token"], "${{ steps.octo-sts.outputs.token }}")
        self.assertEqual(tag_checkout["with"]["path"], "release-tag")
        scripts = "\n".join(step.get("with", {}).get("script", "") for step in publish["steps"])
        self.assertIn("getReleaseByTag", scripts)
        self.assertIn("updateRelease", scripts)

        publish_policy = yaml.load(
            (MODULE_PATH.parents[2] / ".github/chainguard/self.release-notes.publish.sts.yaml").read_text(encoding="utf-8"),
            Loader=yaml.BaseLoader,
        )
        repair_policy = yaml.load(
            (MODULE_PATH.parents[2] / ".github/chainguard/self.release-notes.repair.sts.yaml").read_text(encoding="utf-8"),
            Loader=yaml.BaseLoader,
        )
        self.assertEqual(publish_policy["claim_pattern"]["event_name"], "release")
        self.assertEqual(publish_policy["claim_pattern"]["ref"], "refs/tags/[0-9]+\\.[0-9]+\\.[0-9]+")
        self.assertEqual(repair_policy["claim_pattern"]["event_name"], "workflow_dispatch")
        self.assertEqual(repair_policy["claim_pattern"]["ref"], "refs/heads/main")


class ReleaseNoteDecisionWorkflowTest(unittest.TestCase):
    def test_requires_a_fragment_or_no_changelog_label_for_every_pull_request(self):
        workflow_path = MODULE_PATH.parents[2] / ".github/workflows/release-note-check.yml"
        workflow = yaml.load(workflow_path.read_text(encoding="utf-8"), Loader=yaml.BaseLoader)

        self.assertEqual(
            workflow["on"]["pull_request"]["types"], ["opened", "reopened", "synchronize", "labeled", "unlabeled"]
        )
        job = workflow["jobs"]["release-note-check"]
        self.assertEqual(job["permissions"]["pull-requests"], "read")
        script = job["steps"][0]["with"]["script"]
        self.assertIn("github.rest.pulls.listFiles", script)
        self.assertIn("releasenotes/notes/", script)
        self.assertIn("changelog/no-changelog", script)
        self.assertIn("core.setFailed", script)


if __name__ == "__main__":
    unittest.main()
