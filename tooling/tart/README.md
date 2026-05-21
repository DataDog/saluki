# Tart-based macOS test wrapper

This directory contains an optional wrapper that runs Saluki's native macOS
integration tests inside an ephemeral [Tart](https://tart.run/) virtual machine.

## When to use this

You want to use the Tart wrapper when:

- You are developing on a Mac and want to run macOS integration tests in
  isolation from your host (avoid port conflicts, file path collisions, or
  leftover state from a previous test run).
- You want a pristine macOS environment for each test run.

You do **not** need the Tart wrapper when:

- Running on a bare-metal CI runner that is already a dedicated macOS host.
  CI should invoke `make test-integration-macos` directly.
- Running on Linux or Windows. Tart requires an Apple Silicon macOS host.

## Layering

The Tart wrapper sits **above** the test framework. Panoramic and the native
runtime know nothing about VMs — they spawn ADP as a native macOS process in
whatever macOS environment they are running in. The Tart wrapper just produces
a fresh-looking macOS environment, runs the existing test command inside it,
and tears it down.

```
make test-integration-macos-tart
        │
        ▼
tooling/tart/run-in-vm.sh
  • boot ephemeral macOS VM
  • mount the repo into the VM
  • run: make test-integration-macos    (inside the VM)
  • stop + delete the VM
        │
        ▼
make test-integration-macos             (the existing bare-metal target)
  • build-panoramic, build-adp-native   (on the host, before VM boots)
  • panoramic run -t basic-startup/native_macos
```

Because the repo is shared into the VM via `tart --dir`, the binaries built on
the host (Apple Silicon Mach-O) run directly inside the VM (also Apple Silicon)
without rebuilding. Test logs written by panoramic land on the host because
they live on the shared mount.

## Prerequisites

- macOS host (Apple Silicon recommended)
- `tart` installed. Either:
  - `brew install cirruslabs/cli/tart`, or
  - Download from <https://tart.run/>
- About 35 GB of free disk space (~30 GB base image + ~5 GB headroom)

The wrapper uses [`tart exec`](https://tart.run/quick-start/#run-virtual-machine)
to run commands inside the guest, which avoids any SSH / password handling.
This requires the Tart Guest Agent to be present inside the VM. The default
Cirrus Labs base image (`ghcr.io/cirruslabs/macos-sequoia-base:latest`) ships
with the agent preinstalled.

## Usage

From the repo root:

```sh
make test-integration-macos-tart
```

To run a specific case:

```sh
make test-integration-macos-tart CASE=basic-startup/native_macos
```

To run an arbitrary command inside a VM (useful for debugging the VM
environment itself):

```sh
tooling/tart/run-in-vm.sh sh -c 'uname -a && sw_vers'
```

## First-run cost

The first invocation pulls the base image, which is roughly 30 GB and can take
10+ minutes depending on network speed. Subsequent runs reuse the cached image
and complete in roughly:

- 60 seconds VM boot
- 15 seconds test
- 5 seconds VM teardown

…so plan on ~90 seconds per test run after the first.

## Environment variables

The wrapper script honors:

| Variable | Default | Purpose |
| -------- | ------- | ------- |
| `TART_BASE_IMAGE` | `ghcr.io/cirruslabs/macos-sequoia-base:latest` | Base VM image to clone from |
| `TART_VM_NAME` | `saluki-integration-<pid>` | Name of the ephemeral VM |
| `TART_SHARED_NAME` | `saluki` | Mount tag for the shared repo directory |
| `TART_BOOT_TIMEOUT` | `180` | Seconds to wait for the VM to become reachable |
| `TART_KEEP_VM` | `0` | Set to `1` to keep the VM on disk after the run (useful for debugging) |

## Cleanup

The wrapper installs cleanup traps on `EXIT`, `INT`, and `TERM`, so the VM is
stopped and deleted on normal exit, ^C, and most signal paths.

If a previous run was force-killed (`SIGKILL`) and left a VM behind, you can
list and remove stale VMs manually:

```sh
tart list
tart stop saluki-integration-NNNN || true
tart delete saluki-integration-NNNN
```

## Troubleshooting

- **`error: tart is not installed`** — install via `brew install cirruslabs/cli/tart`.
- **First pull is slow** — that is expected; the base image is ~30 GB. It is
  cached after the first run.
- **`VM did not become reachable within 180s`** — bump `TART_BOOT_TIMEOUT`, or
  inspect with `tart list` to see if the VM is actually running. The Tart
  Guest Agent inside the VM takes some seconds to start after boot.
- **Stuck VMs after force-kill** — see the cleanup section above.
