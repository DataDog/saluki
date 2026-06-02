---
slug: interner-reclamation-no-corruption
sut_path: /home/ssm-user/src/saluki
commit: 042f41db3bd97118c38981765fd49696fce9d318
updated: 2026-05-28
type: Safety
priority: High
assertion_status: MISSING (net-new instrumentation)
---

# Property: Concurrent intern + drop-last-ref never yields overlapping/corrupt entries

## Origin
SUT analysis §3 ("manual reclamation/tombstoning"), §4 ("Interner reclamation is loom-tested;
worst case is a duplicate entry, never corruption … the most concurrency-unsafe component in the
bounded-memory story"). existing-assertions.md notes a `loom` cfg already marks this path as
concurrency-sensitive. No Antithesis assertion exists.

## What the code does
`lib/stringtheory/src/interning/fixed_size.rs` + `map.rs` — a fixed byte buffer with manual
refcount-based reclamation. The safety argument rests on a mutex + a refcount **re-check**:

- `EntryHeader.refs: AtomicUsize` (fixed_size.rs:92-95). `increment_active_refs` uses `AcqRel`
  (212-214); `decrement_active_refs` (219-221) returns true iff `fetch_sub(1, AcqRel) == 1`
  (i.e. count hit zero). `is_active` loads `Acquire` (207-209).
- Drop (map.rs:73-84): when `decrement_active_refs()` says "now zero," takes
  `interner.lock()` and calls `mark_for_reclamation(self.header)`.
- **The re-check under the lock** (map.rs:447-470 `InternerState::mark_for_reclamation`): re-reads
  `header.is_active()` (459). If a concurrent `try_intern` resurrected the entry (incremented refs)
  between the drop's decrement and acquiring the lock, `is_active()` is now true and reclamation is
  **skipped**. Only if still inactive does it `entries.remove(entry_str)` (466) then
  `storage.mark_for_reclamation` (468). Comment (450-454) states only the mutex-mediated `InternerState`
  increments refs and only the handle drop decrements, so a zero count under the lock means nobody else
  holds or is acquiring a reference.
- **The buffer is overwritten on reclaim**: `write_to_reclaimed_entry`/the fill at map.rs:382-393 fills
  the reclaimed string capacity with `0x21` ("a known repeating value … signal that offsets/reclaimed
  entries are incorrect and overlapping"). So a stale `'static &str` pointing into a reclaimed slot would
  read `0x21` bytes — the corruption sentinel.
- `try_intern` (map.rs:472-517): under the lock, first checks `entries.get(s)` and on hit
  `increment_active_refs` (483-484) — this is the resurrection that the drop re-check defends against.
  On miss, reuses a reclaimed entry (495-497) or unoccupied space (498-499), inserts a `'static`-lifetime
  key (513-514, with a SAFETY note that the lifetime never outlives the entry).
- **Loom test** `concurrent_drop_and_intern` (fixed_size.rs:1072-1142): models T1 holding an entry, T2
  interning the same string, then `drop(t1)` — asserts ≤1 reclaimed entry and that the reclaimed entry
  does **not overlap** the surviving interned string (`do_reclaimed_entries_overlap`, 1078-1086). The
  documented acceptable outcome is a benign duplicate (1090-1094, 1117-1121).

## Failure scenario (Antithesis)
High-cardinality DSD load with many short-lived contexts so the same tag/name strings are repeatedly
interned and dropped across the source's context resolvers, driving concurrent `try_intern` (resolve)
against drop-last-ref reclamation on a near-full interner (forcing reclaimed-slot reuse + buffer
overwrite). The hazard: a `try_intern` that returns a handle into a slot a concurrent drop then reclaims
and overwrites with `0x21`, producing an interned `&str` whose bytes are corrupt or overlap another live
entry.

## Key observations
- The loom test **bounds** the interleavings (loom explores a model with a small, fixed thread/op set);
  Antithesis explores the real scheduler under real load with many shards and entries — the SUT analysis
  explicitly flags this as where Antithesis adds value beyond loom.
- The invariant to check is exactly the loom assertion generalized: **no reclaimed entry overlaps a live
  entry**, and **no live `InternedString` reads the `0x21` corruption sentinel**. The `0x21` fill is a
  ready-made detector: a resolved string containing the fill pattern where it shouldn't is corruption.
- Sharding (`[Arc<Mutex<…>>; SHARD_FACTOR]`, SUT §3) means cross-shard interactions add interleavings loom
  doesn't model per-run.

## Config deps
- Interner capacity / `allow_heap_allocations` (SUT §3): with heap fallback on (default true), a full
  interner spills to heap and the reclaimed-slot-reuse path is less pressured — to exercise reclamation,
  the test wants a **small** interner and/or heap-fallback off so the buffer actually fills and reclaims.
- `SHARD_FACTOR` and per-shard capacity govern how often reclaimed slots are reused.

## Suggested assertion (MISSING — net-new)
- **Always**(no corrupt/overlapping entry): generalize the loom check to a runtime invariant. Realize as
  an SDK `assert_always` (or `assert_unreachable` on the corruption-detected branch) inside
  `mark_for_reclamation`/`try_intern` that verifies a newly returned entry does not overlap any reclaimed
  entry and that resolved bytes are not the `0x21` sentinel. This needs SUT-side instrumentation — the
  race is invisible to a workload-only checker.
- **Sometimes(reclaimed-slot reused)** and **Sometimes(drop re-check found resurrected entry)**: prove the
  contended reclamation path was actually hit (the `is_active()` re-check at map.rs:459 returning true),
  not just steady-state interning.

## SUT-side instrumentation needs
- A debug-build check that scans for overlap between `reclaimed` entries and the live `entries` map, or a
  per-resolve check that the returned `&str` contains no unexpected `0x21` run, gated behind a test cfg.
  A workload cannot see interner internals; only SUT-side assertions can catch the corruption branch.

## Open questions
- **Memory ordering sufficiency:** the re-check relies on `AcqRel`/`Acquire` pairing between
  `increment_active_refs` and the drop's `decrement` + `is_active` under the lock. Confirm the lock
  acquire provides the needed synchronization with a `try_intern` increment that happened *without* the
  drop's lock (the increment at map.rs:484 is under the same `InternerState` lock — verify both paths take
  the same mutex so the re-check is sound). If both are under the lock, the race window is only between the
  atomic decrement (no lock) and acquiring the lock — which is exactly what the re-check covers.
- **Cross-shard handles:** can an `InternedString` from shard A ever be dropped against shard B's lock?
  If sharding is by string hash and stable, no — but confirm, since a wrong-shard reclaim would be
  corruption the per-shard check wouldn't catch.

### Investigation Log

#### Is the reclamation buffer-fill sentinel present in RELEASE builds, or debug-only?
- **Examined:** `lib/stringtheory/src/interning/map.rs:368-394` (`clear_reclaimed_entry`),
  `lib/stringtheory/src/interning/fixed_size.rs:435-460` (the analogous reclaim path), and grepped the
  whole `interning/` dir for `0x21` / `fill` / `debug_assert` / `cfg!` / `debug_assertions`.
- **Found:**
  - **Exact fill site (map.rs):** `map.rs:392` — `str_buf.fill(0x21);` inside the `unsafe` block at
    `map.rs:388-393`, within `fn clear_reclaimed_entry` (`map.rs:368`). It fills the entire string
    capacity of the tombstoned entry (`str_ptr = entry_ptr.add(1).cast::<u8>()`, length `str_cap`).
  - **No cfg gate of any kind.** There is no `#[cfg(debug_assertions)]`, no `if cfg!(debug_assertions)`,
    no `#[cfg(test)]`, no `#[cfg(loom)]` around the fill or around `clear_reclaimed_entry`. The fill is
    unconditional and therefore **present in release builds**. The only cfg-gated constructs in these
    files are `debug_assert!` macros (fixed_size.rs:278/286/325/390, map.rs:408) which are unrelated to
    the fill.
  - **Important discrepancy — two different sentinels in two different interner implementations:**
    - `map.rs:392` (the `InternerState`/`Map`-backed interner) fills with **`0x21`** (ASCII `!`).
    - `fixed_size.rs:458` (the `FixedSizeInterner` reclaim path, `fn` at fixed_size.rs ~430) fills with
      **`0xAA`** — *not* `0x21`. Same surrounding comment ("Write a magic value … signal that
      offsets/reclaimed entries are incorrect and overlapping"), same unconditional `unsafe { … fill() }`,
      but a different byte value. Both are unconditional / release-present.
- **Not found:** No conditional compilation, feature flag, or runtime toggle disabling either fill. No
  third fill site.
- **Conclusion:** RESOLVED. The buffer-fill-on-reclamation is unconditional and present in release builds,
  so an Antithesis assertion *can* rely on a fill sentinel to detect a stale read into a reclaimed slot
  rather than being forced to compute overlap directly. **However, the sentinel value is implementation-
  dependent:** `0x21` for the `map.rs` interner, `0xAA` for the `fixed_size.rs` interner. An assertion
  that hard-codes `0x21` would miss corruption in the `FixedSizeInterner` path. The robust check is either
  (a) match against the correct sentinel per implementation, or (b) check overlap directly (the
  implementation-independent invariant the loom test already uses). Detecting a *run* of either sentinel
  in a resolved `&str` is a valid corruption signal in the corresponding interner.
