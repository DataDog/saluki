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

These apply to every PR, and make up the root `codereview_guideline.md`. It starts with the instructions on
[How we review](./how-we-review.md#instructions-for-ai-reviewers). The checklist that follows comes from the pages that
own each topic:

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

## Area checklists

Each of these makes up a `codereview_guideline.md` file in a subdirectory, and applies only to PRs that change files
under that directory. When a page already covers an area, the checklist for that area lives on that page:

- `lib/saluki-core`: [Core crate](./conventions.md#core-crate)
- `lib/datadog-agent/config`, `lib/agent-data-plane-config`, `lib/agent-data-plane-config-system`, `lib/saluki-config`,
  and `lib/datadog-agent/runtime`: [Configuration system review checklist](./configuration-system.md#review-checklist)
- `bin/correctness` and `test`: [Reviewing test changes](./testing.md#reviewing-test-changes)

### I/O crate

Reviewers apply these rules to pull requests that change `lib/saluki-io`:

<!-- review:saluki-io -->
Apply these rules to files under `lib/saluki-io`: buffers, the DogStatsD codec, listeners, the HTTP client, compression,
and retry queues.

- Prefer exposing builder methods for core primitives (HTTP client, server, etc) while ensuring that a sensible default
  exists: for example, HTTP requests should always have a default timeout, but must be configurable.
- All codecs must be both property and fuzz tested at the raw input boundary: this ensures a minimum baseline of coverage.
- Helpers and primitives should strive to defer choices around memory management and allocations to the caller: for example,
  don't allocate on behalf of the caller if we could just as easily accept a mutable reference to a buffer.

<!-- /review -->

### Components

Reviewers apply these rules to pull requests that change `lib/saluki-components`:

<!-- review:saluki-components -->
Apply these rules to files under `lib/saluki-components`.

- Flag a new async `Transform` that only changes events in place and awaits nothing but its input. Make it a
  `SynchronousTransform`, so that `ChainedConfiguration` can run it in one component together with the other
  synchronous transforms. An async transform that also waits on something else, such as configuration updates or a
  timer, is fine.
- A component runs until the topology stops it. Flag a `run` that returns early for a check that `build` can make.
  Examples are taking a receiver or a subscription, building TLS material, and validating configuration. Do that check
  in `build`, so that the topology build reports the error. Binding a listener in `run`, before `health.mark_ready()`,
  is fine.
- Components should always prefer to acquire environment-based configuration from `EnvironmentProvider` instead of
  passing it through their configuration directly. For example, take the hostname of the current host from
  `HostProvider::get_hostname`, accessed through `EnvironmentProvider::host`.
- For encoders that have payload size limits, ensure that they build their payloads incrementally, if possible, to minimize
  the chance of having the payload rejected. If payloads can be split, then this applies doubly so as incremental encoding
  provides a chance to opportunistically split payloads and avoid costly re-encoding.

<!-- /review -->

### Environment and workloads

Reviewers apply these rules to pull requests that change `lib/saluki-env`:

<!-- review:saluki-env -->
Apply these rules to files under `lib/saluki-env`. This crate reads cgroups, procfs, and containerd to get workload
metadata, and it converts autodiscovery configurations. Bugs here are often silent. The result is missing container
tags, not an error.

- Code in this crate SHOULD strive, as much as reasonably possible, to gracefully handle environmental differences
  and incompatibilities, in a way that does not interrupt overall operation but does not go unnoticed. For example,
  failing to query the tags for an entity for unexpected reasons can cause metrics correctness issues, and so so we
  should alert the operator but we should not degrade in such a way that other operations fail, since transient
  errors are expected more often in this area.
- Unbounded growth of metadata can occur rapidly, so all metadata operations that store state should either be bounded
  in time (such as by using a cache with an expiration/TTL) or have matching/paired codepaths that clean up the state
  at the earliest opportunity.

<!-- /review -->

### Agent Data Plane

Reviewers apply these rules to pull requests that change `bin/agent-data-plane`:

<!-- review:agent-data-plane -->
Apply these rules to files under `bin/agent-data-plane`.

- The Agent Data Plane crate is, primarily, the point at which we assemble various components and types into our topology
  and so shouldn't be a spot where we're adding unit tests. The major exception to this is code that lives in internal
  modules. If you would reach for a unit test when making a change to how the topology is configured or how we handle
  decisions made during bootstrap or configuration loading, that's likely a signal to create an integration test instead.
- Flag a stage that records state, such as a first-seen set, before a stage that can drop the event. Flag a filter that
  runs before the mapping or split that it depends on. A destination that measures input, such as `dsd_stats_out` on
  `dsd_in.metrics`, connects to the raw source output.
- Ensure that identifiying tags (such as `component_id` or `resolver_id`) in metric remapping rules are updated if and
  when the corresponding component's ID changed as part of topology construction.
- Ensure that metric remap rules are arranged from highest to lowest specificity when there are multiple rules for the
  same "base" series: for example, a remap rule that matches `my_counter` directly coming before a rule that matched
  `my_counter` with a specific tag value would cause the second rule to never fire.
- Ensure that metrics referenced in a remap rule or a status section are registered at `info` level, and not `debug`
  or `trace`, otherwise they won't be picked up by default.
<!-- /review -->

### GitHub workflows

Reviewers apply these rules to pull requests that change `.github`:

<!-- review:github -->
Apply these rules to files under `.github`, such as workflows and `dd-octo-sts` trust policies.

Most of these changes take effect only after they merge, so pull request checks don't test them. Read each changed line.

- When creating a new workflow, check for existing workflows of the same category (opening PRs to bump dependencies, etc)
  and ensure the same patterns are followed so that our workflows stay consistent.
- Workflows that need to push commits, or open/label pull requests, MUST get a Github token from the
  `DataDog/dd-octo-sts-action` action. This action requires a corresponding policy (under `.github/chainguard/`) that describes
  when the policy applies (the "claim") and the permissions granted if the claim is validated.
- Use the `DataDog/commit-headless` action to properly commit changes based on an acquired `dd-octo-sts` token.
- Use labels on automatically created pull requests to both identify the PRs as a result of automation, as well as categorizing
  and grouping equivalent PRs: for example, a common pattern is to add a label of `ci/<workflow name>` to identify PRs created
  by a specific workflow.
- Workflows that create resources, like issues or pull requests, should ensure that they clean up older/superseded resources
  where possible: for example, when a workflow opens PRs to bump dependencies, it should close older PRs (potentially by
  findign open PRs with the labels it sets) if the new PR would supersede them.
- Workflows must pin each action they depend on, as well as reusable workflows, to a full commit SHA, with its version in a
- trailing comment such as `# v7.0.1`. Renovate reads that comment to update the pin.
- In a trust policy, `subject_pattern` and each value under `claim_pattern` are regular expressions, and `subject` is
  an exact string. Write `refs/.*`, not the glob `refs/*`. A subject that needs a wildcard goes in `subject_pattern`.
  (A policy used the glob `refs/*` and needed a second fix: #1465.)
- For `dd-octo-sts` policies, pay attention to the `subject_pattern` and `claim_pattern.event_name`. These must be aligned
  with the workflow's event triggers (the values under `on:`) and the subject pattern will change depending on event type. For
  example, the subject pattern for a workflow that targets PRs themselves will be different than the subject pattern for a
  workflow that only runs on pushes/merges to `main`.
- Workflow jobs that depend on the artifacts of another job must list that job under `needs:` to ensure ther workflow
  waits for the dependent job to complete.
- Release branches are `releases/X.Y.x`, release tags are `X.Y.Z` with no `v` prefix, and backport labels are
  `backport/X.Y.x`.
- Don't flag untrusted checkouts or script injection under `pull_request_target`, because CodeQL reports them. A job
  that checks out only commits already on `main` or a `releases/*` branch, as `backport.yml` does, is fine.
<!-- /review -->

### GitLab CI

Reviewers apply these rules to pull requests that change `.gitlab`:

<!-- review:gitlab -->
Apply these rules to files under `.gitlab`.

Many jobs run automatically only on tags or schedules, and otherwise only when someone starts them. When the pull
request changes the logic of such a job, and the description doesn't show a pipeline that ran it, ask for one. Don't
ask for one on an automated bump of a version or an image digest.

- Each new `${VAR}` reference has balanced braces and a definition that the job reaches. Definitions can be in the
  job, including `id_tokens:`, its `extends:` chain, the global `variables:` and `workflow:` rules in
  `.gitlab-ci.yml`, or the script. A child pipeline also gets the variables of its `trigger:` job and the variables
  that `ci/tooling/generate-correctness-pipeline.sh` writes. A `dotenv` artifact of an earlier job defines variables
  too, but a job with `needs:` gets them only from the jobs in that list. Shell defaults such as `${VAR:-}`,
  predefined `CI_*` and `GITLAB_*` variables, and schedule variables such as `SMP_NIGHTLY` need no definition. (A
  release job that lacked the template with its image variables built from an empty `FROM`: #579.)
- `extends:` merges `variables:` key by key, but a list that the job sets, such as `needs:`, replaces the list of the
  template. A release job takes its `needs:` only from the `.release-gating-*` template of its platform. Add a release
  prerequisite to that template, not to the job. (Release jobs with their own `needs:` replaced the test needs of
  their templates, so they could publish before tests passed: #2251.)
- Flag `when: always` in `rules:`, because the job then runs after a job that it depends on fails. The dependency can
  be a `needs:` entry in the job, its rule, or a template, or a job in an earlier stage. A job that cleans up or
  reports results, such as `report-benchmarks-adp`, can use it. `when: always` under `artifacts:` is fine.
  (Multi-arch image jobs ran after a per-architecture build failed: #1456.)
- Siblings of a job are the job for the other architecture, the FIPS and `-internal` variants, and the other
  benchmark jobs. The two `PANORAMIC_IMAGE_OVERRIDES` lists are siblings too. When the pull request changes one, ask
  whether each sibling needs the same change. A job copied from a sibling uses the names of its own variant in
  `needs:`, image tags, and `BUILD_FEATURES`. (The FIPS tarball jobs waited on the non-FIPS image build: #1085.)
- In the `variables:` of a `trigger:` job, nested variables can expand in the downstream pipeline, with its own
  `CI_*` values, or not at all. For per-pipeline values, use only the forms of the current trigger jobs: `CI_*`
  variables, or `${ADP_IMAGE_VERSION}` and `${ADP_IMAGE_VERSION_FIPS}` with a fixed suffix. For another form, such
  as `ADP_INTERNAL_IMAGE_TAG`, ask for a pipeline that shows the downstream values. In `correctness-mixins.yml`,
  which runs in a child pipeline, use `PARENT_PIPELINE_ID` for the images and artifacts of the parent pipeline, not
  `CI_PIPELINE_ID`. (An internal image tag got downstream values: #656. A publish job's source image lost the
  version that the `workflow:` rules set: #719.)
- When pipeline variables, such as `AGENT_NIGHTLY`, change an image, its tag includes the pipeline ID, because two
  pipelines can build one commit. (A retried nightly job pulled the image of a `main` pipeline: #2731.)
- Some values can change while the pipeline runs, such as the merge base with `main` or a nightly SHA that depends on
  the date. When several jobs use such a value, one job computes it and passes it in a `dotenv` artifact, as
  `smp-vars.env` does. (A merge to `main` between two jobs could change the SMP baseline: #987.)
- Artifact paths are inside the project directory. GitLab doesn't upload other paths, such as `/tmp`, and the job
  doesn't fail. (Ground-truth logs under `/tmp` were never uploaded: #1400.)
- A job name is unique across the files under `.gitlab` and `.gitlab-ci.yml`. A second definition with the same name
  replaces the first or merges into it, with no error. (The FIPS publish job reused the name of the non-FIPS job:
  #701.)
- Jobs on `arch:*` runners, including the `arch:amd64` default, can lack a Docker daemon. Run commands that need one,
  such as `docker pull`, `docker run`, `docker inspect`, and `docker create`, on a `docker-in-docker:*` runner. To
  read an image, you can also query the registry with `docker buildx imagetools inspect` or `crane`. (The nightly
  `build-bundled-agent-adp-image` job failed at random: #1943.)
<!-- /review -->
