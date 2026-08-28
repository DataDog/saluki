# Saluki release notes implementation plan

> **For automation workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add opt-in, categorized Reno release notes to Saluki and automatically prepend their rendered content to each published GitHub release while retaining GitHub's complete merged-pull-request list.

**Architecture:** A Python helper owns tag validation, note validation, Reno/`Pandoc` rendering, and idempotent release-body merging. A narrow unprivileged pull-request job runs the helper only for release-note changes. A separately credentialed release job checks out the published tag, renders its note set, and updates only the generated block in the existing GitHub release body.

**Tech Stack:** Python 3 standard library, PyYAML, Reno 4.1.0, `Pandoc`, GNU Make, GitHub Actions, `dd-octo-sts-action`, GitHub Releases API.

---

## File structure

| Path | Responsibility |
| --- | --- |
| `releasenotes/config.yaml` | Reno configuration, `X.Y.Z` tag recognition, contributor template, and ordered categories. |
| `ci/tooling/release_notes.py` | CLI and pure functions for note validation, Reno rendering, Markdown conversion, marker handling, and release-body merging. |
| `ci/tooling/test_release_notes.py` | Standard-library unit and subprocess tests for the release-note helper. |
| `requirements.txt` | Pin Reno with the existing Python tooling dependencies. |
| `Makefile` | Local `check-release-notes`, `test-release-notes`, and `render-release-notes` entry points. |
| `.github/workflows/release-notes.yml` | Note-only pull-request validation plus published-release and manual-repair jobs. |
| `.github/chainguard/self.release-notes.sts.yaml` | Least-privilege OIDC policy for the release-body updater. |
| `docs/development/contributing.md` | Optional contributor workflow and customer-facing note-writing guidance. |
| `docs/agent-data-plane/releasing.md` | Release-manager checkpoint after GitHub Release publication. |

## Task 1: Define and test the pure release-body merge contract

**Files:**
- Create: `ci/tooling/test_release_notes.py`
- Create: `ci/tooling/release_notes.py`

- [ ] **Step 1: Write failing unit tests for tag recognition and body merging**

  Create `ci/tooling/test_release_notes.py` using `unittest` and load `release_notes.py` through `importlib.util`. Define fixtures for a GitHub-generated body and a rendered curated body. Cover these cases:

  ```python
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
  ```

  Add a malformed-marker test that asserts a `ValueError` when exactly one marker is present or the end marker precedes the start marker. This prevents a repair run from accidentally deleting unbounded release content.

- [ ] **Step 2: Run the focused unit test to verify it fails**

  Run:

  ```bash
  python3 -m unittest ci/tooling/test_release_notes.py -v
  ```

  Expected: FAIL because `ci/tooling/release_notes.py` does not yet exist.

- [ ] **Step 3: Implement the pure merge API and CLI skeleton**

  Create `ci/tooling/release_notes.py` with these public constants and functions:

  ```python
  RELEASE_TAG_RE = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+$")
  START_MARKER = "<!-- saluki-curated-notes:start -->"
  END_MARKER = "<!-- saluki-curated-notes:end -->"

  def is_release_tag(version: str) -> bool:
      return bool(RELEASE_TAG_RE.fullmatch(version))

  def build_curated_block(markdown: str) -> str:
      if not markdown.strip():
          return ""
      return "\n".join((START_MARKER, "# Release notes", "", markdown.strip(), END_MARKER))

  def merge_release_body(existing: str, markdown: str) -> str:
      block = build_curated_block(markdown)
      if not block:
          return existing
      start = existing.find(START_MARKER)
      end = existing.find(END_MARKER)
      if (start == -1) != (end == -1) or (end != -1 and end < start):
          raise ValueError("release body contains malformed curated-release-note markers")
      if start != -1:
          end += len(END_MARKER)
          return existing[:start] + block + existing[end:]
      return block + "\n\n" + existing if existing else block
  ```

  Add an `argparse` parser with `check`, `render`, and `merge` subcommands. Implement `merge` now: read `--release-body-file` and `--notes-file`, write `--output`, and print `has_notes=true` or `has_notes=false` in GitHub Actions output format when `GITHUB_OUTPUT` is set.

- [ ] **Step 4: Run the focused test to verify it passes**

  Run:

  ```bash
  python3 -m unittest ci/tooling/test_release_notes.py -v
  ```

  Expected: PASS with the tag, prepend, replace, empty, and malformed-marker cases green.

- [ ] **Step 5: Commit the pure merge contract**

  ```bash
  git add ci/tooling/release_notes.py ci/tooling/test_release_notes.py
  git commit -m "feat(release): add release-note body merger" \
    -m "Define and test the marker-bounded merge operation used to prepend curated notes without altering GitHub's generated PR list."
  ```

## Task 2: Add Reno configuration and strict opt-in note validation

**Files:**
- Create: `releasenotes/config.yaml`
- Modify: `ci/tooling/release_notes.py`
- Modify: `ci/tooling/test_release_notes.py`

- [ ] **Step 1: Add failing validation tests**

  Extend `ci/tooling/test_release_notes.py` with temporary release-note directories and tests for valid notes, malformed YAML, a non-mapping document, an unknown category, an empty list item, and filenames that do not end in a lowercase 16-hex-character Reno identifier.

  The valid fixture must be named `fix-listener-0123456789abcdef.yaml` and contain:

  ```yaml
  fixes:
    - |
      Fix a listener shutdown race that could drop telemetry during process exit.
  ```

  Assert that `validate_note_file(path)` returns an empty list for the valid fixture and error messages for every invalid fixture.

- [ ] **Step 2: Run the focused validation tests to verify they fail**

  Run:

  ```bash
  python3 -m unittest ci/tooling/test_release_notes.py -v
  ```

  Expected: FAIL because `validate_note_file` and its category contract do not exist.

- [ ] **Step 3: Add the Reno configuration**

  Create `releasenotes/config.yaml` with `default_branch: main`, `collapse_pre_releases: true`, and this strict release-tag expression:

  ```yaml
  release_tag_re: '^\d+\.\d+\.\d+$'
  ```

  Configure the Agent-compatible section order:

  ```yaml
  sections:
    - [upgrade, Upgrade Notes]
    - [features, New Features]
    - [enhancements, Enhancement Notes]
    - [issues, Known Issues]
    - [deprecations, Deprecation Notes]
    - [security, Security Notes]
    - [fixes, Bug Fixes]
    - [other, Other Notes]
  ```

  Configure Reno's template so every section starts as a list-valued YAML key with customer-facing, self-contained RST instructions. It must state that contributors remove unused sections and must not add a `prelude` section in this initial workflow.

- [ ] **Step 4: Implement strict note validation**

  In `ci/tooling/release_notes.py`, add:

  ```python
  CATEGORY_ORDER = ("upgrade", "features", "enhancements", "issues", "deprecations", "security", "fixes", "other")
  RENO_FILENAME_RE = re.compile(r"^.+-[0-9a-f]{16}\.yaml$")

  def validate_note_file(path: Path) -> list[str]:
      errors = []
      if not RENO_FILENAME_RE.fullmatch(path.name):
          errors.append(f"{path}: filename must end in -<16 lowercase hex characters>.yaml")
      try:
          content = yaml.safe_load(path.read_text(encoding="utf-8"))
      except yaml.YAMLError as error:
          return [*errors, f"{path}: invalid YAML: {error}"]
      if not isinstance(content, dict) or not content:
          return [*errors, f"{path}: release note must be a non-empty YAML mapping"]
      for category, entries in content.items():
          if category not in CATEGORY_ORDER:
              errors.append(f"{path}: unsupported release-note category {category!r}")
          elif not isinstance(entries, list) or not entries or any(not isinstance(entry, str) or not entry.strip() for entry in entries):
              errors.append(f"{path}: {category!r} must be a non-empty list of non-empty strings")
      return errors
  ```

  Make the `check` subcommand validate each supplied file, print all failures to stderr, return `1` when any validation fails, and return `0` when no files are supplied or all supplied notes are valid.

- [ ] **Step 5: Run the validation and Reno checks**

  Run:

  ```bash
  make ensure-python-venv
  .venv/bin/python -m unittest ci/tooling/test_release_notes.py -v
  .venv/bin/reno lint
  ```

  Expected: the unit suite passes and `reno lint` accepts the new repository configuration with no note files yet present.

- [ ] **Step 6: Commit configuration and validation**

  ```bash
  git add releasenotes/config.yaml ci/tooling/release_notes.py ci/tooling/test_release_notes.py
  git commit -m "feat(release): validate opt-in Reno notes" \
    -m "Add Saluki's Agent-compatible Reno categories and validate only the note files contributors choose to add."
  ```

## Task 3: Render an exact tag's notes into GitHub-flavored Markdown

**Files:**
- Modify: `ci/tooling/release_notes.py`
- Modify: `ci/tooling/test_release_notes.py`
- Modify: `requirements.txt`
- Modify: `Makefile`

- [ ] **Step 1: Add failing renderer tests**

  Add tests that mock `subprocess.run` and assert that `render --version 1.6.0`:

  - rejects `v1.6.0` before invoking a subprocess;
  - verifies `refs/tags/1.6.0` before rendering;
  - invokes Reno with `report`, `--ignore-cache`, `--no-show-source`, and `--version 1.6.0`;
  - invokes `Pandoc` with `--from rst`, `--to gfm`, and `--wrap=none` when Reno emits content;
  - writes empty output and reports `has_notes=false` when Reno produces only whitespace; and
  - raises a clear exception containing the subprocess stderr when Reno or `Pandoc` fails.

- [ ] **Step 2: Run the renderer tests to verify they fail**

  Run:

  ```bash
  .venv/bin/python -m unittest ci/tooling/test_release_notes.py -v
  ```

  Expected: FAIL because the renderer functions and `render` command behavior are not yet implemented.

- [ ] **Step 3: Implement the renderer and local Make targets**

  Add a `render_release_notes(version: str, repository: Path) -> str` function that uses these commands in the repository root:

  ```text
  git rev-parse --verify refs/tags/<version>
  reno report --ignore-cache --no-show-source --version <version>
  pandoc --from rst --to gfm --wrap=none
  ```

  Invoke each external command with `subprocess.run(command, cwd=repository, check=True, capture_output=True, text=True)` and preserve stderr in raised errors. The `render` subcommand writes the Markdown to `--output`, returns success for an empty report, and reports `has_notes` through `GITHUB_OUTPUT` when present.

  Add `reno==4.1.0` to `requirements.txt`. Add these Make targets after the existing Python-tooling variable definitions:

  ```make
  .PHONY: test-release-notes
  test-release-notes: ## Runs unit tests for release-note tooling
	@$(PYTHON) -m unittest ci/tooling/test_release_notes.py -v

  .PHONY: check-release-notes
  check-release-notes: ## Validates all committed Reno release-note files
	@$(PYTHON) ci/tooling/release_notes.py check releasenotes/notes/*.yaml

  .PHONY: render-release-notes
  render-release-notes: ## Renders VERSION=X.Y.Z Reno notes as GitHub-flavored Markdown
	@test -n "$(VERSION)" || { echo "Set VERSION=X.Y.Z" >&2; exit 2; }
	@$(PYTHON) ci/tooling/release_notes.py render --version "$(VERSION)" --output -
  ```

  Have `check-release-notes` use Python's `glob` expansion inside the helper rather than relying on a shell glob that stays literal when the repository has no notes.

- [ ] **Step 4: Run renderer tests and a real empty-corpus render**

  Run:

  ```bash
  make ensure-python-venv
  make test-release-notes
  make check-release-notes
  make render-release-notes VERSION=1.5.2
  ```

  Expected: all commands exit `0`; the renderer produces no curated note text because this change does not add historical note entries.

- [ ] **Step 5: Commit rendering support**

  ```bash
  git add requirements.txt Makefile ci/tooling/release_notes.py ci/tooling/test_release_notes.py
  git commit -m "feat(release): render Reno notes for GitHub releases" \
    -m "Render note files belonging to an exact Saluki tag and expose reproducible local validation and preview commands."
  ```

## Task 4: Add narrow validation and privileged release-publication automation

**Files:**
- Create: `.github/workflows/release-notes.yml`
- Create: `.github/chainguard/self.release-notes.sts.yaml`
- Modify: `ci/tooling/test_release_notes.py`

- [ ] **Step 1: Add a workflow contract test**

  Add a unit test that reads `.github/workflows/release-notes.yml` as text and asserts all of the following are present:

  - a `pull_request` path filter for `releasenotes/notes/**`;
  - `release` type `published`;
  - `workflow_dispatch` with a required `tag` input;
  - checkout of `RELEASE_TAG` rather than branch head in the publish job;
  - `make test-release-notes` and `make check-release-notes` in the validation job;
  - use of `DataDog/dd-octo-sts-action` and `actions/github-script` in the publish job; and
  - a read of the existing release body before an update request.

  This test guards the opt-in scope and exact-tag boundary without attempting to emulate GitHub Actions.

- [ ] **Step 2: Run the workflow contract test to verify it fails**

  Run:

  ```bash
  make test-release-notes
  ```

  Expected: FAIL because `.github/workflows/release-notes.yml` does not yet exist.

- [ ] **Step 3: Create the GitHub Actions workflow**

  Create `.github/workflows/release-notes.yml` with three triggers:

  ```yaml
  on:
    pull_request:
      paths:
        - 'releasenotes/notes/**'
        - 'releasenotes/config.yaml'
        - 'ci/tooling/release_notes.py'
        - 'ci/tooling/test_release_notes.py'
        - 'requirements.txt'
        - 'Makefile'
        - '.github/workflows/release-notes.yml'
    release:
      types: [published]
    workflow_dispatch:
      inputs:
        tag:
          description: Published X.Y.Z release tag to repair
          required: true
          type: string
  ```

  The pull-request `validate-notes` job uses `actions/checkout`, `actions/setup-python@ece7cb06caefa5fff74198d8649806c4678c61a1` (the `v6` tag), installs `requirements.txt`, and runs `make test-release-notes`, `make check-release-notes`, and `reno lint`. It has only `contents: read` permission and does not inspect labels.

  The `publish-curated-notes` job runs only for `release` and `workflow_dispatch` events. It requests `id-token: write`, receives a `contents: write` token from `dd-octo-sts-action`, sets `RELEASE_TAG` to `github.event.release.tag_name || inputs.tag`, and checks out `RELEASE_TAG` with `fetch-depth: 0`. It installs the pinned Python requirements and `Pandoc` with `sudo apt-get update && sudo apt-get install --yes pandoc`.

  Use a first `actions/github-script` step to obtain the release object. For a `release` event use `context.payload.release`; for manual dispatch call `github.rest.repos.getReleaseByTag`. Write `release.body ?? ""` to `$RUNNER_TEMP/release-body.md` and expose `release.id` as `release_id`.

  Render and merge with:

  ```bash
  python ci/tooling/release_notes.py render \
    --version "$RELEASE_TAG" \
    --output "$RUNNER_TEMP/curated-notes.md"
  python ci/tooling/release_notes.py merge \
    --notes-file "$RUNNER_TEMP/curated-notes.md" \
    --release-body-file "$RUNNER_TEMP/release-body.md" \
    --output "$RUNNER_TEMP/updated-release-body.md"
  ```

  Gate the final `actions/github-script` update on the helper's `has_notes` output. Its only API mutation is:

  ```javascript
  await github.rest.repos.updateRelease({
    owner: context.repo.owner,
    repo: context.repo.repo,
    release_id: Number(process.env.RELEASE_ID),
    body: fs.readFileSync(process.env.RELEASE_BODY, "utf8"),
  });
  ```

  Do not update a release when there are no curated notes. Any retrieval, render, merge, or API error must fail the job before this update step.

- [ ] **Step 4: Create the credential policy**

  Create `.github/chainguard/self.release-notes.sts.yaml` with the repository issuer, a `subject_pattern` that permits only `refs/heads/main` manual dispatches and exact numeric release tags, and a `claim_pattern` that permits only `release` and `workflow_dispatch` for `.github/workflows/release-notes.yml`. Grant only:

  ```yaml
  permissions:
    contents: write
  ```

  Follow the field layout used by the existing `self.bump-adp-version.create-pr.sts.yaml` and `self.docs.sts.yaml` policies. The checked-in claim expressions must admit `refs/heads/main` for manual repair and `refs/tags/X.Y.Z` for release publication; no pull-request event or arbitrary branch may receive this token.

- [ ] **Step 5: Run the workflow contract and local release-note suite**

  Run:

  ```bash
  make test-release-notes
  make check-release-notes
  .venv/bin/reno lint
  ```

  Expected: PASS. Inspect the staged workflow to confirm that only release publication or an explicit manual tag can request a write token and that the pull-request job has no write credential.

- [ ] **Step 6: Commit release automation**

  ```bash
  git add .github/workflows/release-notes.yml .github/chainguard/self.release-notes.sts.yaml ci/tooling/test_release_notes.py
  git commit -m "feat(release): publish curated notes on release" \
    -m "Prepend idempotently rendered Reno notes to published GitHub releases while retaining GitHub's generated pull-request history."
  ```

## Task 5: Document optional contributor and release-manager workflows

**Files:**
- Modify: `docs/development/contributing.md`
- Modify: `docs/agent-data-plane/releasing.md`

- [ ] **Step 1: Add documentation-checking expectations**

  Before editing prose, identify the new content that `make check-docs` must accept:

  - the contributor page says release notes are optional rather than required;
  - it includes a minimal `reno new` command and categorized YAML example;
  - it states that notes describe customer-visible behavior and use self-contained RST;
  - the release instructions keep GitHub's generated notes and require the curated workflow to pass before the announcement and artifact publication.

- [ ] **Step 2: Update the contributor guide**

  Add a `### Release notes` subsection after the pull-request workflow material in `docs/development/contributing.md`. Include:

  ```shell
  make ensure-python-venv
  .venv/bin/reno new describe-the-change --edit
  ```

  State explicitly that contributors add a note only when a change has customer-visible release information; no label or approval rule determines that choice. Include the `fixes` YAML example from Task 2, tell contributors to remove unused template sections, and provide local validation:

  ```shell
  make check-release-notes
  ```

- [ ] **Step 3: Update the ADP release instructions**

  In `docs/agent-data-plane/releasing.md`, retain the existing GitHub **Generate release notes** step. Add the next step: after publishing, open the `Release notes` workflow run for the tag and confirm it succeeded before proceeding to the GitLab tag pipeline, manual artifact-publish jobs, or Slack announcement. Explain that the workflow places curated notes at the top and preserves GitHub's `What's Changed` list below them.

- [ ] **Step 4: Run documentation and release-note checks**

  Run:

  ```bash
  make check-docs
  make test-release-notes
  make check-release-notes
  .venv/bin/reno lint
  git diff --check
  ```

  Expected: every command exits `0`.

- [ ] **Step 5: Commit the user-facing workflow**

  ```bash
  git add docs/development/contributing.md docs/agent-data-plane/releasing.md
  git commit -m "docs(release): explain optional Saluki release notes" \
    -m "Document how contributors add customer-facing Reno notes and how releasers verify the generated GitHub release section."
  ```

## Task 6: Perform final repository verification and prepare review

**Files:**
- Verify: all files changed by Tasks 1–5

- [ ] **Step 1: Run all release-note-specific checks from a clean working tree**

  Run:

  ```bash
  make ensure-python-venv
  make test-release-notes
  make check-release-notes
  .venv/bin/reno lint
  make check-docs
  git diff --check
  git status --short
  ```

  Expected: all checks exit `0` and `git status --short` reports no uncommitted changes.

- [ ] **Step 2: Review the behavior against the approved design**

  Confirm these observable requirements from `docs/superpowers/specs/2026-08-28-saluki-release-notes-design.md`:

  - a pull request without a note file has no new changelog requirement;
  - a pull request adding a malformed note fails only the narrow validation job;
  - a published `X.Y.Z` release with notes receives one marked curated block before the old body;
  - a published `X.Y.Z` release without notes retains its old body unchanged;
  - a manual rerun replaces an existing marked block and preserves GitHub's PR list;
  - no Datadog Agent repository code or release note is changed.

- [ ] **Step 3: Inspect commits and working tree before review**

  Run:

  ```bash
  git log --oneline origin/main..HEAD
  git diff --stat origin/main...HEAD
  git status --short --branch
  ```

  Expected: only the focused release-note commits are present, each has a meaningful subject and body, and the working tree is clean.

- [ ] **Step 4: Request code review**

  Ask a reviewer to inspect the helper's marker safety, the release workflow's tag checkout and credential policy, the absence of missing-note enforcement, and the documentation's updated release checkpoint.
