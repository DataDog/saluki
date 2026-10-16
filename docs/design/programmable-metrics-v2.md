# Programmable metric processing: version 2 simplifications

What we would build differently now that the premise has changed, and the order in which to remove what the old
premise needed. Read the [index](programmable-metrics-index.md) and the
[direction](programmable-metrics-next.md#direction-programs-replace-native-policy) first.

## Contents

- [The premise, then and now](#the-premise-then-and-now)
- [Two modes, not coexistence](#two-modes-not-coexistence)
- [What stays](#what-stays)
- [The simplifications, in order](#the-simplifications-in-order)
- [How each step is validated](#how-each-step-is-validated)
- [Progress](#progress)
- [Handing off](#handing-off)

## The premise, then and now

The specification and stages 1 to 9 assumed that **programmability is added inside a native system that stays the
authority**. That premise needs machinery for coexistence: programs splice into a native default, native configuration
keeps authority over parts of a program, the physical layout is shaped by native components, and native identities are
preserved throughout. Two instances of it have already been removed: aggregation as a native component in the middle
(it is a kernel, and only I/O still needs a task of its own, [N2 and
N3](programmable-metrics-next.md#n2-and-n3-one-region-from-decode-to-encode-with-an-interleaved-flush)), and policy
interacting live with native features of the same kind ([detached
programs](programmable-metrics-detached-programs.md)).

The premise now: **native configuration is a front end, and the program is the system.** Native configuration lowers to
a program the way a legacy language compiles to the same intermediate representation; after that, the program stands
alone.

## Two modes, not coexistence

The program-driven path will run beside ADP's existing native implementation for some time. That is two **modes**,
chosen at boot, not two systems sharing authority inside one process:

- **Compatibility still matters.** In program mode, the default program reproduces native behavior: the same metrics
  delivered, the same telemetry the Core Agent reads, the same configuration accepted. Every intentional difference is
  listed under the status document's changes users can see.
- **Coexistence does not.** Nothing in program mode exists only so that a program can share control with a native
  component, or so that a native component can keep authority over part of a program.
- **Differential testing between the modes must stay possible.** The same configuration and the same input, through
  native mode and through program mode, must give the same output. Today the pieces are the native path kept as a test
  reference (`create_legacy_topology`, under `cfg(test)` in `bin/agent-data-plane/src/cli/run.rs`), the frozen copies
  of native code the kernels are checked against, the compatibility matrix, and the measurement harness's native
  baseline. A harness that runs both modes on the same recorded traffic, and a boot-time mode switch if native mode must
  ship from this code, are future work; nothing below may make either harder.

## What stays

The substrate: the IR with effect contracts, the validator, the reference interpreter and the equivalence oracle, the
kernels, generations with semantic diffs and kernel reuse, exact cuts at bucket boundaries, the defunctionalized
delivery host, the syntax, and the editor. None of the simplifications changes what a program means.

## The simplifications, in order

Each step is a deletion, or a deletion with a small replacement, done on this branch with the oracle running. Each one
leaves the demo the same shape, ideally simpler.

| Step | What goes | What replaces it | Depends on |
|---|---|---|---|
| **V1. Opaque hosts** | The `opaque_host` operator, `trusted_host`, `authorable`, and the plan-crate fixtures with opaque gateways (stage 3's gateways, used only by tests) | Nothing | |
| **V2. Resources artifacts** | The `resources` product, declared recipients and outputs, credential references for declared recipients, resource provenance, the resources stub routes | Endpoints come from configuration, which ADP now applies live; the demo already works this way | |
| **V3. Native configuration lowers to literals** | Live policy bindings, keyed bindings (`metrics_endpoint_routing.group.<key>.*`), the binding classes for policy, the detached-export conversion, comparing a value with a binding | A native policy change lowers a new default program, which installs as a precise generation; bindings remain for facts and signals | |
| **V4. Programs are whole** | Scopes as replaceable parts, `base` and staleness, output fingerprints, correspondence and splicing (`ScopeRegion`, exits, `in_default_order`), partial-scope support in the plan crate | An artifact is a complete program; composition resolves names and elaborates | V3 |
| **V5. One program document** | Several configs and scopes per snapshot, configs moving between scopes, the cross-scope revision mark | One program document with a revision and an acknowledgment | V2, V4 |
| **V6. Elaboration as a phase** | Resolution and inference scattered across composition and export; two hashes | One elaboration pass from the program as written to the core program, which explain shows and which comparison, hashing, and adoption use | V4 |
| **V7. One region from decode to encode** | The `dsd_agg` isolation slot as a separate task, R1's cost model, lanes by default | [N2 and N3](programmable-metrics-next.md#n2-and-n3-one-region-from-decode-to-encode-with-an-interleaved-flush): state steps in fused regions, an interleaved flush, lanes opt-in | |
| **V8. A trivial frame** | Placement against a native-shaped frame, idle pre-provisioned exits, "needs frame changes" errors, encode hosts with leases | Sources, one region, delivery hosts; encoders as terminal kernels | V7 |
| **V9. A smaller cut** | Cut arming across several admission points with lock timeouts and retries | One admission point for state; the cut stays exact | V7, lanes decided |
| **V10. Telemetry at the edge** | Reserved node IDs, telemetry profiles threaded through plans, "the aggregation is `dsd_agg`" | One adapter that maps program telemetry to the series the Core Agent reads | V6 |

V1 and V2 are contained and come first. V3 to V6 are the core of the work: together they turn composition into
elaboration of a whole program. V7 to V9 are the runtime half and follow the N1 measurements. V10 can come at any point
after V6.

## How each step is validated

Targeted, so that each step is quick to check:

- The crates a step touches, with focused test filters while iterating, and their full suites once at the end of the
  step.
- ADP's `programmable_metrics` tests for anything ADP-facing, including the lowering matrix and the differential tests
  against the native path.
- The release property tests only for steps that touch composition, installation, or the interpreter.
- `make demo-programmable-metrics-check` once per step, at the end, after formatting.
- clippy and formatting on the changed crates; vale on changed documents.

## Progress

**Left by V2, for later:** the delivery host in `saluki-components` still has a declared recipient origin
(`RecipientOrigin::Declared`), bounded delivery policies, and conflict detection between recipients of a declared
origin, all general capabilities with their own tests that ADP no longer uses. Remove them, or reuse the bounded policy
for N5's shedding.

| Step | State | Commit |
|---|---|---|
| V1 | not started (V2 went first: opaque hosts carry about 50 tests of the frame and the interpreter's re-admission, which V7 and V8 rework anyway) | |
| V2 | done: amendment 192 | `06e7301501` |
| V3 | not started | |
| V4 | not started | |
| V5 | not started | |
| V6 | not started | |
| V7 | not started | |
| V8 | not started | |
| V9 | not started | |
| V10 | not started | |

## Handing off

How a new session continues this work. The state is the branch's last commit; the working tree is clean apart from
`.claude/launch.json`, which is not committed and stays so.

### Working rules from the owner

- Work in the foreground, where the owner can follow it; hand work to other agents only when it runs in parallel and
  cannot conflict.
- Commit locally with `git commit --no-gpg-sign`, one commit per step (code), then one for documents if needed. Do not
  push unless the owner asks for that push.
- Do not edit the [specification](programmable-metrics.md); propose changes as numbered amendments. The next amendment
  is **193**: add it to the [stages 7 and 9
  amendments](programmable-metrics-stages-7-and-9.md#10-proposed-spec-amendments) and to the [status amendments
  index](programmable-metrics-status.md#spec-amendments-index).
- Do not touch `architecture-experiment/` or `.claude/`, and add no CI jobs.
- Run no UDP benchmarks or load tests apart from `make demo-programmable-metrics-check`. Give interactive demo runs a
  directory of their own under `target/programmable-metrics-demo/` (macOS cleans `/tmp`).
- Keep the demo the same shape, ideally simpler as concepts go. Keep program mode compatible with native mode and
  differentially testable ([two modes](#two-modes-not-coexistence)).
- For a Saluki-only configuration key, follow the repository's configuration-system workflow (source model, seed,
  model destination, `SALUKI_KEYS`, the generated reference); format with `make fmt`, not `cargo fmt`.

### Validation, and what it costs

Targeted while iterating, full suites once per step ([how each step is validated](#how-each-step-is-validated)). On
this Mac, as of V2:

| Command | Tests | Time |
|---|---|---|
| `cargo nextest run -p saluki-metric-plan -E 'not test(/property_test_/)'` | 904 | about 1 minute |
| `cargo nextest run -p saluki-metric-plan --release -E 'test(/property_test_/)'` | 50 | a few minutes, with the release build |
| `cargo nextest run -p agent-data-plane -E 'test(/programmable_metrics::/)'` | 323 | the slowest suite; filter while iterating |
| `cargo nextest run -p agent-data-plane-config -p agent-data-plane-config-system` | 250 | quick |
| `cargo nextest run -p programmable-metrics-demo` | 146 | quick |
| `make demo-programmable-metrics-check` | two launches | about 7.5 minutes with the build (154 s and 281 s) |

Then `cargo clippy` with `-D warnings` on the changed crates, `cargo check --workspace --tests`, `make check-fmt`, and
`vale --minAlertLevel=error` on changed documents. Golden files: `SALUKI_METRIC_PLAN_UPDATE_EXPLAIN=1` and
`SALUKI_METRIC_PLAN_UPDATE_SCHEMA=1` rewrite them; review the diff. More commands are in
[status: focused tests and golden files](programmable-metrics-status.md#focused-tests-and-golden-files).

### Documents each step updates

The progress table above, with the commit; an amendment if the step changes what the specification says; the status
document's [changes users can see](programmable-metrics-status.md#changes-users-can-see) for anything a user notices;
and a section of the [log](programmable-metrics-log.md) saying what went, what replaced it, how the tests moved, and
the results.

### The next step: V3

**Native configuration lowers to literals.** Today the default program reads native policy settings (filterlists,
endpoint routing, failover) through live policy bindings, so a Remote Config change reaches the running program with no
new generation, and the detached export converts those bindings into literals. After V3 the default program holds the
literals itself; a native policy change lowers a new default program, which installs as a generation. Bindings remain
for facts and signals only.

The one behavior to keep is **precise updates**: a filterlist change must give a generation that rebuilds only the
kernels whose settings changed, with no cut and no rebuild where nothing changed. Prove that with a test before
removing the live path; the installer's semantic diff and kernel reuse should already give it.

Where to start:

- `bin/agent-data-plane/src/programmable_metrics/bindings.rs`: the policy bindings (`BindingClass::Policy`) and the
  keyed bindings per routing group (`metrics_endpoint_routing.group.<key>.*`).
- `bin/agent-data-plane/src/programmable_metrics/lowering/mod.rs`: `lower` and `lower_after`, which would write the
  values; and the code that reacts to a native configuration change, which would lower again and install.
- `lib/saluki-metric-plan/src/export.rs` and `export/eliminate.rs`: the detached conversion, which should shrink to
  nothing once the default program is already detached.
- `lib/saluki-metric-plan/src/compose.rs` (refusing policy bindings in an artifact), `validate/catalog.rs`
  (`detached_programs`, the binding classes), `validate/references.rs` (`LiveBindingOnHost`), and the keyed bindings
  in `testing/fixtures.rs`.
- The demo: its default program and editor should show the same program, with nothing to convert.

### Also deferred by the owner

- V1 (opaque hosts), until V7 and V8 rework the frame tests that use them.
- A scripted video of the demo (a tour mode in the page, recorded with Playwright).
