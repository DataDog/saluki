# Workspace devcontainer

This devcontainer is designed to work with [Datadog Workspaces](https://datadoghq.atlassian.net/wiki/x/-ITIpQ).

## User's guide

### Creating a workspace

The simplest way to create a workspace is:

```sh
workspaces create {WORKSPACE_NAME} --repo Datadog/saluki
```

This defaults to using the `.devcontainer/datadog/default` dev container.

### Tooling

ADP workspaces come pre-setup with:

- Rust stable toolchain (exact version pinned in `rust-toolchain.toml`)
- protoc v29.3
- Cargo tools: cargo-binstall, cargo-deny, cargo-hack, cargo-nextest,
  cargo-autoinherit, cargo-sort, dd-rust-license-tool, dummyhttp,
  cargo-machete, rustfilt

## Maintainer's guide

### Architecture

- `Dockerfile` -- GBI Ubuntu 24.04 base image with system packages and protoc
- `features/adp/` -- local devcontainer feature that installs Rust and cargo
  tools as the `bits` user
  - `install.sh` -- bakes rustup + cargo-binstall into the image
  - `lifecycle/postCreate.sh` -- runs after repo clone; installs the pinned
    Rust toolchain (`rustup show`) and cargo tools (`make cargo-preinstall`)
- `prebuild-devcontainer.json` -- full build configuration (edit this to make changes)
- `devcontainer.json` -- points to the pre-built image SHA; updated automatically
  by the Workspaces campaigner after each change to `prebuild-devcontainer.json`

### Devcontainer pre-building

`devcontainer.json` references a pre-built image rather than building from
source at workspace-creation time. This avoids the 10-minute Workspaces
creation timeout. For more information, see the
[documentation](https://datadoghq.atlassian.net/wiki/spaces/DEVX/pages/4194009834/Creating+Specialized+Dev+Containers+and+Features#How-Can-a-Dev-Container-Launch-Faster).

Any branch push that touches `prebuild-devcontainer.json` triggers the
Workspaces campaigner to build a new image and open a follow-up PR on that
branch updating the SHA in `devcontainer.json`. Merge the campaigner PR before
merging the branch. Monitor `#workspaces-ops` for build failures.

### Testing devcontainer changes

In a workspace:

```sh
cd ~/dd/saluki
devcontainer build --config .devcontainer/datadog/default/prebuild-devcontainer.json --config-file-override prebuild-devcontainer.json
# at the end, it will print the sha of the generated container image
```

If this builds, you can explore the container with:

```sh
docker run -it --rm --user=bits <image-id> /bin/bash -l
```

Verify inside the container:

```sh
whoami            # should be "bits"
protoc --version  # should be libprotoc 29.3
rustc --version   # should show stable
```

### Testing the full workspace flow

After the campaigner PR updates `devcontainer.json` with the real SHA:

```sh
workspaces create --devcontainer-config .devcontainer/datadog/default/devcontainer.json
```

### Checklist in a new workspace

```sh
whoami
protoc --version
rustc --version

cd ~/dd/saluki
# following steps should be no-ops
# (they were already done at postCreate)
make check-rust-build-tools
make cargo-preinstall
# check that it does compile
cargo build
```

### Troubleshooting

#### "Complete the login via your OIDC provider" and the build hangs

The build is stuck because the workspace is not authorized to access our registry.

- Stop the build
- Trigger interactive authorization:

```sh
docker pull registry.ddbuild.io/images/base/gbi-ubuntu_2404:release
```

- Follow the browser instructions, then restart the build
