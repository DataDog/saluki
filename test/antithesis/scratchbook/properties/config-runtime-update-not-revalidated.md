# config-runtime-update-not-revalidated

## Origin

Surfaced during the open-question investigation of `config-incompatible-refuses-start`. ADP's
incompatibility gate (`check_and_warn_config` + `ConfigClassifier`) protects startup, but the
**dynamic config stream** from the Core Agent can deliver partial/snapshot updates at runtime that
are never re-classified. A config key that becomes high-severity-incompatible *after* startup is
applied silently.

## Code paths

- `bin/agent-data-plane/src/cli/run.rs:157` — `check_and_warn_config(&config)` runs exactly once,
  before `create_topology` / `build` / `spawn`. Its `Err` aborts startup (`exit(1)`).
- `ConfigClassifier` / `check_and_warn_config` are referenced **only** in `run.rs` — there is no
  re-validation hook on the dynamic-config update path.
- The dynamic config updater (`saluki-config` `lib.rs` ~541-651) applies runtime `Partial`/`Snapshot`
  updates with no classifier check.
- The config stream itself: `bin/agent-data-plane/src/internal/remote_agent.rs` (config event loop)
  pushes `ConfigUpdate::Snapshot/Partial` into the dynamic configuration.

## Failure scenario

The Core Agent pushes a config update (e.g. enabling a feature ADP classifies as
`Incompatible(Severity::High)`) over the AgentSecure config stream while ADP is running. ADP applies
it and **keeps running** in a configuration it would have refused to start with — risking wrong
aggregates or silent data corruption, exactly the outcome the startup gate exists to prevent.

## Property

- **Type:** Safety (Reachability / scope gap)
- **Invariant:** Either `Unreachable("pipeline running with high-severity incompatible non-default
  key after a runtime config update")`, or — if the intended design is "startup-only gating" — a
  `Reachable` marker proving the unguarded runtime-apply path is taken, documenting the gap.
- **Antithesis angle:** Start ADP with a clean config (passes the gate), then inject a config-stream
  update carrying a high-severity-incompatible non-default key; observe whether ADP detects/refuses
  or silently applies it. This exercises the control-plane → data-plane config path the diff-test
  never touches.
- **Priority:** Medium (depends on whether high-severity keys are reachable via the stream in
  practice — a product question).

## Open Questions

- Is this an intentional design choice (startup-only gating, runtime updates trusted because they
  come from the authoritative Core Agent) or an oversight? `(needs human input)`
- Can a `Severity::High` key actually be delivered over the config stream, or does the Core Agent
  pre-filter what it sends to remote agents? Determines real-world reachability.

### Investigation Log

#### Are runtime partial config updates re-validated by the incompatibility gate?

- Examined: `bin/agent-data-plane/src/cli/run.rs:157,331-381` (gate + caller), grep for
  `check_and_warn_config` / `ConfigClassifier` across the tree, `saluki-config` dynamic updater
  (`lib.rs` ~541-651), `internal/remote_agent.rs` config event loop.
- Found: the gate runs exactly once at startup; the classifier is referenced only in `run.rs`; the
  runtime update path applies `Partial`/`Snapshot` updates with no classifier call.
- Conclusion: confirmed — a runtime config update can introduce a high-severity-incompatible key
  with no re-gate, and ADP keeps running. Filed as this standalone property. The remaining questions
  (intentional vs. oversight; stream reachability of high-severity keys) need the team's input.
