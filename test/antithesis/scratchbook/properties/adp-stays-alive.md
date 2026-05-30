---
slug: adp-stays-alive
title: ADP boots and stays serving (self-inflicted-crash liveness)
type: Liveness
priority: High
sut_path: /home/ssm-user/src/saluki
commit: 2e4ae1b8be45143882f0dbeb5e74998021c5faf9
updated: 2026-05-31
status: LANDED as the `eventually_adp_alive` test command
---

# adp-stays-alive — ADP boots and stays serving

## Property (one sentence)
After the per-replay `datadog.yaml` is applied and the workload runs, ADP's unprivileged API
(`:5100`) is reachable within a faults-paused window; if it never comes up, ADP died of its own
config or load — not of an injected node fault.

## Origin
The harness now generates per-replay configs (`datadog-yaml-config-gen`) and adversarial load whose
boundary values *should* sometimes crash ADP — e.g. an oversized `dogstatsd_string_interner_size`
panics at boot (`capacity would overflow isize::MAX`). But nothing demonstrated ADP is alive: the
only ADP-side assertion was a `reachable` bootstrap probe, which fires on success and is satisfied as
long as *some* branch boots — it cannot flag the branch where ADP died. A process that panics also
cannot self-assert its own liveness after the fact. So the catch must be an external observer.

> **Provenance clarification (the misunderstanding worth recording).** That interner boot-panic was
> first seen only via local **`snouty validate`** — the single-config smoke run at launch time,
> *outside* any Antithesis timeline. That is **not** the same as an Antithesis shot finding the crash:
> `validate` exercises one static `datadog.yaml`, whereas a run draws the whole config space across
> timelines. Pre-`eventually_adp_alive`, **no in-run mechanism** would have turned such a boot/load
> crash into a counterexample. This property is exactly that missing in-run catch; do not conflate
> "validate rejected a config locally" with "a shot found the bug."

## The fault-gating mechanism (the crux)
The requirement is: trigger on self-inflicted death (panic on startup, crash from load) but **not**
on death caused by injected node faults (kill/pause/stop/throttle/clock). A quiet period separates
the two:
- `eventually_` and `finally_` test commands run with **faults already paused**; `ANTITHESIS_STOP_FAULTS`
  gives the same mid-run.
- During a quiet period a **fault-killed** container is restored by the platform, so fault-induced
  down recovers → the liveness check passes.
- A **self-inflicted** crash is config/load-driven and deterministic: it crash-loops or stays dead
  even with faults paused → `:5100` never binds → the check fails.

## Observation points considered
- **`:5100` API reachability (chosen):** external TCP check; survives the crash; the existing
  `deploy/workload/entrypoint.sh` already polls it. Clean and direct.
- **End-to-end intake delivery:** stronger (alive *and* functional) — split into
  [`adp-keeps-delivering`](adp-keeps-delivering.md).
- **Container-exit / built-in crash detection:** opaque; runs show it not firing for our boot panic
  (open question); and it would also trip on fault-kills (not gated). Rejected as the primary.
- **In-SUT assertion:** rejected *for the death case* — a panicking process can't report its own
  death, so liveness-on-crash must be observed externally. (Note: `saluki-components` *has* since
  gained an `antithesis` feature + SDK dep for the **good-function** anchor in
  [`adp-keeps-delivering`](adp-keeps-delivering.md); that proves a *booted* ADP works, which is a
  different question from detecting a *dead* one.)

## Why this image makes API liveness valid (vs. catalog note R1)
Note R1 says container/API liveness is vacuously green because the **production** ADP image runs an
s6 supervisor that auto-restarts ADP. The **harness** adp image is different: `deploy/Dockerfile`
adp stage is a bare binary + boot wrapper (`ENTRYPOINT ["/entrypoint.sh"]` → `agent-data-plane
run`), no supervisor. So a crash is not silently restarted, and a deterministic config/load crash
leaves `:5100` permanently unbound — API liveness is a real signal here. If the harness ever adopts
an auto-restart image, this property must move to a restart-count assertion (per R1).

## Implementation (landed)
Realized as the `eventually_adp_alive` test command
(`test/antithesis/harness/src/bin/eventually_adp_alive.rs`). In the faults-paused `eventually_`
window it polls **both** ADP's `:5100` API (`TcpStream::connect`) and the DogStatsD listener socket
(`/var/run/datadog/dsd.socket` exists) for up to ~60×1s, then
`assert_always!(api_reachable && socket_present, …)` with the addresses in the details. Checking the
socket as well as `:5100` is slightly stronger than the original sketch: it confirms ADP got far
enough through bootstrap to *accept metrics*, not just to bind its control API. The assertion fires
once per branch; a branch where ADP self-crashed never satisfies both → counterexample. The workload
container intentionally gates on adp `service_started` (not `service_healthy`) so this command still
runs — and can observe a dead ADP — when ADP never becomes healthy.

## Assertion-type rationale
**Liveness**, realized as an `assert_always!` *inside a faults-paused command after a bounded
recovery poll* — within that command ADP must be up, so a single always-evaluation is the right fit;
the quiet-period prefix supplies the fault discrimination rather than the assertion type.

## Open Questions
- Does Antithesis's built-in container-exit detection already observe ADP boot-panics in this
  topology? Runs show it reporting nothing — confirm via an `antithesis-query-logs` search for the
  adp exit / the `isize::MAX` panic. `(needs human input)`
- Does a deterministic boot crash actually crash-loop, or exit once and stay down, under the harness
  compose (no `restart:` policy on the adp service)? Either way `:5100` stays down; confirm.
- Mid-run crash coverage (workload masks a transient crash) needs an `ANTITHESIS_STOP_FAULTS`
  liveness loop — deferred to a follow-up.

## Investigation Log
- 2026-05-31: **Landed** as `eventually_adp_alive` (poll `:5100` + DSD socket, faults-paused
  `eventually_`, `assert_always!`). Decoupled the workload from adp health (`service_started`) and
  made the workload entrypoint non-gating so the check runs even when ADP is down. `snouty validate`
  registers it ("1 eventually script"). Clarified detection provenance in Origin: the interner
  boot-panic was a `snouty validate` finding, not an in-run one — this command is the in-run catch.
- 2026-05-31: Confirmed harness adp image is bare (no s6) from `deploy/Dockerfile` adp stage
  (`ENTRYPOINT ["/entrypoint.sh"]`, `CMD ["run"]`) — so API liveness is non-vacuous here, reconciling
  with catalog R1 which describes the production s6 image.
- 2026-05-31: Reviewed Antithesis fault model: `eventually_`/`finally_` run faults-paused;
  `ANTITHESIS_STOP_FAULTS` for mid-run quiet periods; killed containers are restored during the
  quiet period — basis for the self-inflicted-vs-fault discrimination above.
