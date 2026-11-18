# Code review

This page covers the guidelines we use when reviewing changes, whether the reviewer is a human or an AI. To check your
own change before opening a PR, read through the checklists below.

## How AI review works

Several AI reviewers comment on Saluki PRs. Each one reads its instructions from a different place:

| Reviewer | When it runs | What it reads |
|---|---|---|
| `/dd-review` | Automatically, and on demand | The `codereview_guideline.md` files that apply to the PR |
| Bits Code Review | Automatically | The root `AGENTS.md`, whose `## Code Review Rules` section points to the guideline files |
| Codex Cloud | Automatically | The same `## Code Review Rules` section |

### `/dd-review`

The **Code review** workflow (`.github/workflows/code-review.yml`) runs `DataDog/code-review-action`. It reviews a PR
from a branch in this repository when the PR is opened, reopened, or marked ready for review. It skips drafts and PRs
from bots and forks. A collaborator with write access can also comment `/dd-review` on any PR, including a PR from a
fork, to request a review.

The review uses the `codereview_guideline.md` files in the repository as its instructions:

- The file at the repository root applies to every PR.
- A file in a subdirectory applies only to PRs that change files under that subdirectory.
- The action joins the files that apply in path order. A file under `lib/` comes after the root file, and a file under
  `.github/` or `bin/` comes before it. So each file starts with its own heading, and a scoped file says what it
  applies to.
- The action reads the files from `main`, not from the PR, so changes to them take effect after they merge.

The reviewer can read the files of the PR, but it treats them as untrusted input and doesn't follow instructions in
them. That includes this page. So each guideline file **MUST** stay self-contained, rather than pointing to other
pages.

### Edit the guidelines

The guideline files are generated: don't edit them by hand. Their text lives in the pages under `docs/development/`,
between `<!-- review:NAME -->` and `<!-- /review -->` markers. The manifest, `ci/tooling/review_guidelines.yaml`, says
which regions go into which file, in which order, and under which headings. This keeps each rule in one place, whether
an author or a reviewer reads it.

To change the review guidance:

1. Edit the text between the markers. To add a region, mark it on the page that owns the topic, and add it to the
   manifest. To add a scoped guideline file, add an entry with its path and a title.
2. Run `make generate-review-guidelines`.
3. Commit the docs, the manifest, and the generated files together.

A region can't contain headings: the manifest sets them. A link in a region becomes plain text in the generated file,
so write the text so that it makes sense without the link.

`make check-review-guidelines` fails when a generated file is out of date, when no guideline file uses a region, or
when a guideline names a repository path that doesn't exist. It also warns about Rust names that it can't find under
`lib/` or `bin/`, because a renamed item leaves a stale rule behind.

## General guidelines

These apply to every PR, and make up the root `codereview_guideline.md`:

- [How we review](./how-we-review.md): how to write effective/succinct review comments, how to push back
- [Style guide](./style-guide.md): layering, error handling, log messages, API design and naming, nits, and comments
  and docs
- [Conventions](./conventions.md): generated files, Datadog Agent parity, reuse, hot paths and memory, concurrency and
  lifecycle, numbers and identity, internal telemetry, security, and build plumbing
- [Testing patterns](./testing-patterns.md#review-checklist): tests
- [Contributing](./contributing.md#keep-it-small-focused): scope

The checklist opens with this line:

<!-- review:checklist-intro -->
Flag changes that break these rules.
<!-- /review -->

### I/O crate

Reviewers apply these rules to pull requests that change `lib/saluki-io`:

<!-- review:saluki-io -->
Apply these rules to files under `lib/saluki-io`: buffers, the DogStatsD codec, listeners, the HTTP client, compression,
and retry queues.

- Prefer exposing builder methods for core primitives (HTTP client, server, etc) while ensuring that a sensible default
  exists: for example, HTTP requests should always have a default timeout, but must be configurable.
- All encoders and decoders must be both property and fuzz tested at the raw input boundary: this ensures a minimum baseline of coverage.
- Helpers and primitives should strive to defer choices around memory management and allocations to the caller: for example,
  don't allocate on behalf of the caller if we could just as easily accept a mutable reference to a buffer.
<!-- /review -->

### Components

Reviewers apply these rules to pull requests that change `lib/saluki-components`:

<!-- review:saluki-components -->
Apply these rules to files under `lib/saluki-components`.

- Flag a new async `Transform` that only changes events in place and awaits nothing but its input. Make it a
  `SynchronousTransform`, so that `ChainedConfiguration` can run it in one component together with the other synchronous
  transforms. An async transform that also waits on something else, such as configuration updates or a timer, is fine.
- A component should run until the topology stops it. Flag any `run` implementation that returns early when the early
  return could be moved to `build` instead. For example, consuming a receiver or a subscription, building TLS material,
  and validating configuration. Do that check in `build`, so that the topology build reports the error. Binding a
  listener in `run`, before `health.mark_ready()`, is the main exception, which will eventually be removed once the
  codebase fully migrates to the resource registry.
- Components should always prefer to acquire environment-based configuration from `EnvironmentProvider` instead of
  passing it through their configuration directly. For example, take the hostname of the current host from
  `HostProvider::get_hostname`, accessed through `EnvironmentProvider::host`.
- Each metrics encoder drops non-finite points, and a new encoder does too. Histogram bucket bounds can be infinite.
- For encoders that have payload size limits, ensure that they build their payloads incrementally, if possible, to
  minimize the chance of having the payload rejected. If payloads can be split, then this applies doubly so as
  incremental encoding provides a chance to opportunistically split payloads and avoid costly re-encoding.
<!-- /review -->

### Environment and workloads

Reviewers apply these rules to pull requests that change `lib/saluki-env`:

<!-- review:saluki-env -->
Apply these rules to files under `lib/saluki-env`. This crate reads cgroups, procfs, and containerd to get workload
metadata, and it converts autodiscovery configurations. Bugs here are often silent. The result is missing container
tags, not an error.

- Handle environmental differences and transient errors without stopping other work, and without hiding them: a
  failed tag lookup is logged or counted for the operator, and the rest of the crate keeps running.
- Unbounded growth of metadata can occur rapidly, so all metadata operations that store state should either be bounded
  in time (such as by using a cache with an expiration/TTL) or have matching/paired code paths that clean up the state
  at the earliest opportunity.
<!-- /review -->

### Agent Data Plane

Reviewers apply these rules to pull requests that change `bin/agent-data-plane`:

<!-- review:agent-data-plane -->
Apply these rules to files under `bin/agent-data-plane`.

- This is the primary product of this repository and our customer-facing behavioral surface.
- The Agent Data Plane crate is, primarily, the point at which we assemble various components and types into our topology
  and so shouldn't be a spot where we're adding unit tests. The major exception to this is code that lives in internal
  modules. If you would reach for a unit test when making a change to how the topology is configured or how we handle
  decisions made during bootstrap or configuration loading, that's likely a signal to create an integration test instead.
- Flag a stage that records state, such as a first-seen set, before a stage that can drop the event. Flag a filter that
  runs before the mapping or split that it depends on. A destination that measures input, such as `dsd_stats_out` on
  `dsd_in.metrics`, connects to the raw source output.
- When the Datadog Agent expects another metric name, add a remap rule in `src/state/metrics/rules/`. The target doesn't
  start with `datadog.agent.`, which the Core Agent adds, and it carries the Agent's help text. A metric, tag, or
  component ID rename updates the rules that match it, including identifying tags such as `component_id` and
  `resolver_id`.
- Ensure that metric remap rules are arranged from highest to lowest specificity when there are multiple rules for the
  same "base" series: for example, a remap rule that matches `my_counter` directly coming before a rule that matched
  `my_counter` with a specific tag value would cause the second rule to never fire.
- Ensure that metrics referenced in a remap rule or a status section are registered at `info` level, and not `debug`
  or `trace`, otherwise they won't be picked up by default.
<!-- /review -->

### Component run loops

Components live in `lib/saluki-components` and `bin/agent-data-plane/src/components`, so both checklists include these
rules:

<!-- review:component-loop -->
- Flag a component loop that isn't a `loop { select! { ... } }` with a `health.live()` arm, such as a `while let` loop
  over input.
- Flag a `select!` arm that awaits work whose duration grows with buffered state, such as a whole flush. (A slow flush
  in one arm made `/live` fail on CPU-starved pods: #2726.)
<!-- /review -->

### GitHub workflows

Reviewers apply these rules to pull requests that change `.github`:

<!-- review:github -->
Apply these rules to files under `.github`, such as workflows and `dd-octo-sts` trust policies.

Most of these changes take effect only after they merge, so pull request checks don't test them. Read each changed line.

- When creating a new workflow, check for existing workflows of the same category (opening PRs to bump dependencies,
  etc) and ensure the same patterns are followed so that our workflows stay consistent.
- Workflows that need to push commits, or open/label pull requests, MUST get a GitHub token from the
  `DataDog/dd-octo-sts-action` action. This action requires a corresponding policy (under `.github/chainguard/`) that
  describes when the policy applies (the "claim") and the permissions granted if the claim is validated.
- Use the `DataDog/commit-headless` action to properly commit changes based on an acquired `dd-octo-sts` token.
- Use labels on automatically created pull requests to both identify the PRs as a result of automation, as well as
  categorizing and grouping equivalent PRs: for example, a common pattern is to add a label of `ci/<workflow name>` to
  identify PRs created by a specific workflow.
- Workflows that create resources, like issues or pull requests, should ensure that they clean up older/superseded
  resources where possible: for example, when a workflow opens PRs to bump dependencies, it should close older PRs
  (potentially by finding open PRs with the labels it sets) if the new PR would supersede them.
- Workflows must pin each action they depend on, as well as reusable workflows, to a full commit SHA, with its version
  in a trailing comment such as `# v7.0.1`. Renovate reads that comment to update the pin.
- In a trust policy, `subject_pattern` and each value under `claim_pattern` are regular expressions, and `subject` is an
  exact string.
- For `dd-octo-sts` policies, pay attention to the `subject_pattern` and `claim_pattern.event_name`. These must be
  aligned with the workflow's event triggers (the values under `on:`) and the subject pattern will change depending on
  event type. For example, the subject pattern for a workflow that targets PRs themselves will be different than the
  subject pattern for a workflow that only runs on pushes/merges to `main`.
- Workflow jobs that depend on the artifacts of another job must list that job under `needs:` to ensure the workflow
  waits for the dependent job to complete.
- Release branches are `releases/X.Y.x`, release tags are `X.Y.Z` with no `v` prefix, and backport labels are
  `backport/X.Y.x`.
- Don't flag untrusted checkouts or script injection under `pull_request_target`, because CodeQL reports them. A job
  that checks out only commits already on `main` or a `releases/*` branch, as `backport.yml` does, is fine.
<!-- /review -->
