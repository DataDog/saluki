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

- A frame comes only from `ReadCursor::take`, which advances the buffer in the same call. Flag a change under `src/buf/`
  that returns unread bytes that outlive the cursor, or that advances the buffer later, such as in a drop guard. Also
  flag a change that makes `ReadCursor::new` public, because then a cursor can advance a read index that isn't the
  buffer's own.
- `Buf::chunk()` returns only the first contiguous slice of a buffer. For `VecDeque<u8>`, which this crate implements
  `ReadIoBuffer` for, that slice ends at the wrap point. A chained or chunked buffer, such as
  `FrozenChunkedBytesBuffer`, returns only its first chunk. Flag new code in this crate that reads a whole frame or
  value from `chunk()` of such a buffer. (The capture framer couldn't see a frame that wrapped, so the capture buffer
  grew without limit: #2823.)
- The `permissive` field of `DogStatsDCodecConfiguration` sets which defects the parsers accept. In permissive mode, a
  parser skips an empty field, such as `||`, and parses the rest of the line. A metric also skips an empty value in a
  list of values, such as `1::2`. An event or a service check ignores an invalid or unknown optional field, and a metric
  ignores a malformed `c:` or `e:` field. Strict mode rejects the line for each of these defects. Flag a permissive path
  that rejects the whole line for one of them. A bad sample rate (`@`) or timestamp (`T`) rejects a metric in both
  modes, as the Datadog Agent does. In both modes, the metric parser skips an optional field that it doesn't know, so
  don't flag that. (Permissive mode rejected payloads that the Datadog Agent keeps: #3072.)
- The metric, event, and service check parsers in `src/deser/codec/dogstatsd/` each have their own field loop. Flag a
  loop that stops at a defect with `break` or an early `return` and still keeps the line. The decoder discards the input
  that the parser leaves unread, so it loses the rest of the line without an error. Flag a skip of a malformed field
  that doesn't check `permissive`, for example a skip of every short field. Strict mode then keeps the malformed field.
  (An empty field dropped the tags after it: #3071. A draft of that fix skipped every field shorter than two bytes,
  which let a malformed `|T|` through in strict mode.)
- `permissive`, the tag limits, and client origin detection apply in all three parsers. When a change adds a setting
  that more than one parser reads, or changes how one parser uses a setting, check the other parsers. (The parsers read
  `c:`, `e:`, and `card:` when the Agent configuration turned off client origin detection: #1524.)
- `CompressionEstimator` gives a hint, not a limit. `would_write_exceed_threshold` returns `false` until the compressor
  writes its first block, and the caller checks the real compressed size later. After the first block, it adds the
  uncompressed length of the next write to the compressed estimate, which is the intended worst case. Flag a change that
  asks for a flush before the first block. Also flag a comparison of an uncompressed length alone with the compressed
  limit, such as `len > threshold`. (Estimates before the first block caused flushes at about 2% of the limit: #503. A
  `len > threshold` check asked for a flush before the first write, and the caller panicked: #1310.)
- Set `SO_REUSEPORT` on any listener, and `SO_REUSEADDR` on a UDP socket, only when the listener binds several sockets
  to one port, as `bind_udp_sockets` does. On a single socket, these options let a second process that also sets them
  bind the same port and silently take part of the traffic. On Unix, `TcpListener::bind` sets `SO_REUSEADDR`, so that a
  restarted process can bind the port again at once. Don't flag it. When a diff moves socket setup, compare the options
  of each listener type before and after. (A refactor set the reuse options on every UDP bind: #3070.)
- Each connection attempt and each TLS handshake has a timeout. Through a proxy, `hyper_http_proxy::ProxyConnector` does
  a separate TLS handshake in the `CONNECT` tunnel, with its own TLS config from `HttpClientBuilder::build`. Flag a
  timeout or a certificate check for direct connections that the tunnel config doesn't also get. The tunnel config keeps
  platform roots and full verification on purpose, even when the direct config is insecure or comes from
  `with_client_tls_config`. (The handshake in a proxy tunnel had no timeout: #2309.)
- Flag a new `hyper-util` client builder that sets `pool_idle_timeout` without `pool_timer`: the idle timeout has no
  effect without a timer. `HttpClientBuilder::default` sets both.
- A retry path puts an entry back in the queue only after a transient error. Flag a path that re-queues an entry after a
  permanent error, such as a persisted entry that the queue can't decode. That entry fails on every attempt and blocks
  the queue. (A corrupt persisted entry went back in the queue and deadlocked the process: #1184.)
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
  is fine. (A checks source stopped itself when it had no autodiscovery subscription: #823.)
- Take the hostname that `HostProvider::get_hostname` resolves from the `EnvironmentProvider`. A constructor can take
  the provider, as `HostEnrichment::from_environment_provider` in `src/transforms/host_enrichment/` does. Get the
  hostname when the component builds, not in the binary before it builds the topology, so that the blueprint doesn't
  call external resources. Flag a component that reads a `hostname` setting or detects the hostname itself. (The APM
  configuration read a `hostname` key: #1095.)
- Outside tests, start the name of each `ContextResolverBuilder` and `TagsResolverBuilder` with the component ID, as
  `src/sources/dogstatsd/resolver.rs` does. Two resolvers with one name report their metrics as one. A rename changes
  the `resolver_id` tag, which remap rules in `bin/agent-data-plane/src/state/metrics/rules/` match. Update those rules
  in the same change.
- The builder of a caching context resolver calls `with_idle_context_expiration`. The builder of a tags resolver that
  goes to `with_tags_resolver` calls `with_idle_tagsets_expiration`. (The shared DogStatsD tags resolver had no
  expiration, so it held tag sets until it reached its limit: #838.)
- Use `Trace::ingest_info` to tell OTLP traces from native Datadog traces. Flag code that detects OTLP input from span
  attributes, such as `otel.trace_id`. Flag an OTLP-only field, such as `_dd.otlp_sr`, on a native trace. (The
  sampler treated native traces with an `otel.trace_id` tag as OTLP traces, and the encoder added `_dd.otlp_sr` to
  native traces: #2854.)
- Flag an encoder that builds a whole batch and checks its byte limits only afterward. Stop adding inputs before the
  uncompressed size or the estimated compressed size passes its limit. Split a payload that still comes out too large.
  (The V3 encoder checked byte limits only after it built a batch, which raised peak memory: #2230.)
- `RequestBuilder` in `src/common/datadog/request_builder.rs` uses `CompressionEstimator` only as a hint to flush
  early. It checks the real compressed size when it finishes a payload. Flag a guard, a hard limit, or a `panic!` that
  depends on the estimate. Also flag a request to flush before the builder writes the first input. Fix the estimate
  instead. (An estimate asked for a flush of an empty payload: #1310.)
- Clamp a configured payload limit to the limit for its payload type with `clamp_payload_limits` from
  `src/common/datadog/mod.rs`. The events, service checks, and metrics encoders do this. Then a user setting can't
  cause payloads that the intake rejects. (New payload limit settings had no upper bound: #1715.)
- Write a payload prefix or an input separator only after the request builder accepts the input. A rejected input
  leaves the payload unchanged. (Rejected inputs left framing such as `,]` and `[[[` in requests, and the intake dropped
  those requests: #3023.)
- `generate_retry_queue_id` in `src/common/datadog/io.rs` names the retry queue of each endpoint and its directory on
  disk. For an additional endpoint, it hashes the raw configured URL and the position of the API key. Then the ID
  stays the same when a key rotates. Flag an additional-endpoint ID that depends on the API key value or on the
  normalized URL. Flag a change to how the code derives the ID if the pull request description doesn't say so. After
  such a change, the retry queue doesn't load the transactions that it stored under the old ID. (Queue IDs changed
  when a key rotated, and two URLs that normalized to one host shared a queue: #1739.)
<!-- /review -->

### Environment and workloads

Reviewers apply these rules to pull requests that change `lib/saluki-env`:

<!-- review:saluki-env -->
Apply these rules to files under `lib/saluki-env`. This crate reads cgroups, procfs, and containerd to get workload
metadata, and it converts autodiscovery configurations. Bugs here are often silent. The result is missing container
tags, not an error.

- A cgroup can disappear during a walk of the hierarchy. Below the root, skip a path that the code can't read, and don't
  stop the walk with `?`. Remove aliases only after a walk where `TraversalResult::is_complete` is true. A path that no
  longer exists, or that the code could never read, keeps the walk complete on purpose. (A removed cgroup stopped a
  walk, and the poller removed the aliases of running containers: #2460.)
- A line in `/proc/<pid>/cgroup` can hold a relative path, such as `0::/../docker-<id>.scope`. Flag code that joins such
  a path to the cgroupfs root. The container ID can also be in an ancestor, such as `<id>.scope/init`.
  `try_cgroup_from_path` and `get_cgroup_by_pid` get the ID from `extract_container_id_from_path`, which checks the
  ancestors. Flag a change that makes either one match only the leaf name. (The code matched only the leaf name, so
  workloads in nested cgroups got no container: #2464.)
- The lookup of the process's own container reads `/proc/self/cgroup` and `/sys/fs/cgroup`, not the configured roots,
  and `get_container_id_from_cgroup_lines` matches only the leaf name. Don't flag these choices, which are on purpose.
  In a private cgroup namespace, `/proc/self/cgroup` reads `0::/`, so under cgroups v2, `resolve_self_container` falls
  back to the controller inode. (ADP's own telemetry had no container tags: #2550.)
- The procfs and cgroupfs filesystems are separate mounts. Choose each root from its own configured value, then its own
  feature: the cgroupfs root from `Feature::HostMappedCgroupfs`, not from `Feature::HostMappedProcfs`. If the mount list
  shows a hierarchy twice, use the mount under the configured cgroupfs root. (The cgroupfs root followed the procfs
  feature: #2549.)
- A collector that emits `MetadataOperation::add_alias` also emits `remove_alias` or `delete` when the cgroup, task, or
  container no longer exists. This includes the PIDs that an initial scan adds. Stale aliases keep their container IDs
  in the fixed-size `GenericMapInterner`, and when it's full, container IDs and origin tags fail to intern. (The cgroups
  collector never removed aliases, and the interner became full on busy nodes: #772.)
- A containerd call whose response grows with the number of containers, such as a list, sets
  `max_decoding_message_size`. (Container list responses reached about 15 MB, over the 4 MiB default: #881.)
- A conversion between two autodiscovery types that this crate defines, such as `From<Config> for CheckConfig`, maps
  each field of its source. Match the source with a struct pattern that names each field and has no `..`, and bind a
  field that the target doesn't use as `field: _`. Then a new field that nobody maps makes the build fail.
  (`CheckConfig` had 4 of the 16 fields of `Config`: #2424.)
- A test of behavior that depends on detected features builds the detector with
  `FeatureDetector::from_detected_features`. A test that reads cgroupfs or procfs uses a `tempdir` fixture, not the real
  `/proc` or `/sys/fs/cgroup`. A path string that the code only parses is fine.
<!-- /review -->

### Agent Data Plane

Reviewers apply these rules to pull requests that change `bin/agent-data-plane`:

<!-- review:agent-data-plane -->
Apply these rules to files under `bin/agent-data-plane`.

- `create_topology` in `src/cli/run.rs` adds different components for each combination of pipeline settings. Examples
  are DogStatsD only, OTLP only, OTLP proxy mode, standalone mode, and DogStatsD with and without the no-aggregation
  pipeline. In each combination that the change affects, the blueprint adds each component once and connects all of
  them. Each ID passed to the `connect_components` methods, such as `dsd_in.metrics`, names a component and output that
  the combination adds. ADP finds these errors only when it starts, and then exits. (Enabling OTLP made ADP fail to
  start: #1061.)
- Don't ask for a unit test of the wiring in `create_topology`. If no case under `test/` starts ADP with a combination
  that the change affects, ask for an integration test case.
- Flag a stage that records state, such as a first-seen set, before a stage that can drop the event. Flag a filter that
  runs before the mapping or split that it depends on. A destination that measures input, such as `dsd_stats_out` on
  `dsd_in.metrics`, connects to the raw source output. (Onboarding tags ran before the sampler, so services whose first
  trace it dropped never showed as onboarded: #3131.)
- In `src/main.rs`, in `AdpLocalConfig`, and in `handle_run_command` before `RemoteAgent::start` returns, ADP has only
  the bootstrap configuration from `datadog.yaml` and the environment. Flag a decision there on a key that the Core
  Agent can set, such as `data_plane.enabled`. That key keeps its default there unless a local source sets it. Read the
  key from `agent.config()` after `start` returns. Logging is the exception, because `start` reloads it from the Core
  Agent configuration. The `config`, `debug`, and `dogstatsd` subcommands read only the local configuration by design.
- A command that runs until a signal stops it waits on `saluki_app::util::wait_for_shutdown_signal`, not on
  `tokio::signal::ctrl_c` alone. Process managers and container runtimes stop ADP with SIGTERM, or with
  `CTRL_BREAK_EVENT` on Windows. (`systemctl stop` killed ADP before it drained the topology: #2322.)
- Flag a new `std::process::exit` call in a subcommand handler. It exits without running `Drop`, so ADP can lose the
  `error!` line before it. Return a `Result` or an exit code from the handler, and pass it through `run_inner` to
  `async_main`. `async_main` exits only after it drops the `BootstrapGuard`. In a handler that already calls
  `std::process::exit`, report it as Not blocking. (Disabled ADP sometimes exited before it flushed its logs: #1201.)
- The remap rules in `src/state/metrics/rules/` and the status sections in `src/internal/status.rs` match tag values
  that come from component IDs, such as `component_id:dsd_agg` and `resolver_id:dsd_in/dsd/primary`. Update the rules
  and sections that match a component ID or a resolver ID that the change renames. Also update them when the change
  moves a counter to another component, such as from `dsd_agg` to `dsd_no_agg_split`. This rule also applies to these
  changes in `lib/saluki-components`, such as a new resolver name. (A resolver ID change left the
  `aggregator.dogstatsd_contexts` rule with no match: #715.)
- Flag a remap rule that counts series that measure something else in an Agent metric, such as
  `purpose:api_key_validation` requests. Reject those series with `rejecting_tags` on every rule for that name. A
  rejected series goes to the next rule for its name. Don't flag a rule that merges counters across components or
  listeners to match the Agent's tags. A merged gauge keeps only its last value, so flag a rule that merges gauge
  series. (The transaction rules counted API key validation errors in `transactions.errors`: #3031.)
- Flag a remap rule that adds a `remote_agent` tag. The Remote Agent Registry adds that tag.
- A metric that a remap rule or a status section reads doesn't register at `debug` or `trace`, for example with
  `register_debug_counter`. `metrics_level` defaults to `info`, so such a metric doesn't reach its readers. (Tag
  filterlist counters that registered at `debug` were missing in production: #1822.)
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
