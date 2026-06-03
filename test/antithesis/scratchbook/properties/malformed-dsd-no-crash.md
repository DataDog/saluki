---
slug: malformed-dsd-no-crash
sut_path: /home/ssm-user/src/saluki
commit: 042f41db3bd97118c38981765fd49696fce9d318
updated: 2026-05-28
type: Safety
priority: High
assertion_status: MISSING (net-new instrumentation)
---

# Property: Malformed DogStatsD packets never crash the process or kill the socket

## Origin
SUT analysis §2 ("a malformed packet never kills the socket", `mod.rs:1283-1318`), §6 gap
#6, §8 ("undecided malformed-input error policy in the codecs"). Decomposes the headline
"ADP will not crash under load." No Antithesis assertion exists (existing-assertions.md).

## What the code does
`lib/saluki-components/src/sources/dogstatsd/mod.rs::drive_stream` (1146-1337):
- The `'read` loop reads into an I/O buffer, then a `'frame` loop calls
  `framer.next_frame(io_buffer, reached_eof)` (1237) and `handle_frame(...)` (1252).
- **Per-frame parse error** (`handle_frame` returns `Err`, 1266-1270): logged at `warn!`
  and the loop continues — the bad frame is skipped, not fatal.
- **Framing error** (1272-1293): increments `framing_errors`; for **connectionless** streams
  (UDP, UDS datagram) `io_buffer.clear()` + `continue 'read` (1283-1288) — clear-and-continue,
  the socket survives. For **connection-oriented** streams it `break 'read` (1289-1291), closing
  only that one connection (not the process).
- **I/O error** (1306-1318): connectionless → `continue 'read`; connection-oriented → `break 'read`.
- `handle_frame` (1458-1542) routes to `codec.decode_packet`; on decode error it bumps a
  type-specific `*_decode_failed` counter and returns `Err` (1462-1473) — caught by the caller above.

Codecs (`lib/saluki-io/src/deser/codec/dogstatsd/`):
- `metric.rs::parse_dogstatsd_metric` (67-194): `nom` parsers returning `IResult`; unknown
  `|`-chunks are silently skipped (136-141, with a TODO "throw an error, warn, or be silently
  permissive?"). `permissive_metric_name` (197-206) uses `from_utf8_unchecked` but only after
  `take_while1(valid_char)` constrains bytes to printable ASCII (SAFETY comment). `raw_metric_values`
  validates UTF-8 with `simdutf8` before any `from_utf8_unchecked` (232-234).
- `event.rs:146-148` and `service_check.rs:94-96`: identical "skip unknown chunk" TODO — **undecided
  error policy**, currently silently permissive.
- `metric.rs:243` `unreachable!("should be constrained by alt parser")` — reachable only if the
  `alt((tag("g"),tag("c"),…))` matched something not in the match arm; provably constrained, so not
  a real panic site, but it *is* an `unreachable!` on the hot parse path worth covering.
- Existing proptest `property_test_malicious_input_non_exhaustive` (metric.rs:761-772): 1000 random
  byte vectors, asserts no panic. This is a `cargo test` proptest, **not** an Antithesis assertion,
  and is non-exhaustive by its own comment.

## Failure scenario (Antithesis)
Drive each listener (UDP 8125, TCP, UDS datagram, UDS stream) with adversarial packets:
oversized frames (exceed buffer → framing error), invalid UTF-8 in value/name positions,
truncated extensions (`|@`, `|#`, `c:`, `e:`, `card:` with missing bodies), enormous tag lists,
embedded NULs, partial multi-value (`x:1:2:`), zero-length frames. Expectation: process stays
up; connectionless sockets keep serving subsequent valid packets; a TCP connection may close but
the listener accepts new connections; no panic.

## Key observations
- The clear-and-continue (1283-1288) and skip-bad-frame (1266-1270) paths are the explicit
  socket-survival mechanism — the property is precise: **connectionless sockets never die on a bad
  packet; the process never panics on codec/framing errors.**
- The codecs return `Err` rather than panicking for malformed input by construction (nom + guarded
  `unsafe`), but the `unreachable!`/`from_utf8_unchecked` sites mean a *parser regression* could turn
  malformed input into a panic — exactly what Antithesis should guard.
- TCP `break 'read` closes one connection; that is acceptable (connection-oriented semantics) and must
  be excluded from a "socket never dies" claim — scope the listener-survival half to connectionless.

## Config deps
- `permissive` mode (metric.rs:73) widens accepted metric names — broadens the malformed surface;
  test both permissive and strict.
- `client_origin_detection`, `timestamps` gates (metric.rs:117/122/127/132) change which extension
  chunks are parsed; toggling them changes the reachable parse branches.
- Which listeners are enabled is config-driven; the property should hold for every enabled listener.

## Suggested assertion (MISSING — net-new)
- **Always**(process up): the ADP process stays alive across the entire malformed-input workload —
  realized as a workload-side liveness/health check plus a panic hook converting any codec/framing
  panic into a recorded failure.
- **Unreachable** at codec panic: `assert_unreachable` covering the `unreachable!` (metric.rs:243) and
  the `from_utf8_unchecked` SAFETY invariants — any panic there is a must-never.
- **Always**(connectionless socket survives): after a malformed packet on UDP/UDS-datagram, the same
  socket successfully receives a subsequent valid packet (`packet_receive_success` increments again).
- **Sometimes(framing_errors > 0)** and **Sometimes(*_decode_failed > 0)**: prove the adversarial
  input actually reached the error paths, not that the workload was too benign.

## SUT-side instrumentation needs
- A panic during parse is invisible to a workload-only checker until the process dies; pair a panic
  hook / `assert_unreachable` at the codec sites with workload-side liveness. `framing_errors` and
  `*_decode_failed` counters already exist for the `Sometimes` reachability anchors.

## Open questions
- **What should the codec error policy be for unknown/trailing chunks** (the four TODOs)? Currently
  silently permissive. If the policy changes to "error," more inputs become `Err` (still no crash) but
  more packets are dropped — affects the data-loss family, not this no-crash property. Worth resolving
  before finalizing the assertion so the test's expected-drop accounting is stable.
- **Does a malformed packet ever cause a *partial* dispatch** that mis-routes remaining events (SUT §7
  #6, mod.rs dispatch-error swallow)? That is a separate routing-correctness property; confirm it is
  not conflated with no-crash.
- **Is there a max frame/datagram size that, when exceeded, the framer handles gracefully on every
  transport** (vs. only connectionless)? Confirm TCP oversized-frame handling does not wedge the
  connection (it `break`s, but verify no resource leak per closed connection).
