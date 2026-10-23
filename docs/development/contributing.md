# Contributing

First of all, thank you for looking to contribute to Saluki. We're excited to have you here.

This document covers filing issues and the lifecycle of a code change: making it, committing it, opening a pull
request, adding a release note, addressing review feedback, and merging.

## Filing issues

If you've discovered a bug, have a feature request, or have discovered a security issue, start by [creating a new
issue](https://github.com/DataDog/saluki/issues/new/choose). We use GitHub issue templates to provide a number of
pre-defined issue types, which will prompt you for the necessary information to help us understand the problem.

If none of the existing issue types fit your needs, create a **Blank issue** and provide as much information as you
can.

## Making a change

We strive for a high level of quality in terms of pull requests, and this guide is meant to help you understand what we
expect from a PR so that you're not surprised during the review process.

### Keep it small, focused

Avoid changing too many things at once.

For example, if you're working on changes to a component that requires adding new functionality to core types
(topology builder, event model, etc), then you should split up your changes into multiple PRs, focusing first on the
supporting changes and then the final change that makes the component use the new functionality. Stack dependent PRs
on top of each other so that each one can be reviewed on its own.

### Testing

We generally require unit tests for bug fixes, as well as new features. See [Testing](./testing.md) for the kinds of
tests we write, and [Testing patterns](./testing-patterns.md) for how to write them.

Sometimes, this can be difficult to do depending on the type of code change: some areas of code don't have a great way
to be run independently and we often choose to not abstract code just to make it testable. This can sometimes
require a little more thought and creativity to test the code.

A good rule of thumb to follow is that if you don't have to change the code to make it testable, then you should
probably write a test for it. For example, if you're fixing a bug in an existing type, and that type is already being
tested in other ways... then the overhead to add an additional test is low, and so you should do it.

Depending on the complexity of the change, we may insist that tests be included, even if it means adding additional
abstractions. Reviewers will help guide you on this during the review process.

### Documentation

If a change alters behavior, update the documentation, code comments, and rustdoc that describe it in the same PR.
All documentation lives under `docs/`; see [Where documentation lives](./style-guide.md#where-documentation-lives).

## Commit messages and PR titles

PR titles **MUST** follow the [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/) format, which CI
enforces. A title has a type, an optional scope in parentheses, and a short description in the imperative mood:

```text
fix(dogstatsd): match core agent's listener startup behavior
```

The allowed types are:

- `feat`: a new capability
- `enhancement`: an improvement to an existing capability
- `fix`: a bug fix
- `docs`: documentation only
- `chore`: everything else, such as refactoring, tests, CI, dependencies, and developer tooling
- `revert`: reverting an earlier change

The scope names the area of the codebase you changed, such as `config`, `agent-data-plane`, `dogstatsd`, `otlp`, `test`,
`ci`, `deps`, or `dev`. Look at recent history on `main` for the scopes already in use, and reuse one where it fits.

We usually squash-merge PRs, so the PR title becomes the commit title on `main`. Write it for someone reading the history a year
from now: describe what changed, not how you got there. Wrap names of code items, configuration keys, and commands in
backticks.

Commit messages on your branch should still describe the change, rather than read `Fixed stuff` or `WIP`. Keep in mind
that they don't survive the squash: put anything a reviewer needs to know in the PR description.

## Opening a pull request

Open the PR when the code is reviewable, or open it as a draft if you want to share code before it's ready for review.

Fill out the PR template (`.github/PULL_REQUEST_TEMPLATE.md`):

- **Summary**: what the PR does and why, written so that a reviewer can give faster, better feedback
- **Change type**: bug fix, feature, non-functional change, or performance
- **How did you test this PR?**: the tests you added, and anything you ran by hand
- **References**: issues the PR closes, and related issues or PRs

Before marking a PR ready for review, run the checks described in the [Agent guide](./agents.md#checking-your-work).
A collaborator with write access can comment `/dd-review` on the PR to request an AI code review against our
[review guidelines](./code-review.md).

## Release notes

Every PR records whether it has customer-visible release information. When it does, add a Reno note under
`releasenotes/notes/`. When it doesn't, ask a maintainer to apply the `changelog/no-changelog` label. The release-note
check requires one of these choices before the PR can merge.

Release notes are the source for the curated section of each tagged GitHub release: `reno report` renders them as
reStructuredText, and `ci/tooling/release_notes.py render` converts that to GitHub-flavored Markdown.

### When a change needs a note

A note is needed when the change alters behavior that someone operating Agent Data Plane could observe: new or changed
configuration, metrics, telemetry, defaults, performance characteristics, error handling, or a fixed bug.

A note isn't needed for changes with no observable effect on the shipped binary: tests, documentation, CI
configuration, developer tooling, dependency bumps that change no behavior, and pure refactors.

### Create the note

```shell
make ensure-python-venv
.venv/bin/reno new describe-the-change --edit
```

`--edit` opens the new note in your editor. To create the file and edit it afterwards, use `--no-edit` instead.

Always create the file with `reno new`: the hexadecimal suffix it appends is the note's unique identifier, and two notes
that share one break release-note rendering at release time. `make check-release-notes` rejects any filename that
doesn't match `<topic>-<16 lowercase hex characters>.yaml`. If you can't install `reno` at all, generate a suffix with
`openssl rand -hex 8` rather than typing one.

### Write the note

Choose one section from `releasenotes/config.yaml`, and remove every other section of the template, including its
comments:

| Section | When to use |
|---|---|
| `upgrade` | A required operator action, or a major behavior change on upgrade |
| `features` | A wholly new customer-visible capability |
| `enhancements` | A customer-visible improvement too small to be a feature |
| `issues` | A known customer-visible limitation |
| `deprecations` | A planned customer-visible removal |
| `security` | A customer-relevant security change |
| `fixes` | A customer-visible correction |
| `other` | Customer-visible release information that fits no other section |

For example:

```yaml
fixes:
  - |
    Fix a listener shutdown race that could drop telemetry during process exit.
```

People operating Agent Data Plane read these notes, not reviewers of your PR. Describe the observable effect rather
than the implementation, and make each note self-contained: don't refer to PRs, commits, internal type names, or other
release notes. Use a separate list item for each distinct change.

Reno renders note prose as reStructuredText, not Markdown, and `make check-release-notes` rejects Markdown spellings:

| Instead of Markdown | Write reStructuredText |
|---|---|
| `` `dogstatsd_port` `` | ``` ``dogstatsd_port`` ``` |
| `[the docs](https://example.test)` | ``` `the docs <https://example.test>`_ ``` |
| `__important__` | `**important**` |
| `# Heading` | A title underlined with `===` |
| A fenced code block | A `.. code-block:: yaml` directive |
| `> quoted text` | Indentation, or a `.. note::` directive |

Validate notes locally with `make check-release-notes` and `.venv/bin/reno lint`.

## Addressing review feedback

The goals, in priority order, are:

- make PR reviews, both initial and follow-up, easy for reviewers using GitHub
- keep a meaningful history on `main` that explains, even years later, what each commit does and why

### Before the first review

Before the first review, meaningful commits are best: logically encapsulated commits help the reviews go quicker and
make the job for the reviewer easier. You can resolve conflicts with `main` by rebasing on `origin/main` and force
pushing, if it makes future reviews easier.

### After the first review

After the first review, make follow-up reviews easier:

- Address feedback with additional commits on the PR branch. Rewriting history that was already reviewed makes
  follow-up reviews painful, because GitHub loses track of each comment.
- Resolve merge conflicts with `main` by merging `origin/main`.
- Reply to each comment: say what you changed, or explain why you didn't. Leave it to the reviewer to resolve the
  conversation.
- In a stack of PRs, restacking after a change lower in the stack rewrites the branches above it. That's expected:
  keep the rewrite to the restack itself, and push feedback fixes as their own commits.

## Merging to `main`

Once reviews are complete, merge to `main` with either:

- the **squash-merge** option, which keeps the history of `main` clean, even though some details are lost in the squash.
  Always edit the commit message for the squash to concisely describe the change, without extraneous "address review
  comments" text.
- the **rebase-merge** option, after manually rewriting the PR's commit history and force-pushing to the branch. When
  using this option, the branch must have a clean history.
