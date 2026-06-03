---
slug: replay-no-panic-on-malformed-capture
sut_path: /home/ssm-user/src/saluki
commit: 042f41db3bd97118c38981765fd49696fce9d318
updated: 2026-05-28
type: Safety
priority: High
assertion_status: MISSING (net-new instrumentation)
---

# Property: Parsing an arbitrary/corrupt/truncated DogStatsD capture never panics

## Origin
SUT analysis §6 gap #6 ("DogStatsD replay has zero suite coverage despite being the
newest, largest, untrusted-input feature"), §7 #12, §8 ("Most regression-prone area:
DogStatsD replay", `e88d04b10a`, +1765 LOC, validated only with `cargo check`). No
Antithesis assertion exists (existing-assertions.md).

## What the code does
`lib/saluki-components/src/sources/dogstatsd/replay/reader.rs`:
- `from_path` (39-64): `fs::read` → zstd sniff (`has_zstd_magic`, 141-143, checks 4
  magic bytes) → `zstd::stream::decode_all` (44) → `valid_header` → `file_version`.
  All fallible steps map to `GenericError` via `?`. zstd errors are caught (45).
- `read_next` (84-104): bounds-checks `offset + LENGTH_PREFIX_SIZE > len` → `Ok(None)`
  (85-87); reads 4-byte LE length prefix; **`size_bytes.try_into().expect("length
  prefix is 4 bytes")`** (90) — this `expect` is provably safe because the slice is
  exactly `[offset .. offset+4]` after the bounds check, so it can never fire on any
  input; bounds-checks `offset + size > len` → `Ok(None)` (95); `UnixDogstatsdMsg::decode`
  returns mapped error (99-100).
- `read_state` (110-138): symmetric, with the same provably-safe `expect` at 121 and a
  real error return at 126-131 if the trailer size overruns.

`bin/agent-data-plane/src/cli/dogstatsd.rs` (the **driver**, runs inside an ADP process):
- 269-270: `from_path(&cmd.replay_file_path)?` then `read_state()?` (config-check-style path).
- 355: `from_path(replay_file_path)?`; 367: `let msg = match reader.read_next()? { … }`
  inside `replay_one_iteration` — all errors propagate via `?` up to a `tokio::select!`
  (341-347) that returns the error. No `unwrap`/`expect` on the reader results here.

## Failure scenario (Antithesis)
Feed the `agent-data-plane dogstatsd replay` subcommand a capture file that is:
arbitrary bytes; a valid header followed by a truncated/garbage protobuf; a zstd stream
that decompresses to a header + corrupt body; a length prefix that decodes but whose body
is invalid protobuf; a zstd bomb / partial zstd frame. Expectation: the process exits with
a clean `Err` (non-zero exit, logged), never a panic/abort/SIGABRT.

## Key observations
- The two non-test `expect` sites (reader.rs:90, 121) are guarded by an exact-length
  bounds check, so they are **not** reachable panic sites — the "~25 unwrap/expect" figure
  in the brief is the whole-file count and is dominated by test code (26 of 28 are in
  `#[cfg(test)]`). The real untrusted-input panic surface in the reader is small.
- The realistic panic risk is in the *dependencies*: `zstd::stream::decode_all` on a
  malicious stream (memory blowup / library panic) and `prost`'s `Message::decode` on
  adversarial protobuf (recursion/length). Both are wrapped in `Result`, but a panic
  inside them would still abort — Antithesis is the right tool to find such a panic.
- A panic is invisible to a workload-only checker if the replay runs as a subprocess that
  is expected to exit non-zero anyway; distinguishing "clean error exit" from "panic/abort"
  needs SUT-side signal.

## Config deps
- Replay path is gated on the `dogstatsd replay` CLI subcommand + a UDS listener; Linux-only
  (`#[cfg(target_os = "linux")]`, dogstatsd.rs:351). The capture file path is operator-supplied.
- zstd decompression is auto-selected by magic bytes — no config flag needed to reach it.

## Suggested assertion (MISSING — net-new)
- **Unreachable** at any panic/abort originating from the reader or its decode calls. Best
  realized as an SDK `assert_unreachable` in a panic hook installed for the replay path, or
  by treating any SIGABRT/panic-unwind during replay as a property violation. The workload
  cannot cleanly observe a panic from outside, so this needs SUT-side instrumentation.
- Pair with **Reachable**(replay parse executed) so the test confirms the path was actually
  exercised, not skipped because the subcommand never ran.

## SUT-side instrumentation needs
- A panic hook (or `assert_unreachable`) on the replay code path is required; a workload-only
  check sees only an exit code and cannot distinguish panic from a deliberate `Err`.
- Optionally anchor `assert_always(result.is_ok() || clean_err)` at dogstatsd.rs:367 so an
  unexpected panic in `read_next` is converted into a recorded assertion failure.

## Open questions
- (none remaining — see Investigation Log)

### Investigation Log

#### How is replay invoked; whole-file read OOM vector; zstd decompression-bomb vector
- **Examined:** `bin/agent-data-plane/src/cli/dogstatsd.rs:38-114` (subcommand defs),
  `:169-213` (`handle_dogstatsd_command` dispatch), `:261-310` (`handle_dogstatsd_replay`),
  `:322-399` (`run_dogstatsd_replay` / `replay_one_iteration`); and
  `lib/saluki-components/src/sources/dogstatsd/replay/reader.rs:34-143`.
- **Found:**
  - **(a) Separate process, sends over UDS to the running data-plane.** Replay is the
    `dogstatsd replay` argh subcommand (`ReplayCommand`, dogstatsd.rs:103-114), dispatched at
    `dogstatsd.rs:192-211` to `handle_dogstatsd_replay`. The CLI process itself opens the capture
    file and reads records, then **sends each record as a UDS datagram to the already-running ADP**
    via `uds_sendmsg_with_creds(socket, &msg.payload, &credentials)` (`dogstatsd.rs:394`); the
    socket target is ADP's configured `dogstatsd_socket` (`dogstatsd_socket_path`, :313). So parsing
    of the capture file happens **in the replay CLI process, not in the data-plane process.** A
    panic during parsing aborts the replay tool (exit/SIGABRT), not the data-plane.
    - Consequence for instrumentation: a panic-catch / `assert_unreachable` for malformed-capture
      parsing belongs in the **replay CLI process** (the `from_path`/`read_next`/`read_state` call
      sites in `dogstatsd.rs` and `reader.rs`). The data-plane process only ever sees the resulting
      *bytes* of `msg.payload` arriving over the DSD UDS socket — i.e. ordinary DSD packets, which
      are covered by the malformed-dsd-no-crash property, not by the capture parser.
    - Note `from_path` is invoked **twice per replay**: once eagerly in `handle_dogstatsd_replay`
      (dogstatsd.rs:269 for `read_state`) and again per loop iteration in `replay_one_iteration`
      (dogstatsd.rs:355). Both propagate parse errors via `?`; no `unwrap`/`expect` on reader output.
  - **(b) Whole-file `fs::read` with NO size guard — OOM vector confirmed.** `reader.rs:40-41`:
    `let raw = fs::read(path).map_err(...)?;` reads the *entire* file into a `Vec<u8>` before any
    parsing or size check. There is no stat/metadata length check, no max-size constant, no
    streaming. A multi-GB capture path is loaded fully into memory in the replay process. This is an
    OOM vector independent of parsing correctness. (Lives in the replay CLI process per (a).)
  - **(c) `zstd::stream::decode_all` on untrusted input with NO decompressed-size cap —
    decompression-bomb vector confirmed.** `reader.rs:43-48`: if `has_zstd_magic(&raw)` (4-byte magic
    sniff, `reader.rs:141-143`), it calls `zstd::stream::decode_all(raw.as_slice())` (`reader.rs:44`)
    and stores the full decompressed output in `contents`. There is no `Decoder` with a window/size
    limit, no streaming bound, no cap on decompressed length — `decode_all` allocates as large as the
    stream dictates. A small crafted `.dog.zstd` can expand to an arbitrarily large `Vec<u8>`. Errors
    from `decode_all` are caught (`.map_err(...)?`, reader.rs:45), so a *malformed* stream returns a
    clean `Err`; the hazard is specifically unbounded *memory*, not a panic, on a *valid but huge*
    decompression.
- **Not found:** No file-size limit, no `fs::metadata` length pre-check, no zstd window/size cap,
  no streaming reader. No panic-prone `unwrap`/`expect` on untrusted bytes in the reader (the two
  non-test `expect`s at reader.rs:90/121 are guarded by exact-length bounds checks, as previously
  noted).
- **Conclusion:** RESOLVED. (a) Replay is a separate CLI process that parses the capture and forwards
  payloads to the running ADP over the DSD UDS socket — panic/assert instrumentation for capture
  parsing belongs in the replay process. (b) and (c) are both LIVE resource-exhaustion vectors:
  unbounded `fs::read` (reader.rs:40) and uncapped `zstd::stream::decode_all` (reader.rs:44). These
  are OOM/decompression-bomb hazards (memory-bound family), distinct from the no-panic property; they
  warrant either a size cap in the reader or an explicit resource-exhaustion property. The no-panic
  property itself stands and its panic surface is the zstd/prost decode calls, not the reader's own
  bounds-checked logic.
