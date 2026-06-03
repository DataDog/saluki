# memory-limiter-survives-rss-read-failure

**Family:** Resource Boundaries — memory / fault tolerance
**Status:** Verified against code at commit 042f41db3b. Property is expected to **FAIL by design**:
an RSS read failure mid-run panics the limiter thread and silently freezes backpressure. Needs
SUT-side instrumentation to express well.

## What led to the property

`sut-analysis.md` §4 and §7 flag the limiter checker thread's `.expect()` on the RSS read as a
fail-open hazard: if `/proc` reads start failing mid-run (transient `/proc` unavailability, fault
injection, namespace/cgroup churn), the dedicated checker thread panics and dies. Memory
protection — already the only runtime memory mechanism — silently vanishes, and the
last-written backoff value is **frozen** in the `AtomicU64` forever.

## Behavior in code

`MemoryLimiter::new` smoke-tests RSS availability once at construction
(`lib/resource-accounting/src/limiter.rs:43-44`: `Querier::default().resident_set_size()?` — if
unavailable at startup, returns `None` and the caller falls back to a noop limiter via
`accounting.rs:175-177`). But the **steady-state checker loop** does not tolerate later failures
(`limiter.rs:99-122`):
```rust
loop {
    let actual_rss = querier
        .resident_set_size()
        .expect("memory statistics should be available");   // <-- panics the thread mid-run
    let maybe_backoff_duration = calculate_backoff(...);
    match maybe_backoff_duration {
        Some(d) => active_backoff_nanos.store(d.as_nanos() as u64, Relaxed),
        None    => active_backoff_nanos.store(0, Relaxed),
    }
    std::thread::sleep(Duration::from_millis(250));
}
```
Consequences when `resident_set_size()` returns `None` mid-run:
1. `.expect()` panics. The thread is a bare `std::thread` (`limiter.rs:54-65`), not a supervised
   task and not in the data-topology JoinSet — its death does **not** trigger the process-wide
   shutdown that data-component exits cause. The process keeps running.
2. The loop stops updating `active_backoff_nanos`. Whatever value was last stored is **frozen**:
   - If it was 0 (RSS was below threshold when reads failed), backpressure is permanently off —
     fail-open, no protection. Cooperating tasks (`wait_for_capacity`, `limiter.rs:83-88`) never
     wait again even as RSS climbs.
   - If it was a nonzero backoff, that exact backoff is applied forever regardless of actual RSS
     — including after RSS would have dropped, needlessly throttling the source indefinitely.
3. No telemetry or log surfaces the thread death; `memory_limiter.current_backoff_secs`
   (`limiter.rs:111,116`) simply stops updating. Observability goes stale silently.

So the property — "memory protection remains active (or the failure is surfaced) when RSS reads
fail" — is violated: protection silently freezes and the failure is not surfaced.

## Failure scenario (Antithesis)

Run with the limiter enabled (`memory_mode: permissive|strict` + `memory_limit` set,
`enable_global_limiter: true`). Use Antithesis fault injection to make RSS reads fail mid-run
(e.g. interfere with `/proc/self/statm` or the platform stat source the `process_memory::Querier`
uses) while a load generator pushes RSS toward the limit. Observe that the checker thread dies,
backoff freezes, and no error is surfaced. The race that makes the freeze damaging — reads fail
*before* RSS crosses the threshold, leaving backoff at 0 — is exactly the kind of timing-ordering
Antithesis explores and the deterministic harness cannot.

## Suggested assertions (NET-NEW — see existing-assertions.md: NO SDK assertions exist)

This property needs SUT-side instrumentation; a workload-only checker can only see "RSS grew
unbounded after a fault," which is indistinguishable from the off-by-default limiter case.

- `Unreachable` on the panic path: wrap the RSS read so the `.expect()` site is replaced with a
  branch that, if it would have panicked, fires `assert_unreachable("limiter RSS read failed —
  protection lost")`. The panic-the-checker-thread state is a critical-failure state that should
  never be observed. (Today it IS observed — that is the finding; the assertion makes it a
  reportable property rather than a silent thread death.)
- Alternatively, if the fix is to surface-and-continue: `Sometimes(rss_read_failed_and_surfaced)`
  on a new error-reporting path (logged + telemetry incremented + protection conservatively kept
  active, e.g. retain a safe backoff). `Sometimes` because the failure is a rare optional path we
  want to prove is *handled* at least once, not an always-true invariant.
- Anchor a `Sometimes(active_backoff_nanos updated within last N ms)` liveness check to detect a
  frozen/dead checker — proves the limiter is still doing work, not stuck.

Honest framing: today there is no surfaced-error path, so the realistic immediate assertion is
the `Unreachable` on the panic site (which will fire), documenting the fail-open. The
`Sometimes(surfaced)` form presupposes an SUT-side fix and should be tagged as fix-dependent.

## Configuration dependencies

- Requires the limiter to actually be running: `memory_mode != disabled`, `memory_limit` set,
  `enable_global_limiter: true`. Under the default `disabled` mode the limiter is a noop with no
  checker thread, so this failure mode does not even exist (a separate, larger problem — no
  protection at all; see `rss-bounded-under-cardinality`).
- Platform: `process_memory::Querier` backing source (e.g. `/proc` on Linux) determines what
  "RSS read failure" means and how to inject it.

## Open questions

- Can `process_memory::Querier::resident_set_size()` actually return `None`/error *after*
  succeeding once at startup, on the Antithesis Linux target? If the underlying read effectively
  cannot fail post-startup on this platform, the panic is only reachable via injected `/proc`
  corruption — which determines whether this is a realistic production risk or a fault-injection-
  only curiosity. This is the pivotal question for priority.
- Is the frozen backoff value more likely 0 (fail-open, no protection) or nonzero (fail-stuck,
  over-throttle) in practice? Both are bugs but with opposite symptoms; determines which
  assertion/observable to lead with.
- Should the correct behavior be "keep last-known protection" or "fail loudly and shut down"?
  Given data components are fail-stop and the container s6 supervisor restarts ADP on exit, a
  loud crash might be *safer* than silent freeze. The intended remediation changes whether the
  property is framed as Unreachable(panic) or Reachable(clean restart).
