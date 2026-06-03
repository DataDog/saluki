---
slug: replay-corruption-not-silent-eof
sut_path: /home/ssm-user/src/saluki
commit: 042f41db3bd97118c38981765fd49696fce9d318
updated: 2026-05-28
type: Safety
priority: Medium
assertion_status: MISSING (net-new instrumentation)
---

# Property: Capture corruption is distinguishable from a clean EOF (no silent truncation)

## Origin
SUT analysis §7 #12 ("Replay reader treats corruption as clean EOF … silently truncates
the remaining record stream — false replay-fidelity confidence"). No Antithesis assertion
exists (existing-assertions.md). **Framed honestly: the current code intentionally returns
`Ok(None)` on these inputs; this property captures the data-fidelity risk, it does not
claim the code is wrong today.**

## What the code does
`lib/saluki-components/src/sources/dogstatsd/replay/reader.rs::read_next` (84-104):
```rust
if self.offset + LENGTH_PREFIX_SIZE > self.contents.len() { return Ok(None); } // (a)
let size = u32::from_le_bytes(...) as usize;
self.offset += LENGTH_PREFIX_SIZE;
// "The writer emits a zero-length prefix to mark the start of the tagger state trailer;
//  treat that (and any size that would overrun the buffer) as the end of the record stream."
if size == 0 || self.offset + size > self.contents.len() { return Ok(None); }       // (b)
```
Three distinct conditions all collapse to the **same** `Ok(None)` "clean EOF" signal:
1. Legitimate end: offset reached the start of the zero-length trailer separator (`size == 0`).
2. Truncation: a record length prefix is present but the body is cut short
   (`offset + size > len`).
3. Corruption: a corrupt length prefix that happens to read as `0` or as an oversized value.

The driver (`bin/agent-data-plane/src/cli/dogstatsd.rs:367-373`) treats `Ok(None)` as
"replay iteration completed" and stops sending packets — so cases 2 and 3 **silently drop
every remaining record** with no error and no telemetry.

Tests currently *assert* this behavior: `truncated_record_returns_none` (245-257) writes a
file with the last 8 bytes dropped and asserts `read_next()` yields `Ok(None)` ("clean EOF on
truncation"); `read_next_stops_at_state_separator` (233-242) asserts the trailer boundary →
`Ok(None)`. So the silent-truncation behavior is encoded as intended.

Contrast: `read_state` (126-131) **does** return an `Err` when the trailer size overruns the
buffer — so the codebase already distinguishes "oversized length → error" in the trailer path
but not in the record path. This asymmetry is the crux of the property.

## Failure scenario (Antithesis)
Replay a capture that is valid for the first N records, then has a corrupt 4-byte length
prefix (e.g. a flipped byte making `size` huge, or a zeroed prefix mid-stream). The reader
returns `Ok(None)` at that point; the replay tool reports success having sent only N of M
records. A diff against the capture's true record count would reveal the loss, but the tool
itself signals success — the fidelity loss is invisible.

## Key observations
- This is a *data-fidelity* property, not a crash property. The "bad thing" is **silent**
  truncation reported as success, not a panic.
- A faithful fix would track whether the offset reached exactly the trailer separator vs. ran
  off a malformed prefix, and surface the latter as `Err`. The trailer path (read_state) already
  does this for oversize. The property can be stated without demanding a fix: *if records were
  truncated by corruption, the replay must not report a clean completion.*
- Because the legitimate-EOF case (size==0 separator) and the truncation case are byte-shape
  identical from `read_next`'s local view, distinguishing them requires either a record count in
  the header/trailer or an explicit corruption sentinel — neither exists today (open question).

## Config deps
- Same gating as `replay-no-panic-on-malformed-capture`: `dogstatsd replay` subcommand, UDS
  listener, Linux-only.
- File version ≥ MIN_STATE_VERSION (2) means a trailer is expected (file_header.rs:11); the
  separator-vs-truncation ambiguity is most acute for versioned files that *should* have a trailer.

## Suggested assertion (MISSING — net-new)
- **AlwaysOrUnreachable**(replay completion is faithful): when `read_next` returns the
  terminating `Ok(None)`, the consumed offset equals the start of the tagger-state trailer
  (clean end) — i.e. the loop did not stop because of an unconsumed/over-running length prefix.
  Realize as an SDK assertion at the (b) branch (reader.rs:95) distinguishing
  `size == 0 && at_trailer_boundary` (clean) from the overrun/`size==0`-mid-stream case (corrupt).
- **Sometimes(corruption-detected)**: at least once, the reader reaches the (b) branch with a
  length prefix that overruns the buffer (proves the corrupt input actually exercised the path,
  not just clean EOF). Meaningful state, not `Sometimes(true)`.

## SUT-side instrumentation needs
- A workload-only check cannot tell truncation from clean EOF (both look like "replay finished").
  Needs an SDK assertion at reader.rs:95 (or a new telemetry counter `replay_records_truncated`)
  to expose the corrupt branch. Could also compare a record count emitted at capture time against
  records replayed.

## Open questions
- **Is there a record count or total-length field anywhere** (header/trailer) that would let the
  reader detect "stopped early"? If not, distinguishing truncation from clean EOF requires a format
  change. Determines whether this can be a strict `Always` or only a best-effort heuristic.
- **Do the maintainers consider silent truncation acceptable for replay** (best-effort tool) vs. a
  fidelity bug? The intentional `Ok(None)` and the asserting tests suggest "accepted"; this property
  documents the risk and lets Antithesis quantify how often corruption silently truncates. Changes
  priority, not the property statement.
- **How does a corrupt prefix that decodes to a small but wrong `size` behave?** It would decode the
  wrong bytes as a record body (case where `offset+size <= len` but bytes are garbage) → either a
  prost decode `Err` (good, surfaced) or, worst case, a successfully-decoded *wrong* record. Worth
  enumerating: this is a third outcome (silent corruption rather than silent truncation).

### Investigation Log

#### Do the maintainers consider silent truncation acceptable, or is it a fidelity bug? `(needs human input)`

- **Examined**: `lib/saluki-components/src/sources/dogstatsd/replay/reader.rs` `read_next`
  (length-prefix parse ~84-104, the `size == 0 || offset+size > len → Ok(None)` collapse); the
  reader's own unit tests (~244-257) which feed truncated/oversized prefixes and **assert** the
  result is `Ok(None)`; the capture-file format (`UnixDogstatsdMsg` records + a `TaggerState`
  trailer, `writer.rs`) for any record-count or total-length field.
- **Found**: the silent-truncation behavior is *intentional in code* — `Ok(None)` is the deliberate
  return for both legitimate EOF and a corrupt/over-running prefix, and the tests pin that behavior
  as desired. There is **no record-count or total-length field** in the format, so the reader has no
  in-band way to distinguish "stopped early" from "clean end."
- **Not found**: any comment, doc, ADR, or commit message stating whether silent truncation is an
  accepted best-effort property of the replay tool or a known fidelity gap. Code intent ("we return
  Ok(None)") is clear; *product* intent ("is that OK?") is not recoverable from the repo.
- **Conclusion**: tagged `(needs human input)`. The behavior is unambiguous; only the maintainers can
  say whether it is acceptable. The answer changes this property's **priority** (and whether a
  format change adding a record count is warranted), not the property statement.
