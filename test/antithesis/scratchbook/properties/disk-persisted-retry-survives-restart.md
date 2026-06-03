---
slug: disk-persisted-retry-survives-restart
sut_path: /home/ssm-user/src/saluki
commit: 042f41db3bd97118c38981765fd49696fce9d318
updated: 2026-05-28
type: Liveness (with safety sub-clauses: no-duplication, poison-drop)
priority: High
assertion_status: MISSING (net-new instrumentation)
---

# Property: Disk-persisted retry transactions survive process kill+restart and are eventually delivered exactly once

## Origin
SUT analysis §2 (disk persistence), §3 (two disk-backed subsystems), §6 gap #3
("disk-persisted retry queue recovery never tested across a real kill+restart"), §9
open question (persisted.rs disk-full/partial-write/corrupt across crash). No Antithesis
assertion exists.

## What the code does

### Persistence enable + silent fallback
`lib/saluki-components/src/common/datadog/io.rs:391-409`: a `RetryQueue` is created; if
`config.retry().storage_max_size_bytes() > 0`, `with_disk_persistence(...)` is awaited. On
**init failure it logs and silently falls back to an in-memory-only `RetryQueue`** (~405-408):
"Failed to initialize disk persistence ... Transactions will not be persisted." This is a
durability *downgrade* with no hard failure — the operator believes persistence is on but it isn't.

### Flush to disk on shutdown
`io.rs:488-503`: on endpoint-task shutdown, `pending_txns.flush().await` is called; `flush`
(`io.rs:781-800`) pushes high-priority into low-priority then `self.low_priority.flush()`, which
persists outstanding transactions to disk **only if disk persistence is enabled** (`io.rs:769-776`
doc: "If disk persistence isn't enabled, all pending transactions will be dropped"). A flush error
logs "Events may be permanently lost" (~500-501).

### Exactly-once on consume (delete-before-return)
`lib/saluki-io/src/net/util/retry/queue/persisted.rs` `try_deserialize_entry` (~373-397): after
deserializing, it **deletes the file from disk before returning** (~391-394) "so that we don't risk
sending duplicates." So a successful pop removes the on-disk copy first — a crash *after* delete but
*before* send loses that one txn (at-most-once for the in-flight item), while a crash *before* delete
keeps it (at-least-once). The delete-before-return biases toward no-duplication.

### Poison/corrupt entry handling (drop, don't loop forever)
- `pop` (~206-243): on `try_deserialize_entry` `Err(e)` (corrupt/unreadable), it logs
  "Permanently dropping persisted entry that could not be consumed", decrements `total_on_disk_bytes`,
  increments `entries_dropped`, and `continue`s — does NOT retry the poison entry forever, does NOT
  abort recovery (~227-241).
- `remove_until_available_space` eviction path (~304-323): same poison handling during eviction.
- `try_deserialize_entry` deserialize failure (~373-389): attempts to `remove_file` the corrupt
  entry so it doesn't accumulate, tolerates removal failure.
- `refresh_entry_state` (~245-273): unrecognized files are warned and skipped, not fatal.

## Failure scenario (Antithesis)
1. Enable disk persistence (`forwarder_retry_queue_storage_max_size > 0`).
2. Drive a known set of transactions; induce an intake outage so they land in the retry queue.
3. SIGKILL the ADP process mid-flow (the s6 container supervisor restarts it).
4. Restore healthy intake.
5. Expectation: every persisted retryable transaction is eventually delivered, **exactly once**
   (no loss, no duplication). Separately: inject a corrupted on-disk entry and assert recovery
   continues and the corrupt entry is dropped (not retried forever, not crashing recovery).

## Key observations
- "Exactly once" is approximate at the crash boundary: delete-before-return means at most one
  in-flight txn can be lost on a crash in the delete→send gap, and at-least-once if crash precedes
  delete. The clean claim is **no systemic loss and no duplication of the persisted backlog**; the
  single in-flight item is a known narrow window.
- SIGKILL (not graceful) skips the shutdown flush (`io.rs:488-503`), so only transactions *already
  written to disk* survive; high-priority in-memory txns not yet persisted are lost. The graceful
  path (SIGTERM/30s) flushes them to disk. The property must distinguish kill vs graceful.
- Retry-queue IDs are stable across API-key rotation (`io.rs:514-533`) so a restart with a rotated
  key still finds and retries the same persisted backlog — relevant if the workload rotates keys.

## Config deps
- `forwarder_retry_queue_storage_max_size` (`storage_max_size_bytes`) > 0 to enable persistence.
- `storage_path`, `storage_max_disk_ratio` — disk-full eviction behavior
  (`remove_until_available_space`, `on_disk_bytes_limit`).

## Suggested assertion (MISSING — net-new)
- **Sometimes(persisted-backlog-fully-recovered)**: at least once, after a kill+restart with
  persistence enabled and intake restored, the set of transactions delivered post-restart covers the
  persisted backlog with no duplicates (reconcile workload input vs mock-intake received, dedup by
  transaction identity). Liveness + no-dup.
- **AlwaysOrUnreachable(poison-dropped)**: whenever a corrupt on-disk entry is encountered, it is
  dropped (entries_dropped increments) and recovery proceeds — never an infinite retry of the same
  entry and never a recovery abort. Anchor at `persisted.rs:227-241` / `304-323`.
- **Reachable(disk-persistence-actually-active)**: confirm persistence init succeeded (the
  silent-fallback at `io.rs:405-408` did NOT fire) — otherwise the whole property is vacuously testing
  in-memory mode. Treat the fallback as an Unreachable in the persistence-enabled workload, OR detect
  it and fail the run setup.

## SUT-side instrumentation needs
- SDK `assert_unreachable` (or workload detection) at the silent-fallback branch `io.rs:405` when
  persistence is configured — to catch the durability downgrade that would otherwise make the test
  vacuous.
- SDK `assert_reachable` at the poison-drop `continue` (`persisted.rs:238`) gated on the
  corrupt-entry test variant.
- Primary check is workload-side reconciliation against the mock intake with transaction-identity
  dedup; needs a deterministic countable input set and a mock intake that records received IDs.

## Open questions
- **Ordering after restart**: `refresh_entry_state` sorts by timestamp (~268) but filename timestamp
  has second granularity + nonce; confirm restart preserves enough ordering that the
  bias-to-freshest/oldest-drop semantics aren't inverted across a restart (affects which txns survive
  overflow, not raw loss).
- **The narrow at-most-once window** (delete-before-return then crash before send): is the single
  in-flight txn loss acceptable per the headline guarantee, or should the assertion tolerate it? Sets
  whether the reconcile allows a 1-txn slack.

### Investigation Log

#### Durability-downgrade visibility + torn-write classification + recovery wedging (2026-05-28)

**Examined:**
- `lib/saluki-components/src/common/datadog/io.rs:391-410` (RetryQueue create + `with_disk_persistence`
  + silent fallback) and grep of io.rs for `persistence`/`fallback`/metric near the branch.
- `lib/saluki-io/src/net/util/retry/queue/persisted.rs`: `try_from_path` (:30-37),
  `decode_timestamped_filename` (:410-427), `generate_timestamped_filename` (:400-408), `push`
  (:164-199), `pop` (:206-243), `refresh_entry_state` (:245-273), `try_deserialize_entry` (:354-398),
  `remove_until_available_space` poison handling (:304-330), and tests
  `pop_skips_corrupt_entry`/`pop_returns_none_when_all_entries_corrupt` (:701-795).

**Found — (a) durability downgrade is surfaced ONLY as an `error!` log, no metric:**
- On disk-persistence init failure, io.rs:405-408 runs `.unwrap_or_else(|e| { error!(endpoint_url,
  error = %e, "Failed to initialize disk persistence for retry queue. Transactions will not be
  persisted."); RetryQueue::new(queue_id, config.retry().queue_max_size_bytes()) })`. The only
  observable signal is that one `error!` log line; there is **no metric/gauge/counter** emitted to
  distinguish "persistence active" from "fell back to in-memory". Grep of io.rs for
  persistence/fallback finds only the doc comments (:393, :773-775) and this log (:406). So a
  workload cannot detect the downgrade via telemetry — it must scrape the log or, better, treat the
  fallback branch as an `assert_unreachable` when persistence is configured (as the file already
  recommends). Confirmed the downgrade is effectively silent at the metrics layer.
- **(a, cont.) the in-memory byte cap STILL holds after fallback:** the fallback constructs
  `RetryQueue::new(queue_id, config.retry().queue_max_size_bytes())` (io.rs:407) — identical
  in-memory cap to the non-persisted path (io.rs:391). So in degraded mode the queue is a plain
  capped in-memory queue with drop-oldest; the byte-cap invariant is preserved (it just becomes the
  drop-not-spill branch). No unbounded growth from the fallback.

**Found — torn/partial write is classified as CORRUPT (drop+warn), and does NOT wedge recovery:**
- `push` writes via `tokio::fs::write(&entry_path, &serialized)` (persisted.rs:184) directly to the
  final `retry-<ts>-<nonce>.json` path. NOTE: there is NO temp-file + atomic rename despite the
  stale comment at :165 ("Serialize the entry to a temporary file"). So a SIGKILL mid-write leaves a
  file with a **valid filename** but **truncated/partial JSON content**.
- On restart, `refresh_entry_state` (:245-273) scans the dir and calls `PersistedEntry::try_from_path`
  (:253), which validates ONLY the filename via `decode_timestamped_filename` (:31, :410-427) — it
  does NOT read or validate content. A torn write has a well-formed name, so it is accepted into
  `entries` (not skipped as "unrecognized"). Unrecognized files (bad name) are warned and skipped
  (:255-262) and the scan `continue`s past them.
- When `pop` reaches the torn entry, `try_deserialize_entry` reads the bytes and `serde_json::from_slice`
  fails (:373-375) → best-effort `remove_file` of the corrupt file (:378-384) → returns `Err`
  (:386-387). `pop` matches that `Err` (:227-240): logs `warn!` "Permanently dropping persisted entry
  that could not be consumed", decrements `total_on_disk_bytes`, `entries_dropped += 1`, and
  `continue`s the loop to the next entry (:230-239). So a torn write = **corrupt → dropped (with
  warn), counted in `entries_dropped`** — NOT treated as unrecognized-skip.
- A single bad file does **NOT** wedge recovery: `pop`'s `loop` advances to the next entry on every
  corrupt/poison hit (no infinite retry of the same entry — explicit comment at :228-229), and the
  eviction path `remove_until_available_space` has the same poison handling (:313-322). The "all
  entries corrupt" case returns `Ok(None)` cleanly (test at :774-795). Recovery proceeds past any
  number of bad files.
- Edge case: the `Ok(None)` branch in `pop` (:221-225, file vanished mid-recovery / `NotFound` at
  :358-364) triggers a `refresh_entry_state` + retry — also non-wedging, since the missing file is
  simply dropped from the rescanned set.

**Not found:** No metric for the persistence-fallback downgrade. No atomic write/rename or fsync in
`push` (torn writes are possible and are handled at read time, not prevented at write time). No code
path where a corrupt/torn file aborts recovery or is retried indefinitely.

**Conclusion (RESOLVED):** (a) The durability downgrade on disk-init failure is surfaced ONLY via a
single `error!` log — no metric — so the workload must treat the fallback branch (io.rs:405) as an
`assert_unreachable` (or log-scrape) to avoid a vacuous in-memory-mode test; the in-memory byte cap
still holds after fallback. (b) A torn/partial write across a kill is classified as a **corrupt
entry**: it is dropped with a `warn!` and `entries_dropped` increments, losing just that one
transaction; it does **not** wedge recovery — both the `pop` scan and the eviction scan continue past
any number of corrupt/unrecognized files. This validates the proposed `AlwaysOrUnreachable(poison-
dropped)` assertion and confirms the "torn write loses one txn, not the backlog" framing. (Caveat for
the workload: because writes are non-atomic, the corrupt-entry test variant can be produced naturally
by SIGKILL mid-write, not only by injecting a hand-crafted corrupt file.)
