---
slug: adp-keeps-delivering
title: ADP still processes and delivers after load (functional liveness)
type: Liveness
priority: Medium
sut_path: /home/ssm-user/src/saluki
commit: 2e4ae1b8be45143882f0dbeb5e74998021c5faf9
updated: 2026-05-31
status: partial — in-SUT good-function `Sometimes` LANDED; per-branch wedge `assert_always` MISSING
---

# adp-keeps-delivering — ADP still processes and delivers after load

## Property (one sentence)
After the load drivers run, in a faults-paused window the mock intake has received metrics *and* ADP
still serves `:5100` — i.e. ADP is not merely up but not wedged.

## Origin
Strengthens [`adp-stays-alive`](adp-stays-alive.md): a process can be reachable on `:5100` yet have
stopped processing/forwarding (deadlock, stalled pipeline, dropped-everything). The existing
`finally_verify_delivery` harness command already polls the mock intake and fires a
`Reachable`/`Sometimes` anchor for end-to-end delivery — this property upgrades that to a per-branch
liveness assertion so a wedged-but-alive ADP becomes a counterexample.

## Relationship to existing instrumentation
- `existing-assertions.md`: `finally_verify_delivery` carries a `Sometimes(delivered > 0)` anchor —
  good for "delivery happens at least once across the run," but it does **not** fail on a branch
  where ADP wedged after accepting load. This property is the missing per-branch `assert`.

## The fault-gating mechanism
Same as `adp-stays-alive`: evaluate in a quiet period (`finally_`, or `ANTITHESIS_STOP_FAULTS`), so a
fault that merely delayed delivery recovers and passes, while a self-inflicted wedge persists and
fails. Note: no-loss/delivery reconciliation must use UDS or TCP ingress, not UDP (catalog R3).

## Implementation status
- **Landed (good-function half):** an in-SUT `assert_sometimes!("ADP forwarded a payload to the
  intake", { domain })` at the forwarder's 2xx site
  (`lib/saluki-components/src/common/datadog/io.rs`, in the `status.is_success()` branch of
  `process_http_response`), behind the new `saluki-components/antithesis` feature (enabled
  transitively by `agent-data-plane/antithesis`). A 2xx means the whole ingest→aggregate→encode→
  forward pipeline ran, so this proves a *booted ADP actually works*, and as a `Sometimes` it also
  gives Antithesis a replay checkpoint anchored on a healthy-forwarding state. This is the in-SUT
  counterpart to the workload-side `Sometimes(delivered > 0)` already in `finally_verify_delivery`,
  and it doubles as the in-SUT seed for [`forwarder-eventual-delivery`](forwarder-eventual-delivery.md).
- **Still net-new (the per-branch wedge detector):** extend the `finally_verify_delivery` command so
  that, in the faults-paused window, it polls the mock intake's dump endpoint and `:5100` and asserts
  `assert_always!(delivered_recently && reachable, …)`. This is what catches "ADP accepted load, then
  wedged" on a *specific* branch — neither the run-wide `Sometimes` above nor a bare `:5100`
  reachability check fails on that branch.

## Assertion-type rationale
**Liveness** (a good thing — delivery — eventually happens after load), realized as an
`assert_always!` inside the faults-paused `finally_` after a bounded poll, for the same reason as
`adp-stays-alive`.

## Open Questions
- "Delivered recently" needs a freshness window relative to the last driver batch — define it so a
  stale earlier delivery doesn't mask a current wedge.
- Whether to count only metrics or also events/service-checks delivered (ties to
  `events-sc-no-silent-loss`).
