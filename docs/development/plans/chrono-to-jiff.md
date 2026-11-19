# chrono to jiff migration plan

This document tracks moving Saluki off `chrono` and `chrono-tz`, which RustSec marked as unmaintained on 2026-11-17
(RUSTSEC-2026-0543 and RUSTSEC-2026-0555), and onto `jiff`. Each pull request in the stack marks its own row as done.
The last one moves the lasting guidance into [Conventions](../conventions.md) and deletes this file.

## Goal

- Move every date and time use in Saluki to `saluki_common::time`, which wraps `jiff`, keeping the output byte-for-byte
  the same unless a decision below says otherwise.
- Remove `chrono-tz` entirely.
- Remove `chrono` from the shipped `agent-data-plane` binary, and so from its cargo-auditable SBOM, and then from
  `Cargo.lock`.

`chrono` also reaches the graph through two third-party crates. `tracing-rolling-file` shipped in ADP, and is replaced
by an in-house writer. `lading-payload` is only used by millstone, and needs an upstream change.

## Decisions

- **Log timestamps:** keep today's RFC 3339 shape (`2024-12-31T23:59:59.123456789+00:00`: 0, 3, 6, or 9 fractional
  digits and a numeric offset, never `Z`). Matching the Datadog Agent's `time.RFC3339` shape is a separate follow-up.
- **Offsets RFC 3339 can't write:** an offset with seconds is rounded to the nearest minute, and the local time shifts
  to match, as in the last example in section 5.8 of RFC 3339. The timestamp names the exact instant, but its local
  time can be up to 30 seconds from the zone's clock. chrono rounded the offset but kept the zone's local time, so its
  timestamp named an instant up to 30 seconds away. No zone in the time zone database uses an offset with seconds
  today, but a rule such as `TZ=TST1:00:30` does. An offset that rounds to 24 hours or more, such as `-24:00` from
  `TZ=TST24`, is written in UTC.
- **One time API:** date and time code outside `saluki-common` uses `Timestamp`, `TimeZone`, and `TimestampFormat` from
  `saluki_common::time`, and a `deny.toml` ban keeps every other crate from depending on `jiff`. Each output shape is a
  `TimestampFormat` variant, rather than a format written where it's used.
- **Log rolling:** a std-only, size-only writer (`saluki_common::logging::RollingFileWriter`). It drops the implicit
  rollover at local midnight that `tracing-rolling-file` enabled for the main log, which panicked on some days with a
  daylight saving time change at midnight. Everything else about rolling stays the same.
- **Time zones:** use jiff's detection: `TZ` if it's set (UTC if it can't be resolved), otherwise `/etc/localtime`, or
  the system settings on Windows. That matches the Datadog Agent on Linux, but unlike the Agent, it also honors `TZ` on
  Windows. Enable `tzdb-bundle-always`, so that named zones still resolve on hosts without a time zone database.
- **Version:** `jiff` 1.x.

## Stack

| PR | Scope | Status |
|---|---|---|
| 0 | `deny.toml` ignores for both advisories | Done |
| 1 | Replace `tracing-rolling-file` with `RollingFileWriter` | Done |
| 2 | Log timestamps on `jiff`; remove `chrono-tz` and `iana-time-zone` from saluki-app | Done |
| 3 | Persisted retry queue file names in saluki-io | Planned |
| 4 | saluki-components (OTLP, logs encoder, DogStatsD debug log) and datadog-agent-runtime | Planned |
| 5 | agent-data-plane CLI and panoramic; remove `chrono` from the workspace | Planned |
| 6 | Bump lading once `lading-payload` no longer needs `chrono`; remove the last ignore | Waiting on lading |

## Rules for date and time code

- Use `saluki_common::time`, not `jiff`. If no `TimestampFormat` fits, add a variant, with a test of its documented
  example, rather than formatting at the call site.
- Get the current time from `Timestamp::now`, and handle its error where it's called. Don't replace the error with a
  made-up time.
- Get the local time zone from `TimeZone::system`, which detects it once.
- In tests, build zones with `TimeZone::posix` or `TimeZone::utc`, and don't set `TZ`.
- For each migration, land characterization tests against chrono first. The swap must pass them unchanged.

Inside `saluki_common::time`, watch for these `jiff` traps:

- Don't use `%+`: jiff doesn't support it and prints it literally.
- Don't use bare `%f` or `%.f`: both drop trailing zeros, unlike chrono's `%f`. Use `%9f` or `%N`, or set a precision.
- `civil::DateTime::strptime` takes `(format, input)`, the reverse of chrono's `parse_from_str(input, format)`. Both are
  `&str`, so swapping them compiles. Prefer decoding fixed-width input by slicing.
- Don't use `ZonedDateTime` on hot paths or for RFC 3339 output: its `Display` appends a `[time zone]` annotation.
  jiff's `Timestamp::display_with_offset` doesn't match chrono's `%+` either: without a precision it drops trailing
  zeros (`.12` where chrono writes `.120`), and with one it always writes that many digits.
- Never call jiff's `TimeZone::system()` or `ZonedDateTime::now()` per event: when detection fails, jiff repeats it on
  every call. jiff's `Timestamp::now()` panics when the clock is out of range.
- Keep jiff's `logging` feature off: jiff would log through the `log` crate, which our logging subsystem captures.
- Never use jiff's `TimeZone::get` in tests, because it depends on the host's time zone database.
