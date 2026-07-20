---
sut_path: /home/ssm-user/src/saluki
commit: 93b9f37baf07004a9235f308305d78ef3d7fc226
updated: 2026-07-16
external_references:
  - path: https://github.com/DataDog/saluki/pull/2112#issuecomment-4993034650
    why: the contested boot-retry fix; states the exact gap this note resolves
  - path: https://datadoghq.atlassian.net/browse/SMPTNG-766
    why: boot-fatal on transient Core-Agent gRPC error, the fix under review
---

# Finding: "ADP is online and able to service load" is not soundly assertable

## The question this answers

PR #2112 makes ADP's boot-time Core-Agent gRPC calls (hostname, registration, health) retry in the background instead of blocking boot and crashing on timeout. A reviewer asks why background-retry over block-then-crash. The unresolved half of the PR is whether the background-retry path **harms liveness** — whether ADP can proceed past boot into a state where it accepts load but cannot service it. This research set out to build a cheap Antithesis scenario demonstrating liveness is unharmed. The conclusion is that no such sound assertion exists. That is the finding, not a failure to build one.

## Why no sound assertion exists

1. **Reachable is not servicing.** The only liveness assertion available, `eventually_adp_alive`, checks a TCP connect to the API on :5100 and the presence of the DogStatsD socket file. The API server and the socket listener are separate tasks from the data pipeline. Both bind and answer while the pipeline is wedged or half-initialized. So the assertion proves the listeners came up, exactly the "boot faults resolved" half the PR already demonstrates. It cannot see whether load is serviced end to end.

2. **Servicing load is not an invariant under the fault model.** A liveness property is only soundly assertable when the good thing is guaranteed to eventually happen. "Load goes out" is not guaranteed. The forwarder sheds — it drops to the retry queue rather than blocking (`common/datadog/io.rs:588`, `:640`) — so under an always-on network partition, ADP correctly does not flush. A missing flush is legitimate behavior, not a wedge. An `always`/`eventually` "load went out" assertion therefore false-REDs on healthy ADP whenever a partition or a clock_jitter/cpu_mod-delayed flush is in play. This is the throughput-liveness-gauge pitfall. There is no green-by-default form of it.

3. **The failing link is loopback, so the fault cannot be isolated.** ADP dials the Core Agent at a hardcoded `https://127.0.0.1:{cmd_port}` (`lib/datadog-agent/commons/src/ipc/config.rs:209`). The Core-Agent↔ADP boot gRPC lives inside one converged container. Antithesis inter-container network faults do not reach it. You can fault the whole container — pausing Core Agent and ADP together — or the irrelevant ADP→intake link, but you cannot inject a partition on the exact link that causes the known problem, as an independent variable, in a compose topology. So even a scenario that could observe servicing could not attribute a result to the boot Core↔ADP fault.

4. **The boot edge is ambiguous by construction.** Background-retry means boot "completes" and the listeners bind before a backgrounded dependency has necessarily resolved. Part of the boot sequence blocks and part is backgrounded, and per the PR author it is unclear why. There is no single observable edge that means "fully initialized and servicing," so there is nothing to anchor a readiness assertion on.

## What follows

- `eventually_adp_alive` is the ceiling of what is soundly assertable here: the listeners recover. That is a genuine liveness property because listener-binding IS guaranteed to recover in the fault-quiet window.
- "ADP is servicing load under boot-time Core↔ADP faults" is not soundly assertable with the current architecture. Not for want of a scenario — the architecture provides no sound observable, and the fault model makes servicing conditional rather than invariant.
- This does not say the background-retry fix is wrong. It says Antithesis cannot adjudicate the liveness question the reviewer raised. The block-then-crash alternative has the advantage that it removes the ambiguous half-alive state entirely, which is the state we cannot assert against.

## What would change this

Any of these would create a sound assertion, at a cost:
- A single observable "fully initialized and servicing" edge in ADP (collapse the split blocking/backgrounded boot into one readiness gate), giving a real edge to anchor an `always`-recovers assertion on.
- A servicing signal that is invariant under the fault model — e.g. an ADP self-report that ingest→aggregate→encode advanced within a window, read SUT-side, independent of whether the forwarder reached a sink. This asserts internal forward progress without depending on load actually leaving, sidestepping the shed-vs-wedge ambiguity. Worth scoping if the liveness question must be answered.
