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

The rules for release notes live under "Release notes" in `docs/development/contributing.md`:
when a change needs a note, the sections, how to write the entry, and the reStructuredText spellings
that `make check-release-notes` requires. Read that section first, then follow these steps.

## Step 1: Decide whether the change needs a note

Read the change before asking the user anything:

```bash
git diff $(git merge-base HEAD origin/main)...HEAD --stat
```

Apply "When a change needs a note". If it doesn't need one, don't invent one: tell the user the
change looks like a `changelog/no-changelog` candidate, and that a maintainer applies that label on
the pull request. When it's genuinely ambiguous, ask the user with `AskUserQuestion` rather than
guessing.

## Step 2: Choose the section and draft the content

Propose a section and a draft entry from the diff, then confirm both with the user before writing
the file.

## Step 3: Create the file with Reno

```bash
make ensure-python-venv          # only if .venv/bin/reno is missing
.venv/bin/reno new <topic> --no-edit
```

`<topic>` is a short kebab-case slug describing the change, for example
`fix-listener-shutdown-race`. **Never hand-write the file or its filename**: the hex suffix Reno
appends is the note's unique identifier.

## Step 4: Write the entry

Keep only the section chosen in step 2, delete every other section of the template including its
comments, and write the entry in reStructuredText.

## Step 5: Validate

```bash
make check-release-notes
.venv/bin/reno lint
```

Fix anything either reports and re-run. Report to the user which file you created, its section, and
its content.
