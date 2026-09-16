---
name: create-release-note
description: >
  Write a Reno release-note fragment under `releasenotes/notes/` for a change, or determine that a
  change needs the `changelog/no-changelog` label instead. TRIGGER when: user asks for a release
  note, a changelog entry, or a changelog fragment; the release-note check has failed on a pull
  request; or user invokes `/create-release-note`. DO NOT TRIGGER when: user asks to publish or
  repair the release notes of an already-tagged release — that is the `Release notes` workflow's
  job, not a fragment.
argument-hint: <topic>   e.g. `fix-listener-shutdown-race`
allowed-tools: Bash, Read, Write, Edit, Glob, Grep, AskUserQuestion
---

# /create-release-note

Every pull request records whether it has customer-visible release information. When it does, it
carries a Reno note under `releasenotes/notes/`; when it does not, a maintainer applies the
`changelog/no-changelog` label. The `Release note check` workflow requires one of the two before a
pull request can merge.

Notes are the source for the curated section of each tagged GitHub release: `reno report` renders
them as reStructuredText, and `ci/tooling/release_notes.py render` converts that to
GitHub-flavored Markdown. **The prose you write is reStructuredText, not Markdown** — see step 4.

## Step 1: Decide whether the change needs a note

Read the change before asking the user anything:

```bash
git diff $(git merge-base HEAD origin/main)...HEAD --stat
```

A note is needed when the change alters behavior someone operating Agent Data Plane could observe:
new or changed configuration, metrics, telemetry, defaults, performance characteristics, error
handling, or a fixed bug.

A note is not needed for changes with no observable effect on the shipped binary: tests,
documentation, CI configuration, developer tooling, dependency bumps that change no behavior, and
pure refactors. In that case, don't invent one — tell the user the change looks like a
`changelog/no-changelog` candidate and that a maintainer applies that label on the pull request.

When it's genuinely ambiguous, ask the user with `AskUserQuestion` rather than guessing.

## Step 2: Choose the section and draft the content

Propose a section and a draft entry from the diff, then confirm both with the user. Sections come
from `releasenotes/config.yaml`:

| Section | When to use |
|---|---|
| `upgrade` | A required operator action or a major behavior change on upgrade |
| `features` | A wholly new customer-visible capability |
| `enhancements` | A customer-visible improvement too small to be a feature |
| `issues` | A known customer-visible limitation |
| `deprecations` | A planned customer-visible removal |
| `security` | A customer-relevant security change |
| `fixes` | A customer-visible correction |
| `other` | Customer-visible release information that fits no other section |

The entry is read by people operating Agent Data Plane, not by reviewers of this pull request.
Describe the observable effect, not the implementation, and make it self-contained: no references
to pull requests, commits, internal type names, or other release notes.

## Step 3: Create the file with Reno

```bash
make ensure-python-venv          # only if .venv/bin/reno is missing
.venv/bin/reno new <topic> --no-edit
```

`<topic>` is a short kebab-case slug describing the change, for example
`fix-listener-shutdown-race`. Reno writes `releasenotes/notes/<topic>-<16 hex characters>.yaml`.

**Never hand-write the file or its filename.** The hex suffix is Reno's unique identifier for the
note. `make check-release-notes` rejects a filename that doesn't match
`<topic>-<16 lowercase hex characters>.yaml`, and it rejects two notes that share an identifier —
a collision that a guessed or copied suffix produces and that breaks `reno report` at release time.

If `reno` can't be installed at all, generate a real suffix with `openssl rand -hex 8` rather than
typing one.

## Step 4: Write the entry

Keep only the section chosen in step 2 and delete every other section of the template, including
its comments. The result is small:

```yaml
fixes:
  - |
    Fix a listener shutdown race that could drop telemetry during process exit.
```

Prose is reStructuredText. `make check-release-notes` rejects the Markdown spellings:

| Instead of Markdown | Write reStructuredText |
|---|---|
| `` `dogstatsd_port` `` | ``` ``dogstatsd_port`` ``` |
| `[the docs](https://example.test)` | ``` `the docs <https://example.test>`_ ``` |
| `__important__` | `**important**` |
| `# Heading` | A title underlined with `===` |
| A ` ``` `-fenced code block | A `.. code-block:: yaml` directive |
| `> quoted text` | Indentation, or a `.. note::` directive |

`**bold**` is the same in both, so it needs no change.

Use a separate list item per distinct change, in the same section:

```yaml
fixes:
  - |
    Fix a listener shutdown race that could drop telemetry during process exit.
  - |
    Fix a panic when a DogStatsD payload ends mid-tag.
```

## Step 5: Validate

```bash
make check-release-notes
.venv/bin/reno lint
```

Fix anything either reports and re-run. Report to the user which file you created, its section, and
its content.
