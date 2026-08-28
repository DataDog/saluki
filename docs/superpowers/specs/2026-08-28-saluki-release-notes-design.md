# Saluki release notes design

## Purpose

DADP-144 needs a customer-facing changelog for Agent Data Plane (ADP) changes. Contributors must be able to commit a small, curated note with a change. Each Saluki GitHub release must present those notes before the existing GitHub-generated list of merged pull requests.

This design deliberately establishes Saluki as the canonical source for its release notes before changing the Datadog Agent repository.

## Goals

- Store optional, categorized release notes in Saluki with the commits they describe.
- Render only the notes that belong to a published `X.Y.Z` tag.
- Put curated notes at the start of the corresponding GitHub release body.
- Preserve GitHub's generated `What's Changed` pull-request list and comparison link at the bottom of the body.
- Make rendering safe to re-run after a failed workflow or a release-body correction.
- Validate a note when one is added without requiring any pull request to add a note.

## Non-goals

- Enforce a changelog label, a pull-request template checkbox, or a requirement that every pull request contains a note.
- Add a generated, committed monolithic `CHANGELOG` file.
- Backfill historical Saluki releases.
- Change Datadog Agent release notes or automate the Agent Data Plane bump workflow in the Datadog Agent repository.

## Release-note source format

Saluki will use Reno's repository-native release-note model:

- `releasenotes/config.yaml` configures Reno for Saluki's exact `X.Y.Z` release tags and defines the release-note template.
- `releasenotes/notes/<topic>-<unique-id>.yaml` contains one or more customer-facing notes associated with a change.
- Note text is reStructuredText (RST), matching the Datadog Agent convention. A renderer converts the resulting report to Markdown at the GitHub Release boundary.
- Notes remain in the repository after release. Reno uses Git history and matching tags to select the notes for a requested version.

The configured categories and display order match the Datadog Agent's Reno taxonomy:

1. `upgrade`: actions customers must take or material behavior changes.
2. `features`: new customer-visible capabilities.
3. `enhancements`: smaller customer-visible improvements.
4. `issues`: known limitations.
5. `deprecations`: behavior or interfaces scheduled for removal.
6. `security`: customer-relevant security changes.
7. `fixes`: corrected customer-visible behavior.
8. `other`: uncommon information that does not fit another category.

The contributor guidance will explain that a note is optional, must be self-contained, and is written for someone operating ADP rather than for Saluki developers. It will show `reno new <topic>` as the supported way to create a correctly named file.

## Tooling and validation

Reno will be pinned with the repository's existing Python tooling so local and CI rendering use a known version. A Make target will render the report for an explicit `VERSION=X.Y.Z`; maintainers can use it to inspect the Markdown before publishing a release.

A focused pull-request workflow will run only when `releasenotes/notes/**` changes. It will validate the added or modified note files with Reno, including their YAML structure and generated filename convention. It will not run a missing-note check, inspect labels, or block pull requests that do not touch release-note files.

The release rendering and release-body merge will live in a small, testable helper rather than in ad hoc workflow shell. The helper is responsible for rendering, RST-to-Markdown conversion, empty-section removal, and replacing the marked generated block. The GitHub Actions workflow only supplies the tag and performs the authenticated release update.

## Published-release flow

The current release process remains centered on a GitHub release. GitHub's **Generate release notes** operation continues to create the exhaustive merged-pull-request list.

After a release is published, a GitHub Actions workflow will:

1. Confirm that the release tag is an official `X.Y.Z` Saluki version and check out that tag.
2. Render the Reno report for that exact version and convert the report to Markdown.
3. Fetch the current GitHub release body, which already contains GitHub's generated pull-request list.
4. Build a curated block bounded by stable HTML comments.
5. Prepend or replace that block, preserving all existing body content below it.
6. Update the same GitHub release.

A populated release will have this shape:

```markdown
<!-- saluki-curated-notes:start -->
# Release notes

## Enhancement Notes
- …

## Bug Fixes
- …
<!-- saluki-curated-notes:end -->

## What's Changed
* …GitHub-generated merged pull requests…

**Full Changelog**: …
```

The body merge never regenerates or parses GitHub's `What's Changed` section. It preserves that section, the comparison link, and any manually supplied prose exactly as GitHub supplied them. This keeps the curated changelog first and the exhaustive pull-request record at the bottom.

If a release contains no curated notes, that is a valid result under the opt-in policy. The workflow succeeds without modifying the GitHub-generated release body.

The marker-delimited block makes the workflow idempotent: a manual rerun replaces only an older generated block. A `workflow_dispatch` entry point accepts a release tag for this repair case. Rendering, tag validation, or GitHub API failures occur before any update, so the pre-existing GitHub-generated body remains available as the fallback.

The workflow receives only the write permission needed to update a release and follows the repository's existing trusted GitHub Actions credential policy.

## Release documentation

`docs/agent-data-plane/releasing.md` will retain the current instruction to generate GitHub release notes. It will additionally instruct the releaser to confirm that the curated-release-notes workflow succeeded before posting the release announcement or manually publishing release artifacts from the tag pipeline.

## Verification

Automated tests will cover:

- recognition of valid `X.Y.Z` release tags and rejection or skipping of other tags;
- malformed YAML, unsupported sections, and invalid Reno-generated filenames;
- configured category order and omission of empty categories;
- selection of notes from the release tag rather than later commits on `main`;
- preservation of an existing GitHub release body;
- replacement, rather than duplication, of an existing marked block; and
- the successful no-op case for an empty report.

The implementation will also run the repository's documented formatting, Python-tooling, and relevant workflow checks.

## Deferred Datadog Agent integration

The Saluki work creates a stable, curated URL at `https://github.com/DataDog/saluki/releases/tag/<version>`. A later Datadog Agent change can use that URL while bumping the bundled ADP version.

The initial Agent-side option is one `other` Reno note linking to the corresponding Saluki release, matching the existing Agent entries for ADP 1.3.0 and 1.4.0. Importing the categorized Saluki content into the Agent changelog is explicitly out of scope for this phase. DADP-144 should remain open or be split until that follow-up is completed.
