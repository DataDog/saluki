# DogStatsD Windows Named Pipe Implementation Plan

> **For automated workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add Windows DogStatsD named-pipe intake compatible with the core Agent's `dogstatsd_pipe_name` and `dogstatsd_windows_pipe_security_descriptor` behavior.

**Architecture:** Add a reusable `ListenAddress::NamedPipe` and Windows-only named-pipe listener support to `saluki-io`, then wire DogStatsD config to create that listener when `dogstatsd_pipe_name` is non-empty. Named pipes are connection-oriented and feed the existing DogStatsD stream framing path.

**Tech Stack:** Rust, Tokio Windows named pipes, `windows-sys` for SDDL security descriptor conversion, existing `saluki-io::net::Listener` and DogStatsD stream handling.

---

### Task 1: Add named-pipe address and DogStatsD config tests

**Files:**
- Modify: `lib/saluki-io/src/net/addr.rs`
- Modify: `lib/saluki-components/src/sources/dogstatsd/mod.rs`

- [ ] **Step 1: Write failing tests for address formatting and DogStatsD config**

Add tests to `lib/saluki-io/src/net/addr.rs` tests module:

```rust
#[test]
fn named_pipe_listen_address_formats_with_windows_pipe_prefix() {
    let address = ListenAddress::named_pipe("datadog-dogstatsd", "D:AI(A;;GA;;;WD)");

    assert_eq!(address.listener_type(), "named_pipe");
    assert_eq!(address.to_string(), r"npipe://datadog-dogstatsd");
}
```

Add tests to `lib/saluki-components/src/sources/dogstatsd/mod.rs` tests module:

```rust
#[test]
fn build_addresses_includes_named_pipe_when_configured() {
    let mut config = default_config();
    config.port = 0;
    config.tcp_port = 0;
    config.pipe_name = Some("datadog-dogstatsd".to_string());

    let addresses = config.build_addresses(None);

    assert_eq!(
        addresses,
        vec![ListenAddress::named_pipe(
            "datadog-dogstatsd",
            default_windows_pipe_security_descriptor()
        )]
    );
}

#[test]
fn eol_required_matches_named_pipe_listener_type() {
    let config = deser_config(r#"{"dogstatsd_eol_required": ["named_pipe"]}"#);
    let eol_required = config.eol_required();

    assert!(eol_required.for_listener(&ListenAddress::named_pipe(
        "datadog-dogstatsd",
        default_windows_pipe_security_descriptor()
    )));
    assert!(!eol_required.for_listener(&udp_listen_address()));
    assert!(!eol_required.for_listener(&tcp_listen_address()));
}
```

- [ ] **Step 2: Run tests and verify they fail**

Run:

```bash
cargo test -p saluki-io named_pipe_listen_address_formats_with_windows_pipe_prefix
cargo test -p saluki-components build_addresses_includes_named_pipe_when_configured eol_required_matches_named_pipe_listener_type
```

Expected: tests fail because `ListenAddress::named_pipe`, `pipe_name`, and named-pipe newline routing do not exist yet.

- [ ] **Step 3: Implement minimal address and config support**

In `lib/saluki-io/src/net/addr.rs`, add a `NamedPipe` variant with pipe name and security descriptor, plus constructor, display, and listener type handling.

In `lib/saluki-components/src/sources/dogstatsd/mod.rs`, add:

```rust
#[serde(rename = "dogstatsd_pipe_name", default)]
#[serde_as(as = "NoneAsEmptyString")]
pipe_name: Option<String>,

#[serde(
    rename = "dogstatsd_windows_pipe_security_descriptor",
    default = "default_windows_pipe_security_descriptor_string"
)]
windows_pipe_security_descriptor: String,
```

Update `EolRequired` with `named_pipe: bool` and route `ListenAddress::NamedPipe { .. }` to that flag.

Update `build_addresses` to push a named-pipe address when `pipe_name` is configured.

- [ ] **Step 4: Run tests and verify they pass**

Run:

```bash
cargo test -p saluki-io named_pipe_listen_address_formats_with_windows_pipe_prefix
cargo test -p saluki-components build_addresses_includes_named_pipe_when_configured eol_required_matches_named_pipe_listener_type
```

Expected: all new tests pass.

- [ ] **Step 5: Commit**

```bash
git add lib/saluki-io/src/net/addr.rs lib/saluki-components/src/sources/dogstatsd/mod.rs
git commit -m "feat(dogstatsd): configure Windows named pipe listener"
```

### Task 2: Add Windows named-pipe listener support in saluki-io

**Files:**
- Modify: `lib/saluki-io/Cargo.toml`
- Modify: `lib/saluki-io/src/net/listener.rs`
- Modify: `lib/saluki-io/src/net/stream.rs`

- [ ] **Step 1: Write failing non-Windows unsupported listener test**

Add to `lib/saluki-io/src/net/listener.rs` tests module:

```rust
#[cfg(not(windows))]
#[tokio::test]
async fn named_pipe_listener_is_unsupported_on_non_windows() {
    let address = ListenAddress::named_pipe("datadog-dogstatsd", "D:AI(A;;GA;;;WD)");

    let err = Listener::from_listen_address(address, None)
        .await
        .expect_err("named pipes should be unsupported on non-Windows");

    assert!(err.to_string().contains("Named pipe listen addresses are not supported"));
}
```

- [ ] **Step 2: Run test and verify it fails**

Run:

```bash
cargo test -p saluki-io named_pipe_listener_is_unsupported_on_non_windows
```

Expected: fail because `Listener` does not handle `ListenAddress::NamedPipe`.

- [ ] **Step 3: Implement listener support**

Add Windows-only `NamedPipe` listener and stream variants:

- `ListenerInner::NamedPipe(tokio::net::windows::named_pipe::NamedPipeServer)` behind `#[cfg(windows)]`.
- `Connection::NamedPipe(tokio::net::windows::named_pipe::NamedPipeServer)` behind `#[cfg(windows)]`.
- `impl AsyncRead` / `AsyncWrite` arms for `Connection::NamedPipe`.
- `impl From<NamedPipeServer> for Stream` behind `#[cfg(windows)]`.

On Windows, create the first pipe instance with `ServerOptions::new().first_pipe_instance(true).create_with_security_attributes_raw(...)` and convert the SDDL string with `ConvertStringSecurityDescriptorToSecurityDescriptorW`. Wrap the descriptor in a small RAII helper that calls `LocalFree` after creation.

In `Listener::accept`, after a pipe instance connects, create the next server instance before returning the connected stream. Use the same security descriptor for each new instance.

On non-Windows, return `ListenerError::InvalidConfiguration { reason: "Named pipe listen addresses are not supported on this platform" }`.

- [ ] **Step 4: Run targeted test**

Run:

```bash
cargo test -p saluki-io named_pipe_listener_is_unsupported_on_non_windows
```

Expected: pass on non-Windows.

- [ ] **Step 5: Run saluki-io tests**

Run:

```bash
cargo test -p saluki-io
```

Expected: pass.

- [ ] **Step 6: Commit**

```bash
git add lib/saluki-io/Cargo.toml lib/saluki-io/src/net/listener.rs lib/saluki-io/src/net/stream.rs
git commit -m "feat(io): add Windows named pipe listener support"
```

### Task 3: Wire DogStatsD behavior and docs

**Files:**
- Modify: `lib/saluki-components/src/sources/dogstatsd/mod.rs`
- Modify: `docs/agent-data-plane/configuration/dogstatsd.md`
- Modify: `lib/datadog-agent/config/schema/schema_overlay.yaml`

- [ ] **Step 1: Write/update tests for warnings and stream behavior selection**

Extend DogStatsD tests so `named_pipe` is no longer a no-op in `dogstatsd_eol_required`, and make `should_warn_stream_log_too_big` include `ListenAddress::NamedPipe { .. }`.

- [ ] **Step 2: Run tests and verify failure**

Run:

```bash
cargo test -p saluki-components eol_required_matches_named_pipe_listener_type
```

Expected: fail until production logic treats named pipes as stream listeners.

- [ ] **Step 3: Implement DogStatsD production logic**

Ensure:

- `named_pipe` newline flag maps only to named-pipe listeners.
- `should_warn_stream_log_too_big` returns true for Unix stream and named-pipe stream listeners.
- Traffic capture remains UDS-only; named pipes do not call PID origin capture.

Update docs/config inventory:

- Move `dogstatsd_pipe_name` and `dogstatsd_windows_pipe_security_descriptor` from unsupported/planned to supported for DogStatsD.
- Document that named-pipe origin detection is not supported, matching the core Agent.

- [ ] **Step 4: Run targeted tests**

Run:

```bash
cargo test -p saluki-components eol_required
cargo test -p datadog-agent-config-testing config_registry
```

Expected: pass or report required snapshot/update command from the config registry tests.

- [ ] **Step 5: Commit**

```bash
git add lib/saluki-components/src/sources/dogstatsd/mod.rs docs/agent-data-plane/configuration/dogstatsd.md lib/datadog-agent/config/schema/schema_overlay.yaml
git commit -m "feat(dogstatsd): enable Windows named pipe intake"
```

### Task 4: Final verification

**Files:**
- No planned source edits.

- [ ] **Step 1: Run Rust checks**

Run:

```bash
cargo check --workspace && cargo check --workspace --tests
```

Expected: pass.

- [ ] **Step 2: Run targeted tests**

Run:

```bash
cargo test -p saluki-io
cargo test -p saluki-components dogstatsd
```

Expected: pass.

- [ ] **Step 3: Format**

Run:

```bash
make fmt
```

Expected: pass/no diff after formatting.

- [ ] **Step 4: Commit any final fixes**

```bash
git status --short
git add <changed-files>
git commit -m "test(dogstatsd): cover Windows named pipe configuration"
```

Only commit if files changed after verification fixes.
