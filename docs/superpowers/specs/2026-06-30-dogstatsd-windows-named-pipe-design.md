# DogStatsD Windows named-pipe listener design

## Context

DADP-99 identified Windows support gaps for Agent Data Plane. After comparing the remaining items with `datadog-agent`, the only required Saluki parity gap is DogStatsD named-pipe intake. The core Agent supports `dogstatsd_pipe_name` and `dogstatsd_windows_pipe_security_descriptor` on Windows; ADP currently accepts `named_pipe` in `dogstatsd_eol_required` as a no-op but does not create a named-pipe listener.

## Goal

Add Windows DogStatsD named-pipe intake with behavior matching the core Agent where it affects user-visible semantics:

- `dogstatsd_pipe_name` enables a listener at `\\.\pipe\<name>` on Windows.
- `dogstatsd_windows_pipe_security_descriptor` controls the pipe security descriptor and defaults to the core Agent default.
- `dogstatsd_buffer_size` sizes the per-connection read buffer.
- Named-pipe traffic is newline-framed and feeds the same DogStatsD decode path as other stream transports.
- `dogstatsd_eol_required: named_pipe` requires newline-terminated named-pipe frames.
- Origin detection is not implemented for named-pipe traffic, matching the core Agent.

## Non-goals

- Do not add named-pipe transport for the privileged ADP API. The Agent command API uses local TCP/TLS auth today.
- Do not add Windows-native PID/container origin detection.
- Do not add Windows Event Log output.
- Do not implement APM Windows pipe intake.

## Approach

Implement a reusable Windows named-pipe listener in `saluki-io` and wire DogStatsD to it.

The listener should follow the existing `Listener` / `Stream` abstractions where practical. Named pipes are connection-oriented, so each accepted pipe client should produce a stream-like object consumed by DogStatsD's existing stream framing path. The Windows implementation should be `#[cfg(windows)]`; non-Windows builds should reject named-pipe addresses with an unsupported configuration error.

DogStatsD configuration should add fields for `dogstatsd_pipe_name` and `dogstatsd_windows_pipe_security_descriptor`, build a named-pipe listen address when the pipe name is non-empty, and include `named_pipe` in `EolRequired` routing.

## Testing

Add unit coverage for:

- DogStatsD config address construction with and without `dogstatsd_pipe_name`.
- `dogstatsd_eol_required` behavior for `named_pipe`.
- Non-Windows unsupported handling where compilation permits.
- Windows-only named-pipe listener behavior if it can be exercised in unit tests.

Add or extend Windows integration coverage in `test/integration` if the harness can send DogStatsD payloads over a named pipe from the Windows runtime container.

## Risks

The main implementation risk is fitting Windows named-pipe I/O into the current `saluki-io` stream abstraction without over-generalizing. If that proves too invasive, keep the public DogStatsD behavior but localize more of the named-pipe handling behind a small Windows-only adapter.
