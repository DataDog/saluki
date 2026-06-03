---
slug: forwarder-eventual-delivery
sut_path: /home/ssm-user/src/saluki
commit: 042f41db3bd97118c38981765fd49696fce9d318
updated: 2026-05-28
type: Liveness
priority: High
assertion_status: PARTIAL — in-SUT `Sometimes(forwarded a payload)` landed 2026-05-31; recovery-reconciliation `Sometimes` still net-new
---

# Property: After a transient intake outage clears, accepted-and-retryable transactions are eventually delivered

## Origin
SUT analysis §5 liveness #5 ("After a transient intake outage clears, queued data is
eventually delivered") and §2 egress description. Headline guarantee's *no silent loss*
half, in the egress/forwarder path. No Antithesis assertion exists.

## What the code does

### Retry model = circuit breaker + re-enqueue (not inline retry)
`lib/saluki-components/src/common/datadog/io.rs`:
- In-flight completion handler (~451-482). On a circuit-breaker-open result
  `Err(RetryCircuitBreakerError::Open(req))` (~468-474): the request is reassembled into a
  `Transaction` and **re-enqueued to the low-priority queue** via `pending_txns.push_low_priority`.
  Only if *that re-enqueue itself errors* does it log "Events may be permanently lost." On
  success it tracks queue drops (overflow eviction) telemetry.
- `process_http_response` (~541-563): success → `track_successful_transaction`. Non-success →
  `track_permanently_failed_transaction` (these are statuses the classifier did NOT mark
  retryable — see below — so they are permanent drops by design).
- `Err(RetryCircuitBreakerError::Service(e))` (~460-463): an error the **retry policy declined to
  retry** → `track_permanently_failed_transaction` (permanent drop). Per the breaker logic below,
  this branch is reached only for *non-retryable* outcomes; retryable transport errors do NOT land
  here.

### Circuit breaker mechanics — what becomes Open vs Service
`lib/saluki-io/src/net/util/middleware/retry_circuit_breaker.rs` `ResponseFuture::poll` (~95-128):
after the inner request completes, it calls `state.policy.retry(&mut req, &mut result)`.
- `Some(backoff)` (policy says retry) → arms the shared backoff and returns `Err(Error::Open(req))`
  carrying the original request (~101-112). This is what the io.rs handler re-enqueues.
- `None` (policy declines) or request-not-cloneable → `Err(Error::Service)` (~113-121), the permanent
  branch in io.rs.
The policy wraps `StandardHttpClassifier` whose `should_retry(Err(_)) == true`
(`classifier/http.rs:78-83`) and `StandardHttpRetryLifecycle` which explicitly categorizes
DNS/connection/TLS transport errors (`lifecycle/http.rs:76-100`). **Therefore connection resets,
timeouts surfacing as transport errors, and 5xx are routed to `Error::Open` → re-enqueued, NOT
dropped via `Service`.** The earlier worry that connection resets are permanently dropped is
resolved: they are retryable and re-enqueued, provided the request was cloneable.

### Retry classification — which failures are retryable
`lib/saluki-io/src/net/util/retry/classifier/http.rs`:
- `default_should_retry` (~12-26): **400 / 401 / 403 / 413 → NOT retried** (treated as permanent
  client misconfig/bug). All other 4xx and all 5xx → retried. `should_retry(Err(_))` (~81) →
  transport errors retried.
- So: 5xx storms, timeouts (408/504), 429, 5xx, and transport errors are retryable → must be
  eventually delivered after the outage clears. 400/401/403/413 are a permanent drop by design
  (out of scope for the liveness claim).

## Failure scenario (Antithesis)
Accept a known set of transactions, then inject a transient intake outage: 5xx storm +
timeouts + connection resets for a bounded window, then restore healthy 2xx. Liveness
expectation: every transaction that was (a) accepted and (b) retryable is eventually delivered
(observed at the mock intake), with no permanent loss — assuming the retry queue did not overflow
(see Open Questions / overflow tension).

## Key observations
- This is a **liveness** property: the bad outcome is "never delivered." It needs an eventual
  window after fault clearance; assert progress, not an instantaneous invariant.
- The re-enqueue is to the **low-priority** queue, which is also the overflow target; under a
  long outage the queue can overflow and drop *oldest* (SUT §2 two-tier `PendingTransactions`,
  bias to freshest). So the clean liveness claim holds only for outages short enough that the
  retry queue does not overflow. Beyond that, eventual delivery is intentionally sacrificed for
  bounded memory (the §5-liveness-#4 tension).
- `track_permanently_failed_transaction` and `track_queue_drops` telemetry are the observable
  loss signals; `track_successful_transaction` is the delivery signal.

## Config deps
- `forwarder_retry_queue_max_size_bytes` (`queue_max_size_bytes`) — overflow threshold; sets how
  long an outage can last before eventual delivery is no longer guaranteed.
- Circuit breaker backoff schedule (exponential + jitter) — sets recovery latency, hence the size
  of the "eventually" window the assertion must allow.

## Suggested assertion
- **Landed 2026-05-31 — in-SUT delivery anchor:** `assert_sometimes!("ADP forwarded a payload to the
  intake", { domain })` at the success branch of `process_http_response` (io.rs:553, behind
  `saluki-components/antithesis`). This is the in-SUT proof that delivery *happens* (a 2xx from the
  intake) and a replay checkpoint on a healthy-forwarding state — but it is **not** the recovery
  property: a run-wide `Sometimes(forwarded)` is satisfied by any single delivery and says nothing
  about *post-outage* completeness.
- **Still net-new — Sometimes(all-accepted-retryable-delivered-after-recovery):** at least once, after
  a transient outage clears and within a bounded window, the count of delivered transactions equals
  the count of accepted-and-retryable transactions submitted before/during the outage (queue did not
  overflow). This proves recovery actually happens. Best evaluated workload-side by reconciling the
  controlled input set against the mock-intake received set.
- Supporting **Reachable**: the `Error::Open` re-enqueue path (`io.rs:468-474`) is hit at least once
  (proves the circuit breaker engaged and re-enqueued, not silently dropped).

## SUT-side instrumentation needs
- An SDK `assert_reachable` at the re-enqueue site (`io.rs:470`) to confirm the breaker re-enqueues.
- Primary check is workload-side reconciliation against the mock intake (needs a deterministic,
  countable input set and a mock intake that records received transaction IDs).

## Open questions
- **Retry-queue overflow bound under the test's outage length** — must size the outage shorter than
  overflow, or the assertion must explicitly exclude overflowed (oldest-dropped) transactions. The
  overflow drop (`track_queue_drops`) is the legitimate bounded-memory escape valve, so eventual
  delivery is only guaranteed for outages that don't overflow `queue_max_size_bytes`.

### Investigation Log

#### (a) Are forwarder requests always cloneable? (b) Is breaker backoff per-endpoint? (2026-05-28)

**Examined:**
- `lib/saluki-io/src/net/util/middleware/retry_circuit_breaker.rs` in full — `ResponseFuture::poll`
  (:81-130), `RetryCircuitBreaker::call`/`poll_ready` (:218-258), `State`/`new` (:132-205),
  `Layer for RetryCircuitBreakerLayer` (:164-173).
- `lib/saluki-io/src/net/util/retry/policy/rolling_exponential.rs:95-141` (`Policy` impl, incl.
  `clone_request`).
- `lib/saluki-components/src/common/datadog/io.rs:351-410` (`run_endpoint_io_loop` service build incl.
  the breaker layer at :385-388) and `:236-278` (per-endpoint task spawn loop).
- `lib/saluki-components/src/common/datadog/transaction.rs:55-243` (`TransactionBody<B>` and
  `Transaction<B>`, incl. `#[derive(Clone)]` at :58 and :203).
- `lib/saluki-components/src/forwarders/datadog/mod.rs:83-132` (concrete forwarder instantiation).
- `lib/saluki-common/src/buf/chunked.rs:102-103` (`FrozenChunkedBytesBuffer`).

**Found — (a) requests ARE always cloneable on the production path (re-enqueue, not permanent drop):**
- The breaker layer sits at io.rs:385-388, applied to `Request<TransactionBody<B>>` (the body→
  `ClientBody` conversion is the *inner* `map_request` at :388, AFTER the breaker, by explicit design
  per the comment at io.rs:374-376 — so the request the breaker clones/holds is
  `Request<TransactionBody<B>>`).
- The non-cloneable → `Error::Service` permanent-drop path (retry_circuit_breaker.rs:118-121) is
  reached only when `state.policy.clone_request(&req)` returns `None` (:248 → `req: None` → `take()`
  is `None` at :100,:118). The production policy is `RollingExponentialBackoffRetryPolicy`, whose
  `clone_request` is `Some(req.clone())` unconditionally (rolling_exponential.rs:138-140) and which
  bounds `Req: Clone` (:99). It NEVER returns `None`. (The only `None`-returning impls are
  `NoopRetryPolicy` at policy/mod.rs:19 and the test-only `NonCloneableTestRetryPolicy` — neither is
  on the forwarder path.)
- The concrete `B` in production is `FrozenChunkedBytesBuffer`
  (`TransactionForwarder<FrozenChunkedBytesBuffer>`, forwarders/datadog/mod.rs:132), which is
  `#[derive(Clone)]` (chunked.rs:102-103). `TransactionBody<B>` is `#[derive(Clone)]` (transaction.rs:58)
  and `Request<T>: Clone` when `T: Clone`. So `clone_request` always succeeds.
- Therefore every retryable outcome routes to `Error::Open(req)` (retry_circuit_breaker.rs:101-112) →
  re-enqueued to the low-priority queue at io.rs:468-474. The "non-cloneable → silent permanent loss"
  worry is **NOT realizable** on the production forwarder. RESOLVED.

**Found — (b) circuit-breaker backoff is PER-ENDPOINT (no cross-endpoint serialization):**
- The backoff lives in `State { policy, backoff }` behind `Arc<Mutex<State<P>>>`, created fresh in
  `RetryCircuitBreaker::new` (retry_circuit_breaker.rs:200-205). That constructor runs once per
  `Layer::layer` call (:170-172).
- `run_endpoint_io_loop` builds its own `service` with its own `.layer(RetryCircuitBreakerLayer::new(...))`
  (io.rs:385-388). Crucially, each endpoint gets its **own** `run_endpoint_io_loop` task: the spawn
  loop at io.rs:253-278 iterates `resolved_endpoints` and calls `spawn_traced_named(...,
  run_endpoint_io_loop(...))` once per endpoint, each with its own `endpoint_rx` channel,
  `pending_txns`/`RetryQueue`, and breaker `State`.
- The shared upstream `service` is `.clone()`d into each task (io.rs:273), but the breaker `State`
  `Arc<Mutex<...>>` is constructed *inside* each task's `ServiceBuilder` (io.rs:385), so each endpoint
  has an independent `backoff`. The policy is `.clone()`d per layer (:171) but state is not shared.
  One endpoint's open breaker (its `state.backoff = Some(...)`, retry_circuit_breaker.rs:106-108,
  gating `poll_ready` at :222-229) cannot stall another endpoint's recovery. RESOLVED.

**Not found:** No global/static breaker state, no shared backoff future across endpoints, and no
production code path that supplies a non-cloneable request or a `None`-returning `clone_request` to
the forwarder breaker.

**Conclusion:** Both sub-questions resolved favorably. (a) Production transactions are always
cloneable (`FrozenChunkedBytesBuffer` → `TransactionBody` → `Request`, all `Clone`; policy always
clones), so retryable failures take the `Error::Open` re-enqueue path — no silent permanent drop via
`Error::Service` for retryable errors. (b) Each endpoint task owns an independent circuit breaker and
backoff, so multi-endpoint fan-out recovers per-endpoint; one slow endpoint does not serialize
others. The "eventually" window in the liveness assertion is correctly per-endpoint. The remaining
real caveat is unchanged: eventual delivery holds only for outages short enough that the low-priority
retry queue does not overflow `queue_max_size_bytes` (drop-oldest).
