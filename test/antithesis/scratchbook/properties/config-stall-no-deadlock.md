---
slug: config-stall-no-deadlock
title: Config-stream stall does not deadlock or busy-loop startup
family: Lifecycle Transitions & Configuration
type: Liveness
priority: High
status: assertion-missing
sut_commit: 042f41db3bd97118c38981765fd49696fce9d318
---

# config-stall-no-deadlock

## Origin

SUT analysis §2 (control plane: "Startup **blocks** on `dynamic_config.ready().await` for the
first config (run.rs:119-121, *no timeout shown*). Stream end → reconnect after fixed 5s") and §7
#13 ("Core Agent reachability assumed at startup: ADP blocks indefinitely on
`dynamic_config.ready()` with no visible timeout — if the Agent never sends config, ADP never
starts the pipeline").

## Files / functions / lines

- `bin/agent-data-plane/src/cli/run.rs:119-121`:
  ```rust
  info!("Waiting for initial configuration from Datadog Agent...");
  dynamic_config.ready().await;
  info!("Initial configuration received.");
  ```
  This is on the `use_new_config_stream_endpoint` path (run.rs:107). It is reached only after
  `RemoteAgentBootstrap::from_configuration` (run.rs:96-104) — which itself **blocks on the initial
  registration** (`remote_agent.rs:97-105`, `init_reg_rx.await`).
- `lib/saluki-config/src/lib.rs:687-704` — `GenericConfiguration::ready()`: awaits a `oneshot`
  receiver (`ready_signal`). **No timeout.** If the oneshot never fires it awaits forever. If the
  sender is dropped, `ready_rx.await` returns `Err` and `ready()` logs an error and **returns**
  (so a dropped channel unblocks startup; an idle-but-open channel does not).
- `lib/saluki-config/src/lib.rs:541-584` — `run_dynamic_config_updater`: the oneshot
  `ready_sender.send(())` fires (lib.rs:581) **only after the first `ConfigUpdate::Snapshot`** is
  received and the figment is rebuilt. If `receiver.recv()` returns `None` before any snapshot
  (channel closed), the task returns *without* sending ready (lib.rs:546-552) → `ready()` sees the
  sender dropped → returns with an error log (does NOT hang). If the channel stays open but no
  snapshot ever arrives, the task awaits at lib.rs:546 forever and `ready()` hangs forever.
- `bin/agent-data-plane/src/internal/remote_agent.rs:251-304` — `run_config_stream_event_loop`:
  - `:260` waits for a session ID (`session_id.wait_for_update().await`).
  - `:262-263` opens `stream_config_events` and drains it.
  - On stream error (`:295-298`): logs `error!` and **continues the inner while-loop** — this can
    spin if the stream yields a steady error item without ending (see Open Questions).
  - On stream end (outer loop falls through to `:301-302`): `debug!("Config stream ended, retrying
    in 5 seconds…"); tokio::time::sleep(Duration::from_secs(5)).await;` — fixed 5s reconnect
    backoff, then loops back to `:255`.
- `remote_agent.rs:139-148` — `create_config_stream` builds the `mpsc::channel(100)` whose receiver
  feeds `run_dynamic_config_updater`. The config-stream event loop is the **sender** side; it only
  drops the sender by `return`ing (e.g. `:289-292` when the dynamic config channel is closed).

## Investigation: IS there a timeout?

**No.** Confirmed by reading `GenericConfiguration::ready()` (lib.rs:694-704): a bare
`ready_rx.await` with no `tokio::time::timeout` wrapper, and `run.rs:120` calls it bare. There is
also no timeout on `init_reg_rx.await` in `RemoteAgentBootstrap::from_configuration`
(remote_agent.rs:97). The registration *retries* in the background (registration loop
`remote_agent.rs:185-249`, `DEFAULT_REFRESH_INTERVAL=30s`, `REFRESH_FAILED_RETRY_INTERVAL=5s`), and
the first registration result is forwarded to `init_reg_rx` (success or error) — so the **bootstrap
registration** does resolve (Ok or Err) on the first attempt. But the subsequent **config-stream
`ready()`** has no timeout: if the Core Agent registers ADP but never streams a snapshot, ADP hangs
at run.rs:120 indefinitely, logging only the single "Waiting for initial configuration…" line.

## Honest framing of the property

This is a **liveness** property with two acceptable outcomes (not a crash, not a busy-loop):
1. **Progress:** ADP eventually receives the first snapshot and logs "Initial configuration
   received." → proceeds to build topology.
2. **Bounded waiting:** ADP remains observably blocked at "Waiting for initial configuration from
   Datadog Agent…" — a *quiescent* await (parked on a oneshot / parked in `sleep`), **not** burning
   CPU and **not** panicking.

The property to assert is therefore: **the config stall never produces a crash, panic, or
busy-loop; ADP is either making progress or quiescently waiting.** It is NOT correct to assert
"ADP always eventually starts" — with no timeout and a permanently-silent Agent, it legitimately
never starts. Document that the *absence of a timeout* is the design as-is (matches s6-supervised
container model where the operator/Agent presence is assumed).

## Failure scenarios (Antithesis angle)

- **Drop the config snapshot:** Core Agent registers ADP but the config stream never sends a
  `Snapshot` (or sends only `Partial`). Expect: quiescent block at run.rs:120; CPU near zero; no
  panic. Falsify on busy-loop (high CPU while "waiting").
- **Flap the stream:** stream repeatedly opens then ends (EOF). Expect: reconnect every 5s
  (remote_agent.rs:302), no tighter spin. Sometimes(reconnect-after-5s path taken).
- **Steady stream error (no EOF):** stream yields `Err` items continuously
  (remote_agent.rs:295-298 `continue`s the inner loop without backoff). **Potential busy-loop /
  log-flood.** This is the highest-value falsification target — assert CPU/iteration rate bounded.
- **Close the dynamic-config channel mid-startup:** sender drop → `ready()` returns with error log,
  startup proceeds (or downstream fails). Verify no hang and no panic.
- Network partition between ADP and Core Agent during/after registration.

## Config dependencies

- `use_new_config_stream_endpoint` (run.rs:93) — gates whether `ready()` is awaited at all. If
  false (legacy `remote_agent_enabled` only), the `(bootstrap_config, bootstrap_dp_config)` branch
  (run.rs:149) is taken and there is **no `ready()` wait** → property N/A.
- `standalone_mode` (run.rs:91): standalone skips remote-agent bootstrap entirely → no config stall.
- `secure_api_listen_address` (remote_agent.rs:75) — needed for registration.

## Assertion (MISSING — net-new instrumentation)

No Antithesis SDK assertions exist. Proposed:
- Wrap the conceptual "waiting for config" region with a `Sometimes("config wait was entered")`
  reachability marker just before run.rs:120, and a `Reachable("initial configuration received")`
  just after run.rs:121 — so the workload can distinguish "stalled" vs "progressed".
- The busy-loop hazard (remote_agent.rs:295-298) is best caught **workload-side**: monitor CPU /
  log-line rate while the config stream errors; assert bounded. No clean in-process assertion.
- `Always("no panic in config path")` is implicit (panic = crash = Antithesis catches it); not a
  bespoke assertion.

## SUT-side instrumentation needs

- Antithesis SDK dependency (none today).
- Reachability markers around run.rs:119-121 to separate "entered wait" from "config received".
- Workload-side CPU/log-rate monitor for the busy-loop hazard.

### Investigation Log

#### Steady stream error: busy-loop or backoff? + `init_reg_rx` boundedness (2026-05-28)

**Examined:**
- `bin/agent-data-plane/src/internal/remote_agent.rs:251-304` (`run_config_stream_event_loop`),
  `:185-249` (`run_remote_agent_registration_loop`), `:162-183` (`RemoteAgentState::new`).
- `lib/datadog-agent-commons/src/ipc/client/mod.rs:202-224` (`stream_config_events`) and
  `lib/datadog-agent-commons/src/ipc/client/streaming.rs:53-93` (`StreamingResponse::poll_next`,
  the stream type ADP iterates) plus its regression tests `:105-133`.
- tonic 0.14.6 `Streaming::poll_next` at
  `~/.cargo/registry/.../tonic-0.14.6/src/codec/decode.rs:399-421`.
- `lib/datadog-agent-commons/src/ipc/session.rs:67-103` (`SessionIdHandle`).

**Found — busy-loop question RESOLVED, NOT a bug:**
- The stream ADP drains is `StreamingResponse<ConfigEvent>`, which wraps either an `Initial`
  RPC-establishment future or a tonic `Streaming<T>`, plus a `Terminated` state (streaming.rs:11-23).
- An **initial** RPC error (connection refused, RPC rejected, session invalid) → `Outcome::Terminate`
  → the stream fuses to `Terminated` and yields `Some(Err(status))` exactly **once**, then `None`
  forever (streaming.rs:70-72, 86-89). This is the dominant error mode for a steadily-failing stream
  (the RPC never establishes), and it terminates immediately → outer loop hits the **5s sleep**
  (remote_agent.rs:301-302). Confirmed by the test
  `streaming_response_terminates_after_initial_error` (streaming.rs:105-122).
- A **mid-stream** error from an already-established `Streaming<T>`: `StreamingResponse` yields the
  `Some(Err(_))` (does NOT itself terminate, streaming.rs:74-77), but the underlying tonic
  `Streaming::poll_next` (decode.rs:399-421) yields the error **once** then transitions to
  `State::Error(None)` so the very next poll returns `Poll::Ready(None)` (decode.rs:403-408,
  `status.take()` empties the Option). The explicit comment at decode.rs:403-405 confirms: "yield
  that error once and then on subsequent calls return Poll::Ready(None)".
- Net: in BOTH error modes the inner `while let Some(result) = stream.next().await` loop
  (remote_agent.rs:263) sees at most one `Err`, ADP logs one `error!` (remote_agent.rs:295-298) and
  `continue`s, then the next `.next()` yields `None` → inner loop exits → the **5s sleep**
  (remote_agent.rs:302) runs before reconnect. There is NO unbounded spin and NO reconnect tighter
  than 5s. The `continue` at :297 can iterate at most once per stream instance.
- A residual log/CPU concern only remains if the Core Agent could establish the stream and then emit
  a *steady cadence* of error frames over a long-lived HTTP/2 body without ever closing it — but
  tonic ends the body on the first decode/transport error, so this is not reachable with the standard
  client. The hazard described in the Failure Scenarios ("steady stream error, no EOF") is therefore
  NOT realizable through this stack; downgrade it from "highest-value falsification target" to a
  non-issue. Flap-the-stream (repeated EOF every 5s) remains the realistic shape.

**Found — `init_reg_rx.await` (remote_agent.rs:97) is bounded:**
- `RemoteAgentState::new` always initializes `session_id: SessionIdHandle::empty()`
  (remote_agent.rs:176) and `initial_registration_tx: Some(init_reg_tx)` (:178). The handle is freshly
  created per bootstrap (`RemoteAgentState::new` returns it, :85), so it cannot already hold a
  session ID.
- The registration loop's first `loop_timer.tick()` (remote_agent.rs:192) fires immediately (tokio
  interval first tick is immediate). With an empty `session_id`, `state.session_id.get()` returns
  `None` (session.rs:84-90) → the loop takes the `None` register branch (remote_agent.rs:206-246), NOT
  the refresh branch. Both Ok and Err arms send on `initial_registration_tx.take()`
  (remote_agent.rs:233-235 and :241-243). So the first result (success or failure) is always
  delivered → `init_reg_rx.await` resolves on the first attempt.
- The "session_id already Some on first tick → refresh branch, never sends" path is **impossible**:
  the only writer of a non-None session ID is the register branch itself (`:230`), which has not yet
  run on the first tick. Confirmed no path leaves `initial_registration_tx` unsent.

**Not found:** No metric or counter for stream-reconnect cadence; the only signal is the
`debug!`/`error!` logs at remote_agent.rs:296 and :301. No per-iteration throttle beyond the
terminate-then-5s-sleep structure (none needed given the termination semantics above).

**Conclusion:** The busy-loop concern is resolved — a steadily-erroring config stream cannot
spin: the stream terminates after one error and the loop backs off 5s. The `init_reg_rx.await`
bootstrap wait is bounded (always resolves Ok/Err on the first registration attempt). The remaining
true liveness gap is unchanged and is the *snapshot stall* (`ready()` at run.rs:120 has no timeout):
a Core Agent that registers ADP but never streams a `Snapshot` leaves ADP quiescently blocked
forever. Property framing should drop the "steady stream error → busy-loop" falsification target as
unreachable through tonic, and keep the "drop the snapshot → quiescent (low-CPU) indefinite block,
no panic" assertion as the load-bearing one.
