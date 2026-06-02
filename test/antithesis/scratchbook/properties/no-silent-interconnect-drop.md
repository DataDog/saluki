---
slug: no-silent-interconnect-drop
sut_path: /home/ssm-user/src/saluki
commit: 042f41db3bd97118c38981765fd49696fce9d318
updated: 2026-05-28
type: Safety
priority: High
assertion_status: MISSING (net-new instrumentation)
---

# Property: No silent inter-component drop on a correctly-wired edge

## Origin
SUT analysis §4 ("Backpressure is real and is the load-safety mechanism") and §5
safety guarantee #1. Decomposed from the headline guarantee "ADP will not crash
under load, losing customer data" — the *no silent loss* half. No Antithesis
assertion exists (existing-assertions.md).

## What the code does

### Backpressure (the await-on-full path)
`lib/saluki-core/src/topology/interconnect/dispatcher.rs`:
- `DispatchTarget::send` (~86-123): when senders are present, it sends to all but
  the last sender via `sender.send(item.clone()).await` (~99-104) and the last via
  `last_sender.send(item).await` (~107-111). `mpsc::Sender::send().await` **blocks
  when the channel is full** — this is the backpressure. It returns `Err` only if
  the receiver has been dropped (channel closed), mapped to a `GenericError`.
- All edges are bounded `tokio::mpsc` (SUT analysis §2/§4). A slow downstream stalls
  the upstream send, which (in the DSD source) stalls the read loop:
  `sources/dogstatsd/mod.rs:1186` `memory_limiter.wait_for_capacity().await` plus the
  socket read in the same `'read` loop, so backpressure propagates to the socket.

### Silent discard (the disconnected-output path)
`dispatcher.rs:86-92`: when `self.senders.is_empty()`, `send` increments
`events_discarded_total` by `item.item_count()` and returns `Ok(())` — the events
are **dropped silently** (no error, no backpressure). This only happens for an
output with **zero** connected senders (a disconnected/un-wired output).

### Existing unit test (not an Antithesis assertion)
`sources/dogstatsd/mod.rs:2040-2063` `packet_forwarder_waits_when_queue_is_full`:
fills `FORWARDER_QUEUE_CAPACITY` then asserts a further `forward()` does NOT complete
within 100ms ("forwarding should wait for queue capacity instead of dropping").
Confirms the intended backpressure (await, not drop) behavior at the statsd-forward
boundary specifically.

## Failure scenario (Antithesis)
Sustained DSD load + a deliberately slow downstream consumer (e.g. throttle the
forwarder/intake so the encoder→forwarder edge and then all upstream edges fill).
Expectation: events are queued/awaited (backpressure to the socket), NOT discarded.
On a correctly-wired edge `events_discarded_total` must stay at 0; the only visible
effect is rising latency / falling socket read rate.

## Key observations
- The discard path and the backpressure path are mutually exclusive and chosen purely
  by `senders.is_empty()`. So the safety statement is precise: **a wired edge never
  discards; only a zero-sender edge discards.**
- Partial-delivery hazard (SUT §4): send to N senders is sequential and not atomic —
  if a *later* sender errors (receiver dropped) after earlier clones already sent, the
  earlier sends are not rolled back. This is a *connection-closed* (shutdown/teardown)
  case, not a full-channel case, so it does not contradict the no-discard-under-load
  property but should be excluded from the assertion window (see Open Questions).

## Config deps
- Channel/`interconnect_capacity` default still unread (SUT §9 open question) — sets how
  much buffering exists before backpressure engages; affects timing, not correctness.
- The discard path is reachable only by a topology wiring with an unconnected output.
  In the production DSD blueprint all three DSD outputs are wired, so on the production
  path the discard branch should be Unreachable under load.

## Suggested assertion (MISSING — net-new)
- **Always** on the wired edge: at every check, `events_discarded_total` for a connected
  output does not increase under sustained load (i.e. delta == 0). Anchor on the
  `events_discarded_total` counter scoped per output. Safety, every-check.
- **Sometimes(backpressure engaged)**: at least once, the source read loop is observed
  blocked on a full downstream channel (meaningful progress into the throttled state) —
  proves the fault actually exercised backpressure rather than the load being too light.
  Could be read from rising send_latency_seconds or a workload-side stall signal.

## SUT-side instrumentation needs
- `events_discarded_total` is already emitted; a workload-side checker can read it via
  the telemetry endpoint. For a crisp `Always`, an SDK `assert_always` at the discard
  site (`dispatcher.rs:90`) gated to "output has a name on the production DSD path" would
  fire only on the must-never branch — but note the discard branch is *legitimately*
  reachable for genuinely disconnected outputs, so a blanket `assert_unreachable` there
  is wrong. Prefer reading the counter from the workload for wired edges.

## Open questions
- **Does any production DSD output ever legitimately have zero senders?** If a named output
  (e.g. `dsd_debug_log_out` when debug logging disabled, or `dsd_stats_out`) is conditionally
  unwired, the discard path is reachable by config and the `Always(delta==0)` must be scoped
  to the always-wired outputs (`metrics`, `events`, `service_checks`, `dd_out` chain). If all
  outputs are always wired, the assertion can cover every edge.
- **Should partial-delivery on receiver-drop be excluded?** Yes during teardown; confirm the
  assertion window ends at shutdown signal so the not-atomic multi-sender path (a closed
  channel, not a full one) does not produce false positives.

## Investigation Log

#### Default `interconnect_capacity` (bounded mpsc size on topology edges)
- **Examined**: `lib/saluki-core/src/topology/mod.rs:37`; `blueprint.rs:56,76,87-88,94`;
  `built.rs:73,431-433,651-661` (channel construction); searched `bin/agent-data-plane` and
  `lib/saluki-app` for `with_interconnect_capacity` / overrides.
- **Found**: `const DEFAULT_INTERCONNECT_CAPACITY: NonZeroUsize = NonZeroUsize::new(128).unwrap();`
  (`mod.rs:37`). `TopologyBlueprint::new` seeds `interconnect_capacity` to this default
  (`blueprint.rs:76`). Each non-source event/payload edge builds a `mpsc::channel(interconnect_capacity.get())`
  (`built.rs:653,661`), so every interconnect channel is bounded at **128** entries by default.
  `with_interconnect_capacity()` exists (`blueprint.rs:87`) but no call site sets it outside the
  unit test at `built.rs:717` (capacity 10). No ADP/app config overrides it.
- **Not found**: No runtime/config knob exposing `interconnect_capacity`; it is a hardcoded
  compile-time default with only a programmatic setter that production code never calls.
- **Conclusion**: RESOLVED. Default interconnect capacity = **128** events/payloads per edge.
  Note this is a count of *event buffers* (`EventsBuffer`), not individual metrics, so the burst
  absorbed before backpressure is 128 buffers per downstream component. Property framing unchanged;
  this only sizes workload load to reach the full-channel state.
