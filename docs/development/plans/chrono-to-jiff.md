# chrono to jiff migration plan

This document tracks moving Saluki off `chrono` and `chrono-tz`, which RustSec marked as unmaintained on 2026-11-17
(RUSTSEC-2026-0543 and RUSTSEC-2026-0555), and onto `jiff`. Each pull request in the stack marks its own row as done.
The last one moves the lasting guidance into [Conventions](../conventions.md) and deletes this file.

## Goal

- Move every date and time use in Saluki to `jiff`, keeping the output byte-for-byte the same unless a decision below
  says otherwise.
- Remove `chrono-tz` entirely.
- Remove `chrono` from the shipped `agent-data-plane` binary, and so from its cargo-auditable SBOM, and then from
  `Cargo.lock`.

`chrono` also reaches the graph through two third-party crates. `tracing-rolling-file` shipped in ADP, and is replaced
by an in-house writer. `lading-payload` is only used by millstone, and needs an upstream change.

## Decisions

- **Log timestamps:** keep today's RFC 3339 shape (`2024-12-31T23:59:59.123456789+00:00`: 0, 3, 6, or 9 fractional
  digits and a numeric offset, never `Z`). Matching the Datadog Agent's `time.RFC3339` shape is a separate follow-up.
- **Log rolling:** a std-only, size-only writer (`saluki_common::logging::RollingFileWriter`). It drops the implicit
  rollover at local midnight that `tracing-rolling-file` enabled for the main log, which panicked on some days with a
  daylight saving time change at midnight. Everything else about rolling stays the same.
- **Time zones:** use jiff's detection (`TZ` first, then `/etc/localtime`, or the system settings on Windows), which is
  close to the Datadog Agent's. Enable `tzdb-bundle-always`, so that named zones still resolve on hosts without a time
  zone database.
- **Version:** `jiff` 1.x.

## Stack

| PR | Scope | Status |
|---|---|---|
| 0 | `deny.toml` ignores for both advisories | Done |
| 1 | Replace `tracing-rolling-file` with `RollingFileWriter` | Done |
| 2 | Log timestamps on `jiff`; remove `chrono-tz` and `iana-time-zone` from saluki-app | Planned |
| 3 | Persisted retry queue file names in saluki-io | Planned |
| 4 | saluki-components (OTLP, logs encoder, DogStatsD debug log) and datadog-agent-runtime | Planned |
| 5 | agent-data-plane CLI and panoramic; remove `chrono` from the workspace | Planned |
| 6 | Bump lading once `lading-payload` no longer needs `chrono`; remove the last ignore | Waiting on lading |

## Rules for `jiff` code

- Don't use `%+`: jiff doesn't support it and prints it literally.
- Don't use bare `%f` or `%.f`: both drop trailing zeros, unlike chrono's `%f`. Use `%9f` or `%N`, or set a precision.
- `civil::DateTime::strptime` takes `(format, input)`, the reverse of chrono's `parse_from_str(input, format)`. Both are
  `&str`, so swapping them compiles. Prefer decoding fixed-width input by slicing.
- Don't use `ZonedDateTime` on hot paths or for RFC 3339 output: its `Display` appends a `[time zone]` annotation. Use
  `saluki_common::time::Rfc3339Timestamp`, or `Timestamp::display_with_offset`.
- Get the local time zone from `saluki_common::time::system_time_zone`, which detects it once. Never call
  `TimeZone::system()` or `ZonedDateTime::now()` per event: when detection fails, jiff repeats it on every call.
- Keep jiff's `logging` feature off: jiff would log through the `log` crate, which our logging subsystem captures.
- In tests, build zones with `TimeZone::posix` or `TimeZone::UTC`, never `TimeZone::get`, and don't set `TZ`.
- For each migration, land characterization tests against chrono first. The swap must pass them unchanged.
