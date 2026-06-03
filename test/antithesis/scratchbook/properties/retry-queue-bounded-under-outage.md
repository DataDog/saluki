# retry-queue-bounded-under-outage

**Family:** Resource Boundaries — queues / backpressure / exhaustion
**Status:** Verified against code at commit 042f41db3b. The byte-cap + drop-oldest invariant is
**expected to HOLD** (it is genuinely enforced). The tension is that staying bounded *implies
silent data loss* on prolonged outage — both halves are properties worth asserting.

## What led to the property

The headline guarantee is "won't crash under load, losing customer data," but `sut-analysis.md`
§2/§5 (Liveness 4) flags a real tension: the forwarder retry queue caps memory, which means a
*prolonged* intake outage forces drops. This property pins down the safety half precisely: under
a sustained outage the in-memory + disk retry queue stays within configured byte caps and
overflow drops the **oldest** entries (bias to freshest data), always **counted**, never growing
unbounded. The existing suite never tests intake-down at the system level (§6 gap 2).

## Behavior in code

Egress is `TransactionForwarder` (`lib/saluki-components/src/common/datadog/io.rs`); each resolved
endpoint owns a `PendingTransactions` (two-tier: high-priority in-memory `VecDeque` for fresh
data + low-priority `RetryQueue` for retries/overflow). Under outage the retry circuit breaker
re-enqueues to the low-priority queue, so the `RetryQueue` is where unbounded growth would happen.

**In-memory cap with drop-oldest** — `RetryQueue::push`
(`lib/saluki-io/src/net/util/retry/queue/mod.rs:179-220`):
```rust
if current_entry_size > self.max_in_memory_bytes {            // single entry too big => Err
    return Err(generic_error!("Entry too large to fit into retry queue. (...)"));
}
while !self.pending.is_empty()
      && self.total_in_memory_bytes + current_entry_size > self.max_in_memory_bytes {
    let oldest_entry = self.pending.pop_front()...;            // OLDEST first
    if let Some(persisted) = &mut self.persisted_pending {
        persisted.push(oldest_entry).await?;                  // spill to disk if enabled
    } else {
        push_result.track_dropped_item(&oldest_entry);        // else DROP + COUNT
    }
    self.total_in_memory_bytes -= oldest_entry_size;
}
self.pending.push_back(entry);
self.total_in_memory_bytes += current_entry_size;
```
So `total_in_memory_bytes` is held at/under `max_in_memory_bytes` by evicting oldest-first; with
no disk persistence the evicted entries are dropped and tracked in `PushResult`.

**Disk cap with drop-oldest** — `PersistedQueue` enforces a disk-byte limit, evicting oldest on
overflow (`retry/queue/persisted.rs:285-330`): `while !entries.is_empty() && total_on_disk_bytes
+ required_bytes > limit { ... track_dropped_item(...); entries_dropped += 1; }`. The limit is
`min(max_on_disk_bytes, total_space * storage_max_disk_ratio)` (`persisted.rs:343-349`,
ratio default 0.8). Corrupt/unconsumable persisted entries are also permanently dropped and
counted (`persisted.rs:234-238, 317-321`).

**Drops are surfaced as telemetry** — `PushResult` (`mod.rs:49-78`) carries
`items_dropped`/`events_dropped`/`data_points_dropped`; every push site routes it through
`track_queue_drops` (`io.rs:535-539`, called at `io.rs:420, 471, 498`), and persisted-entry drops
flow via `take_persisted_entries_dropped` => `low_prio_queue_entries_dropped` (`io.rs:739-744`).
The `#[must_use]` on `PushResult` (`mod.rs:49`) makes dropped-item info hard to ignore. So drops
are counted, not fully silent — but they are still **data loss** with only a counter to show it.

## Failure scenario (Antithesis)

Drive sustained load into DSD while Antithesis holds the mock intake down (connection refused /
black-hole / 5xx storm / slow) long enough for the retry queue to saturate. Assert:
1. **Safety:** `total_in_memory_bytes <= max_in_memory_bytes` and on-disk bytes `<= disk limit`
   at all times — the queue never grows unbounded (`Always`).
2. **Liveness/loss:** `Sometimes(items_dropped > 0)` once saturated — proves the bound is real and
   the drop path is exercised (the data-loss reality of the guarantee).
3. **Recovery (cross-ref, separate property):** after the outage clears, queued data drains
   high-priority-first. Antithesis interleaving stresses the shared circuit-breaker backoff +
   per-endpoint queues that the deterministic harness never exercises.

## Suggested assertions (NET-NEW — see existing-assertions.md: NO SDK assertions exist)

- `Always(total_in_memory_bytes <= max_in_memory_bytes)` in `RetryQueue::push`/after-push; and
  the analogous disk-bytes `Always` in `PersistedQueue::push`. Safety: must hold every check; the
  eviction `while` loops make this a true invariant, so a real `Always`.
- `Sometimes(push_result.items_dropped > 0)` — proves saturation/eviction is reached (the
  bound is actually load-bearing, not vacuous). Liveness/progress.
- `Sometimes(persisted_entries_dropped > 0)` when disk persistence is enabled — proves the disk
  cap also evicts. Optional path => Sometimes, not Always.
- Consider `Reachable` on the "entry too large to ever fit" `Err` branch (`mod.rs:184-189`) if
  the workload can produce an oversized payload — a distinct failure mode (the whole entry is
  rejected, not evicted).

SUT-side beats workload-only: a workload checker at the mock intake sees *which* metrics never
arrive but cannot distinguish "dropped by retry-queue overflow" from "dropped as 400/401/403/413
permanent failure" (`classifier/http.rs`) from "still queued." The byte-bound invariant is only
observable from inside `RetryQueue`.

## Configuration dependencies

- `forwarder_retry_queue_payloads_max_size` / `forwarder_retry_queue_max_size` => in-memory byte
  cap; default **15 MiB** when both unset (`retry.rs:97-104, 160-166`,
  `FORWARDER_RETRY_QUEUE_PAYLOADS_MAX_SIZE_BYTES`).
- `forwarder_storage_max_size_in_bytes` (disk cap) default **0 => disk persistence DISABLED**
  (`retry.rs:39-41, 110-113, 169-171`; gated at `io.rs:394`). So by default overflow goes
  straight to **drop**, not disk. Disk path only active when operator sets a nonzero value (and
  `forwarder_storage_path`).
- `forwarder_storage_max_disk_ratio` (default 0.8) caps disk usage relative to total volume space.
- Per-endpoint: each resolved endpoint has its own queue, so the *aggregate* memory bound is
  `num_endpoints * max_in_memory_bytes` (+ disk). Multi-endpoint fan-out multiplies the cap.

## Open questions

- Is the aggregate bound across all endpoints (`num_endpoints * 15 MiB` in-memory) the right thing
  to assert, or is the per-endpoint cap sufficient? With many additional endpoints the total can
  be large; matters for whether "bounded" really protects process RSS.
- Does `IncomingBytesPerSec` / queue-duration accounting (`io.rs:582-639`) feed any *additional*
  drop policy (time-based eviction) beyond the byte cap, the way the Datadog Agent's
  queue_duration_capacity does? If so the bound is byte-AND-time and the assertion must cover both.

### Investigation Log

#### Disk-init fallback byte-cap + corrupt-file wedging under fault injection (2026-05-28)

**Examined:** `lib/saluki-components/src/common/datadog/io.rs:391-410` (queue create + silent
fallback); `lib/saluki-io/src/net/util/retry/queue/persisted.rs` `pop` (:206-243),
`refresh_entry_state` (:245-273), `try_deserialize_entry` (:354-398), `push` (:164-199),
`remove_until_available_space` (:304-330). (Full trace recorded in
`disk-persisted-retry-survives-restart.md` Investigation Log, 2026-05-28.)

**Found:**
- **Byte cap holds in the degraded (fallback) mode.** Disk-init failure falls back to
  `RetryQueue::new(queue_id, config.retry().queue_max_size_bytes())` (io.rs:407) — the same
  in-memory cap as the non-persisted path (io.rs:391). Degraded mode is just the drop-oldest /
  drop-not-spill branch of `RetryQueue::push`, so `total_in_memory_bytes <= max_in_memory_bytes`
  still holds. The disk-path `Always` is vacuous in fallback mode (no disk queue exists), but the
  in-memory `Always` is preserved.
- **Durability downgrade is surfaced only as an `error!` log (io.rs:406), no metric** — so a
  bounded-queue workload that intends to exercise the disk cap must detect the fallback (log-scrape
  or `assert_unreachable` at io.rs:405) or it will silently be testing the in-memory cap instead.
- **A corrupt / torn-written persisted file does NOT wedge the queue and does NOT break the disk
  byte cap.** `pop` drops corrupt entries (warn + `entries_dropped++` + decrement
  `total_on_disk_bytes`) and `continue`s past them (persisted.rs:227-240); the eviction path does
  the same (:313-322); unrecognized-named files are skipped during the scan (:255-262). Dropping a
  corrupt entry *decrements* `total_on_disk_bytes`, so it can only help the cap, never violate it.
  Writes are non-atomic (`tokio::fs::write` direct to final path, :184, no temp+rename), so a
  SIGKILL mid-write yields a valid-name/truncated-content file → classified corrupt → dropped on
  read. Recovery proceeds past any number of such files. The "~47 unwrap/expect" concern: the
  recovery/eviction hot paths use `Result`-propagating `?`/match on the deserialize and IO error
  paths, not `unwrap`, so a corrupt file surfaces as a handled `Err`, not a panic.

**Not found:** No path where a corrupt/torn file inflates `total_on_disk_bytes` past the limit or
halts eviction/recovery; no metric for the fallback downgrade.

**Conclusion (RESOLVED):** The byte-cap `Always` invariant holds under disk-init fallback (in-memory
cap unchanged) and under corrupt/torn-file fault injection (corrupt entries are dropped and decrement
the byte total, never wedge the queue). The disk-path `Always` is testable on clean disks and is not
violated by corrupt files; in fallback mode only the in-memory `Always` applies. Workloads targeting
the *disk* cap must guard against the silent fallback (log-only, no metric).
