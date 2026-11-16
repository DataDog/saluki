# Datadog Agent pin

Saluki builds with and tests against one Datadog Agent build, pinned in `.agent-pin.json` at the repository root. The
pin is the only committed Agent version for builds and tests. CI jobs, Make targets, and Dockerfile build arguments
all derive their Agent inputs from it.

Two settings stay outside the pin: the scheduled `AGENT_NIGHTLY` pipeline keeps testing against floating Agent images,
and the SMP benchmark (`.gitlab/benchmark.yml`) keeps its own `PUBLIC_DD_AGENT_VERSION`.

## Versions

| Term | Meaning |
|---|---|
| Integration version | The Agent that CI builds with and tests against: Linux and Windows images, macOS packages, and debug packages from one Agent commit. The pin's `integration` section. |
| Declared version | The `DD_AGENT_VERSION` compiled into the Linux ADP build, which drives version gates. On `main` it's the Integration version. On a release branch it's the target minor's patch zero, such as `7.87.0`, whatever the Integration version is. |
| Target minor | The Agent minor that an ADP release line supports, in the pin's `target` field. Only release branches have it. See [Releasing ADP](../agent-data-plane/releasing.md). |

Only the jobs that compile the Linux ADP image, and the release tarballs built from it, set `DD_AGENT_VERSION`. Unit
test and lint jobs don't, and Make doesn't export it. Native macOS and Windows ADP builds don't set it either, so their
version gates select the newest behavior.

## What a pin contains

Every pin is complete. It has:

- the Linux multi-platform image, by index digest, with its `amd64` and `arm64` child digests
- the Windows `ltsc2022` server core image with JMX, by child digest
- the macOS DMGs for `arm64` and `x86_64`, with SHA-256 checksums
- the Agent debug packages for `amd64` and `arm64`, with SHA-256 checksums

All artifacts come from one Agent commit. A nightly pin also records the upstream pipeline ID, which appears in the
nightly version string. Package URLs must be the long-lived locations derived from the version; the expiring macOS
`ci/` packages are rejected. `built_at` is the creation time of the Linux `amd64` image, and the pin's age is measured
from it.

`ci/tooling/agent_pin.py` holds the exact schema and rules. To validate the pin, run:

```shell
make check-agent-pin
```

Consumers pull images by digest and verify each package's checksum before they use it, including cached copies. A
mismatch or a failed pull fails the job.

## Reading the pin

CI reads the checked-out pin in job scripts: `jq` on Linux and `ConvertFrom-Json` on Windows. Make recipes read it
with `ci/tooling/agent_pin.py` only when they need it, so targets such as `make fmt` work without it. The reader
validates the pin before it prints anything:

```shell
python3 ci/tooling/agent_pin.py get integration.linux.image   # one field
python3 ci/tooling/agent_pin.py declared-version              # the Declared version
python3 ci/tooling/agent_pin.py age                           # age, measured from built_at
```

## Using the pin locally

Dockerfiles can't read the pin before `FROM`, so they take the resolved values as build arguments and fail when one
is missing. Use these Make targets, which pass the pinned values for you:

| Command | Uses |
|---|---|
| `make build-datadog-agent-image`, `make build-datadog-agent-image-release` | The pinned Linux image, as the base of the bundled Agent and ADP image |
| `make provision-macos-test-env` | The pinned macOS DMG for the host architecture, installed into a sandbox under `/tmp` |
| `make antithesis-build`, `make antithesis-build-differential` | The bundled image, and the pinned `amd64` debug package for the Antithesis scenarios |

The macOS install writes a marker with the version, URL, and checksum after a successful install. Later runs reuse the
installation only when the marker matches the pin and the Agent executable exists.

To build an Antithesis scenario with Compose directly, export the debug package first:

```shell
export AGENT_DEBUG_DEB_URL=$(python3 ci/tooling/agent_pin.py get integration.antithesis_debug_debs.amd64.url)
export AGENT_DEBUG_DEB_SHA256=$(python3 ci/tooling/agent_pin.py get integration.antithesis_debug_debs.amd64.sha256)
docker compose -f test/antithesis/scenarios/general/docker-compose.yaml build
```

## Updating the pin

Nobody edits the pin's artifact fields by hand. `ci/tooling/discover_agent_pin.py` finds a complete artifact set,
downloads and hashes every package, and writes a new pin. Automation and people use the same tool, and a failed or
partial discovery never writes a pin.

### On `main`

The **Update Agent Nightly** workflow (`.github/workflows/update-agent-nightly.yml`) looks for a newer Agent `main`
nightly. It runs daily at 12:30 UTC and on manual dispatch. It resolves the floating Windows nightly
image once, then requires the Linux nightly built from the same commit and pipeline, and all four packages. The
candidate must have a higher pipeline ID than the merged pin and an Agent commit equal to or ahead of it.

The workflow keeps one bot branch, `ci/update-agent-nightly`, and at most one open pull request. It refreshes that pull
request only while the pull request holds nothing but the automated pin update, and it moves the bot branch only by
fast-forward, so a push that arrives during a run makes the run fail rather than overwriting it. If the new Agent needs
an ADP change, push it to the bot branch: the pull request then keeps its candidate, and the workflow proposes nothing
newer until it merges or closes.

The Windows nightly is published under a floating tag, so a missed run can't find an older Windows build later. To
bootstrap the pin or recover a missed nightly, take the `ltsc2022` child digest from the upstream Windows build job's
push log and pass it as the workflow's `windows_digest` input, or run the tool yourself:

```shell
python3 ci/tooling/discover_agent_pin.py nightly --output-dir /tmp/agent-pin --windows-digest sha256:<digest>
cp /tmp/agent-pin/agent-pin.json .agent-pin.json
```

Every run reports the merged pin's age, even when it finds nothing newer. Nothing alerts on a stale pin.

### On release branches

The **Update Agent Release Pins** workflow (`.github/workflows/update-agent-version.yml`) runs weekly from `main` over
the release lines listed in its `MAINTAINED_LINES`. While that list is empty, it changes nothing. For each line that has
adopted the pin, it proposes the newest complete RC or stable Agent release within the line's target minor, in a pull
request against that branch. It changes only `integration`, never `target`. [Releasing
ADP](../agent-data-plane/releasing.md#agent-updates-within-a-release-line) describes the order it follows.

## Checks

| Check | Where | What it enforces |
|---|---|---|
| **Agent pin** workflow | Every pull request | The pin is valid. `main` takes a nightly without `target`. A release branch that has the pin keeps it and keeps its `target`, which must map to the line. |
| `check-agent-pin` GitLab job | Every pipeline | The committed pin is valid, and the tests for the pin tooling pass. |
| **Backport** workflow | Backports | A backport doesn't change `.agent-pin.json`. |
| **Bump ADP Version** workflow | `X.Y.0` tags | The tag is on `releases/X.Y.x` and declares the line's target. |
| `check-release-agent-pin` GitLab job | Release tags | Before publishing: the tag is on its release branch, the ADP version matches the tag, and the pin declares the line's target. |

The checks read the destination branch separately from the proposed change, so deleting the pin or `target` doesn't
exempt a release line that has adopted the pin.

## The `AGENT_NIGHTLY` pipeline

The scheduled pipeline with `AGENT_NIGHTLY` set keeps its floating images as an early warning of Agent changes:

- Linux uses `${NIGHTLY_AGENT_IMAGE:-datadog/agent-dev:master-py3-full}`.
- Windows uses `${NIGHTLY_WINDOWS_AGENT_IMAGE:-datadog/agent-dev:main-py3-jmx-win-servercore-ltsc2022}`.
- The Linux ADP build compiles with `DD_AGENT_VERSION=nightly`.
- macOS uses the pin.

It posts its result to Slack, so don't set `AGENT_NIGHTLY` on a test branch.
