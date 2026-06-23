# Nightly testing against the latest Datadog Agent

ADP pins a released Datadog Agent version for its end-to-end suites (`PUBLIC_DD_AGENT_VERSION`
and the Windows base image). That pin only moves when the automated Agent version bump lands, so an
incompatibility between ADP `main` and Agent `main` — a changed config-stream key, a new IPC
behavior, a tightened platform gate — is not discovered until the bump, well after it was
introduced upstream.

The nightly schedule closes that gap: it runs the correctness and integration suites against the
**latest `main` Agent dev images** so incompatibilities surface within a day.

## How it works

A nightly run is an ordinary scheduled pipeline with the variable `AGENT_NIGHTLY=true`. Following
the existing scheduled-pipeline pattern (see `.gitlab/fuzz.yml`), the schedule runs the full
pipeline; the only difference is which Agent image the end-to-end jobs build against:

| Job | Default base image | Nightly base image |
|-----|--------------------|--------------------|
| `build-bundled-agent-adp-image` (Linux correctness + integration) | `registry.datadoghq.com/agent:${PUBLIC_DD_AGENT_VERSION}` | `datadog/agent-dev:master-py3-full` |
| `test-integration-windows-amd64` | `registry.datadoghq.com/agent:<version>-ltsc2022` | `datadog/agent-dev:main-py3-jmx-win-servercore-ltsc2022` |

Both are floating tags that track the latest `main` Agent build. The Linux image is the `-full`
variant (Agent + DDOT), so the OTLP DDOT baseline tests are exercised too.

The macOS integration leg installs the Agent from a pinned DMG and is **not** switched to a nightly
build; it continues to run against the pinned release.

## Setting up the schedule

Create the schedule once in the GitLab project (**Build → Pipeline schedules → New schedule**):

- **Target branch**: `main`
- **Interval**: daily (off-peak)
- **Variable**: `AGENT_NIGHTLY` = `true`

The schedule owner is responsible for watching the results; failure notifications are not wired up
yet (a Slack alert is a planned follow-up).

## Overriding the dev image registry

The defaults pull from Docker Hub (`datadog/agent-dev`). If the runners cannot reach Docker Hub —
ADP CI otherwise uses `registry.ddbuild.io` mirrors — set the image explicitly on the schedule
without changing code:

- `NIGHTLY_AGENT_IMAGE` overrides the Linux base image.
- `NIGHTLY_WINDOWS_AGENT_IMAGE` overrides the Windows base image.

## Prerequisites

- `datadog/agent-dev:master-py3-full` must be published. It is added by
  [DataDog/datadog-agent#52666](https://github.com/DataDog/datadog-agent/pull/52666); this schedule
  should only be enabled once that has merged and published.
