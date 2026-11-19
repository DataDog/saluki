# Datadog Agent version

Saluki builds with and tests against one Datadog Agent build, recorded in `.datadog-agent-version.json` at the
repository root. This page covers that file, the CI jobs and local commands that use it, how it's updated, and the
version gates that switch ADP behavior by Agent version.

The file is the only committed Agent version for builds, tests and benchmarks. CI jobs, Make targets, and Dockerfile
build arguments all derive their Agent inputs from it. Only the scheduled `AGENT_NIGHTLY` pipeline stays outside it,
and keeps testing against floating Agent images. The SMP benchmark (`.gitlab/benchmark.yml`) runs its baseline and
comparison with the Agent in the file at the pipeline's commit, so the comparison measures only the ADP change.

## Versions

| Term | Meaning |
|---|---|
| Integration version | The Agent that CI builds with and tests against: Linux and Windows images, macOS packages, and debug packages from one Agent commit. The file's `integration` section. |
| Declared version | The `DD_AGENT_VERSION` compiled into the Linux ADP build, which drives version gates. On `main` it's the Integration version. On a release branch it's the target minor's patch zero, such as `7.87.0`, whatever the Integration version is. |
| Target minor | The Agent minor that an ADP release line supports, in the file's `target` field. Only release branches have it, and it must match the line's entry in `.datadog-agent-release-lines.jsonl`. See [Releasing ADP](../agent-data-plane/releasing.md#release-lines-and-their-target). |

Only the jobs that compile the Linux ADP image, and the release tarballs built from it, set `DD_AGENT_VERSION`. Unit
test and lint jobs don't, and Make doesn't export it. Native macOS and Windows ADP builds don't set it either, so their
version gates select the newest behavior.

## What `target` changes

`main` has no `target`. A release branch adds one when it's cut. The tooling in `ci/tooling` reads the field and
behaves differently with it:

| | `main`, no `target` | Release branch, `"target": "7.87"` |
|---|---|---|
| `DD_AGENT_VERSION` for the Linux ADP build | The Integration version, such as `7.87.0-devel+git.394.462b537.pipeline.151347634` | `7.87.0` |
| Version gates | The newest behavior: a development version passes every gate | The behavior of Agent 7.87 |
| Allowed Integration version | A nightly | A nightly, RC, or stable release, but only from 7.87 |
| Updated by | **Update Agent Nightly**, to newer nightlies | **Update Agent Release Version**, to newer 7.87 RCs, then stable releases |
| Pull request check | Rejects `target` and anything but a nightly | Requires `target` to match the line's release-lines entry, and rejects any change to it or removal of the file |
| Release tag check | None | The tag must be on the branch and declare the line's target |

In code, `declared_version`, `check_change`, and `check_release_tag` in `ci/tooling/agent_version.py` make these
choices, and `discover_agent_version.py release` refuses to run without a target.

## What the file contains

Every version file is complete. It has:

- the Linux multi-platform image, by index digest, with its `amd64` and `arm64` child digests
- the Windows `ltsc2022` server core image with JMX, by child digest
- the macOS DMGs for `arm64` and `x86_64`, with SHA-256 checksums
- the Agent debug packages for `amd64` and `arm64`, with SHA-256 checksums

All artifacts come from one Agent commit, `git_revision`. A nightly also records the upstream pipeline ID, which
appears in the nightly version string. Package URLs must be the long-lived locations derived from the version; the
expiring macOS `ci/` packages are rejected. `built_at` is the creation time of the Linux `amd64` image, and the
Integration version's age is measured from it.

`ci/tooling/agent_version.py` holds the exact schema and rules. To validate the file, run:

```shell
make check-agent-version
```

Consumers pull images by digest and verify each package's checksum before they use it, including cached copies. A
mismatch or a failed pull fails the job.

## Reading the file

CI reads the checked-out file in job scripts: `jq` on Linux and `ConvertFrom-Json` on Windows. Make recipes read it
with `ci/tooling/agent_version.py` only when they need it, so targets such as `make fmt` work without it. The reader
validates the file before it prints anything:

```shell
python3 ci/tooling/agent_version.py get integration.linux.image   # one field
python3 ci/tooling/agent_version.py declared-version              # the Declared version
python3 ci/tooling/agent_version.py age                           # age, measured from built_at
```

## Using the Agent locally

Dockerfiles can't read the file before `FROM`, so they take the resolved values as build arguments and fail when one
is missing. Use these Make targets, which pass the values for you:

| Command | Uses |
|---|---|
| `make build-datadog-agent-image`, `make build-datadog-agent-image-release` | The Linux image, as the base of the bundled Agent and ADP image |
| `make provision-macos-test-env` | The macOS DMG for the host architecture, installed into a sandbox under `/tmp` |
| `make antithesis-build`, `make antithesis-build-differential` | The bundled image, and the `amd64` debug package for the Antithesis scenarios |

The macOS install, `ci/tooling/install_macos_agent.py`, writes a marker with the version, URL, and checksum after a
successful install. Later runs reuse the installation only when the marker matches the file and the Agent executable
exists. The macOS integration test jobs in CI use the same Make target.

To build an Antithesis scenario with Compose directly, export the debug package first:

```shell
export AGENT_DEBUG_DEB_URL=$(python3 ci/tooling/agent_version.py get integration.antithesis_debug_debs.amd64.url)
export AGENT_DEBUG_DEB_SHA256=$(python3 ci/tooling/agent_version.py get integration.antithesis_debug_debs.amd64.sha256)
docker compose -f test/antithesis/scenarios/general/docker-compose.yaml build
```

## Updating the file

Nobody edits the artifact fields by hand. `ci/tooling/update-agent-version/discover_agent_version.py` finds a complete
artifact set, downloads and hashes every package, and writes a new file. Automation and people use the same tool, and
a failed or partial discovery never writes a file.

### Where discovery finds the artifacts

Discovery starts from an Agent version and derives every other location from it:

| Artifact | Nightly | RC | Stable release |
|---|---|---|---|
| Linux image | `datadog/agent-dev:nightly-full-main-<sha8>-jmx` | `registry.datadoghq.com/agent:<version>-full` | Same as RC |
| Windows image | The `ltsc2022` child of the floating `datadog/agent-dev:nightly-py3-jmx-win-servercore` | The `ltsc2022` child of `registry.datadoghq.com/agent:<version>-servercore-ltsc2022-jmx` | Same as RC |
| macOS DMGs | `s3.amazonaws.com/dd-agent-macostesting/nightly/` | `s3.amazonaws.com/dd-agent-macostesting/beta/` | `s3.amazonaws.com/dd-agent/` |
| Debug packages | `apt.datad0g.com` | `apt.datad0g.com` | `apt.datadoghq.com` |

For a nightly, the version comes from the Windows image's labels. For an RC or stable release, it comes from the
Agent's Git tags. Discovery resolves each image tag to a digest and requires the image's version and revision labels
to match. It then downloads each package and records its SHA-256. If an artifact isn't published yet, it skips that
version, and a later run tries again.

### On `main`

The **Update Agent Nightly** workflow (`.github/workflows/update-agent-nightly.yml`) looks for a newer Agent `main`
nightly. It runs daily at 12:30 UTC and on manual dispatch. For each candidate it requires the Linux nightly built from
the same commit and pipeline as the Windows image, and all four packages. The candidate must have a higher pipeline ID
than the current nightly and an Agent commit equal to or ahead of it.

The Windows nightly is published only under a floating tag, which can move before a build's packages are published.
So the workflow also runs hourly to record the build the tag points at, and keeps the recorded builds in a GitHub
Actions cache entry. The daily run checks every recorded build newer than the current nightly, newest pipeline first,
and proposes the first complete one. A build is checked again on later runs until 7 days after it was first recorded, or
until a newer nightly merges. Losing the cache entry means only the floating tag's build is checked until the next runs record more.

The workflow keeps one bot branch, `ci/update-agent-nightly`, and at most one open pull request. It refreshes that pull
request only while the pull request holds nothing but the automated update. Each refresh is based on the current
`main`, and it moves the bot branch only by fast-forward, so a push that arrives during a run makes the run fail rather
than overwriting it. If the new Agent needs an ADP change, push it to the bot branch: the pull request then keeps its
candidate, and the workflow proposes nothing newer until it merges or closes. An open pull request also keeps its
candidate when a run finds only an older complete nightly. After a pull request closes without
merging, the next run restarts the bot branch from `main` and opens a new pull request. If `main`'s version file changes
while a run is in progress, that run proposes nothing, and the next run discovers again.

A build that no run recorded can't be found later from the floating tag. To bootstrap the file or recover such a
nightly, take the `ltsc2022` child digest from the upstream Windows build job's push log and pass it as the workflow's
`windows_digest` input, or run the tool yourself:

```shell
python3 ci/tooling/update-agent-version/discover_agent_version.py nightly --output-dir /tmp/agent-version \
  --windows-digest sha256:<digest>
cp /tmp/agent-version/datadog-agent-version.json .datadog-agent-version.json
```

Every run reports the current nightly's age, even when it finds nothing newer. Nothing alerts on an old nightly.

### On release branches

The **Update Agent Release Version** workflow (`.github/workflows/update-agent-version.yml`) runs weekly from `main`
over the release lines listed in its `MAINTAINED_LINES`. While that list is empty, it changes nothing. For each line
that has adopted the file, it proposes the newest complete RC or stable Agent release within the line's target minor,
in a pull request against that branch. It changes only `integration`, never `target`. [Releasing
ADP](../agent-data-plane/releasing.md#agent-updates-within-a-release-line) describes the order it follows, and
[Cutting a new release line](../agent-data-plane/releasing.md#cutting-a-new-release-line) covers the first version of
a new line.

### When an update workflow fails

Discovery separates two outcomes:

- A **skip** is normal. No build is both newer and complete, or the only build came from an Agent release branch.
  The run passes, says why in its summary, and changes nothing.
- An **error** means something unexpected: a registry that keeps failing past the retry budget, a malformed response,
  or an artifact whose name, labels, or platforms don't match what the tooling knows. The run fails, and the error at
  the top of the run names the artifact, what it found, and what it expected.

Errors usually mean the Agent changed how it names or labels its artifacts. Fix the matching constant or pattern:
`NIGHTLY_VERSION_RE` and the URL helpers in `ci/tooling/agent_version.py`, or the tag names at the top of
`discover_agent_version.py`. The tests next to each file cover the formats they expect. A registry error is often
transient: rerun the workflow before changing code.

The workflows can't post to Slack themselves. To get a message for each scheduled run, subscribe a channel through
Slack's GitHub integration:

```text
/github subscribe DataDog/saluki workflows:{name:"Update Agent Nightly" event:"schedule"}
/github subscribe DataDog/saluki workflows:{name:"Update Agent Release Version" event:"schedule"}
```

## Version gates

A version gate switches between old and new Agent behavior, based on the Declared version that ADP was built with.
`datadog_agent_commons::agent_version::meets(major, minor, patch)` is true when the Declared version is at least that
version, and also for development versions and an unset version. On `main` the Declared version is a nightly, so
every gate selects the newest behavior. On a release branch it's the line's target minor, such as `7.87.0`, even when
CI tests a later RC or patch.

For example, `LEGACY_TAG_LINKS_AND_EVENTS` in `lib/saluki-components/src/common/otlp/traces/transform.rs` is
`!meets(7, 87, 0)`. A release line that targets 7.86 writes OTLP span links and events as legacy tags. `main` and lines
that target 7.87 or later write them as native span links and events.

- Gate on Agent minor versions, such as `meets(7, 87, 0)`. Don't add patch-level gates.
- When an Agent change affects what correctness tests compare, put the matching ADP change in the pull request that
  updates the nightly, so the code and the test Agent advance together.
- Delete a gate only when every maintained release line declares at least its threshold. Reaching that version on
  `main` isn't enough, and a maintained line that hasn't adopted the file still counts.

## Checks

| Check | Where | What it enforces |
|---|---|---|
| **Agent version** workflow | Every pull request | The file and `.datadog-agent-release-lines.jsonl` are valid. `main` takes a nightly without `target` and maps its ADP line. A release branch that has the file keeps it and keeps its `target`, which must match the line's release-lines entry. |
| `check-agent-version` GitLab job | Every pipeline | The committed file is valid, and the tests for the tooling pass. |
| **Backport** workflow | Backports | A backport doesn't change `.datadog-agent-version.json`. |
| **Bump ADP Version** workflow | `X.Y.0` tags | The tag is on `releases/X.Y.x` and declares the line's target. |
| `check-release-agent-version` GitLab job | Release tags | Before publishing: the tag is on its release branch, the ADP version matches the tag, and the file declares the line's target. |

The checks read the destination branch separately from the proposed change, so deleting the file or `target` doesn't
exempt a release line that has adopted it.

## The `AGENT_NIGHTLY` pipeline

The scheduled pipeline with `AGENT_NIGHTLY` set keeps its floating images as an early warning of Agent changes:

- Linux uses `${NIGHTLY_AGENT_IMAGE:-datadog/agent-dev:master-py3-full}`.
- Windows uses `${NIGHTLY_WINDOWS_AGENT_IMAGE:-datadog/agent-dev:main-py3-jmx-win-servercore-ltsc2022}`.
- The Linux ADP build compiles with `DD_AGENT_VERSION=nightly`.
- macOS uses `.datadog-agent-version.json`.

It posts its result to Slack, so don't set `AGENT_NIGHTLY` on a test branch.
