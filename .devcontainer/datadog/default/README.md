# Workspace devcontainer

This devcontainer is designed to work with [Datadog Workspaces](https://datadoghq.atlassian.net/wiki/x/-ITIpQ).

## User's guide

### Creating a workspace

The simplest way to create a workspace is:

```
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
- `devcontainer.json` -- ties it all together with Workspaces `base` and
  `claude-code` features

### Testing devcontainer changes

In a workspace:

```sh
cd ~/dd/saluki
devcontainer build --config .devcontainer/datadog/default/devcontainer.json .
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

For a fuller test including the `postCreateCommand` (cargo tools installation),
use `devcontainer run`:

```sh
devcontainer run --config .devcontainer/datadog/default/devcontainer.json .
```

This will start the container, run the lifecycle commands (including
`make cargo-preinstall`), and keep it running. Press `^C` to stop.

### Troubleshooting

**"Complete the login via your OIDC provider" and the build hangs**

The build is stuck because the workspace is not authorized to access our registry.
- Stop the build
- Trigger interactive authorization:
```sh
docker pull registry.ddbuild.io/images/base/gbi-ubuntu_2404:release
```
- Follow the browser instructions, then restart the build
