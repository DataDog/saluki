# Programmable metric processing: version 2 simplifications

What we would build differently now that the premise has changed, and the order in which to remove what the old
premise needed. Read the [index](programmable-metrics-index.md) and the
[direction](programmable-metrics-next.md#direction-programs-replace-native-policy) first.

## Contents

- [The premise, then and now](#the-premise-then-and-now)
- [Two modes, not coexistence](#two-modes-not-coexistence)
- [What stays](#what-stays)
- [The simplifications, in order](#the-simplifications-in-order)
- [Many aggregations](#many-aggregations)
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
| **V7a. State owners in any region** | The isolation slot, the `isolation` check in `runtime/code.rs`, "a state owner outside the boot plan's isolation slots needs a frame change" | Each state owner in a region of its own, made from the program when a generation installs; ticks, watermark, drains, and cuts driven for any region that holds one, keyed by node ([state owners in any region](#v7a-state-owners-in-any-region)) | V10 |
| **V7b. One region from decode to encode** | R1's cost model, lanes by default, a task per state owner | [N2 and N3](programmable-metrics-next.md#n2-and-n3-one-region-from-decode-to-encode-with-an-interleaved-flush): state steps fused with stateless work, an interleaved flush, lanes opt-in | V7a, N1 |
| **V8. A trivial frame** | Placement against a native-shaped frame, idle pre-provisioned exits, "needs frame changes" errors, encode hosts with leases | Sources, one region, delivery hosts; encoders as terminal kernels | V7b |
| **V9. A smaller cut** | Cut arming across several admission points with lock timeouts and retries | One admission point for state; the cut stays exact | V7b, lanes decided |
| **V10. Names and a compatibility analysis** | Reserved node IDs and the native namespace, telemetry profiles keyed by reserved node, amendment 182's adoption of `dsd_agg`, the `single_aggregation` rule (amendment 169) | A node's telemetry identity is its name; operators describe what they report; the remapper's rules stay at the edge and are the contract an analysis checks a program against, with warnings ([telemetry contracts as an analysis](#v10-telemetry-contracts-as-an-analysis)) | V6 |

V1 and V2 are contained and come first. V3 to V6 are the core of the work: together they turn composition into
elaboration of a whole program. V10 follows V6 and comes before the runtime half, so that no runtime step has to carry
native identities. V7a is the semantic half of the runtime work and does not wait for N1; V7b, V8, and V9 are its
performance half and follow the N1 measurements. Amendment 195 records V10 and V7a; [many
aggregations](#many-aggregations) is their design.

## Many aggregations

ADP runs at most one aggregation per program. That is not a limit of the runtime, which already runs several state
owners and whose tests exercise them (transition traces 12 and 13, budgets shared by owners, output stamps per lane). It
is native identity built into the core model, on top of a physical frame shaped like the native topology. V10 and V7a
remove both, so that a program may aggregate wherever and as often as it means to.

### Where the limit is

From the most superficial to the most basic:

1. **A validation rule.** `single_aggregation` (amendment 169, `validate/state.rs`), which ADP's catalog turns on.
   Placement alone accepts a second `aggregate` on the timestamp split's passthrough branch, in the one isolation slot.
2. **Identity by reserved ID.** A reserved node, whose ID derives from a native component's, reports under that
   component's `component_id` and keeps its native metric names
   ([telemetry profiles](../../bin/agent-data-plane/src/programmable_metrics/telemetry_profiles.rs)); a node in the
   policy namespace reports none of its own. The remapper (`state/metrics/rules/aggregation.rs`) turns
   `adp.aggregate_*{component_id:dsd_agg}` into the `aggregator.*` series that the Core Agent's status and telemetry
   read. So that an author's aggregation keeps those series, composition gives it the reserved ID `dsd_agg` whatever it
   is called (amendment 182), which works only when there is exactly one; hence the rule.
3. **The fixed frame.** The frame is fixed at boot and keeps native slot IDs; state owners run only in isolation slots,
   each fed by one source clock's admission points (the status document's invariants 4 to 6). Placement refuses what
   the native layout lacks: an aggregation after enrichment, an aggregation fed by another aggregation (a node a state
   owner feeds runs in a fused slot after the isolation slot), an aggregation over a second source.

What stays, because it is about meaning rather than about native: bucket widths that share a boundary often enough for
an exact cut (`SHARED_WIDTH_LIMIT`), the host's state budget across every state owner, and a watermark for every state
owner.

### V10: telemetry contracts as an analysis

Compatibility with the Core Agent's telemetry is a contract a program may or may not meet. It belongs in an analysis
that says which, not in identities that the core model preserves so that a program cannot fail to meet it.

1. **A node's telemetry identity is its name**, the node's ID in the elaborated program, which `let` gives. Every node
   reports under it, whatever its provenance; there is no native namespace. The default program's lowering already
   writes `let dsd_agg = …`, so it reports as today.
2. **Each operator describes what it reports**: series, kind, tags, and who records each value (the kernel, the
   region host for a `split`'s ports, or the installer from the plan), as telemetry profiles' `MetricSource` does
   today. After V3, what the installer records from bindings (a list's length, a matcher's entries) it records from the
   plan's literals.
3. **The remapper stays**, as a table of rules at the edge, unchanged as data. It still renames series, which names in
   a program cannot do without building the Core Agent's names into kernels. Its rules are also the contract.
4. **The compatibility analysis** is ADP's, beside its catalog; the plan crate knows nothing of the Core Agent. It
   reads the elaborated program, what each node reports, and the remapper's rules as expectations, and gives, for
   each series the Core Agent reads, the node that produces it or why none does. Its findings are warnings in the
   validation report, so the editor shows them at the line and explain and status show them too: "`aggregator.*` will
   be absent: no node is named `dsd_agg`", or "`dsd_agg` is a `filter`, not an `aggregate`;
   `aggregator.dogstatsd_contexts_by_mtype` will be absent". A program that reports more than the contract asks is
   fine.
5. **Which aggregation reports as the aggregator** is the one named `dsd_agg`; no sum and no marker. A program with
   two aggregations names one of them `dsd_agg`, or neither, and is warned.

Rules that select on something other than an aggregation's name are covered the same way: `resolver_id:dsd_in/dsd/primary`
names a source's context resolver, and `dd_events_encode` and `dd_service_checks_encode` stand in for the aggregator for
events and service checks. The default program names those nodes as the rules expect.

**Goes:** reserved node IDs and the native namespace (invariant 6, apart from the remapper's table), telemetry
profiles keyed by reserved node, amendment 182's adoption, and `single_aggregation`. After V10, a second aggregation
places wherever the frame can run it, which is the passthrough branch today.

**Tests:** the default program meets every series in the contract (replacing the profile tests); a program without
`dsd_agg`, or with a `dsd_agg` of the wrong operator, gets the warnings; the differential tests against native mode
compare remapped output, unchanged.

**Visible change:** a node an author adds reports component series under its name, where today it reports none. List
it with the status document's changes users can see when V10 is built.

**Owner question:** with identity by name, renaming an aggregation is a new lineage, a cut that starts its state over.
Amendment 182 retained state across a rename. Accept the cut, or key a lineage by the elaborated program's structure
rather than its name, which V6 would have to define.

### V7a: state owners in any region

The isolation slot's one job is to give a state owner a single owner for its ticks, watermark, drains, and cuts
([N2 and N3](programmable-metrics-next.md#n2-and-n3-one-region-from-decode-to-encode-with-an-interleaved-flush)). That
job moves to whatever region holds a state owner. V7a does only the semantic half: each state owner still gets a region
of its own, so a flush stalls only that region, as it does today; fusing state owners with stateless work and the
interleaved flush are V7b, which waits for N1.

- **Goes:** the isolation slot, the `isolation` check in `runtime/code.rs`, and placement's refusals of state owners
  outside the boot plan's isolation slots.
- **Replaces it:** a region for each state owner, made from the program when a generation installs, rather than taken
  from the boot frame; the region host drives ticks, watermark, and drains for any region that holds a state owner;
  cuts and lineages keyed by node, not by slot ([N2 and N3](programmable-metrics-next.md#n2-and-n3-one-region-from-decode-to-encode-with-an-interleaved-flush),
  item 3).

### Design items

Each needs design before V7a is built:

1. **A watermark over several sources.** A state owner's watermark comes from one source clock today. An aggregation fed
   by two sources needs one from both, such as the smaller of the two, with the clock-reset and idle-source rules
   (`limits.rs`) restated for it.
2. **Chained state owners.** An aggregation fed by another takes its admission time and watermark from the first one's
   time-driven output. A cut through the chain must stay exact: both owners switch at bucket boundaries they share.
3. **Regions made at installation.** Today a frame change is a capability error. A region for a new state owner starts
   with the generation that has it, is retired with the last generation that has it, and is placed where the cut
   protocol arms its admission points.
4. **Lanes for several state owners.** Each owner partitions its own state by key; with lanes opt-in after V7b this can
   wait, but V7a must not make it harder.

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

**Left by V3, for later:** the export keeps two forms, `bound` and `detached`, which now differ only in leaving out
what does nothing and in addressing emits to recipients; V6's elaboration may make one of them the only form.
`program.differs` names default-program nodes; V4 kept it.

**Left by V4 and V5, for later:** node IDs in an artifact are still `policy/<local>` and a node named `dsd_agg` still
adopts the reserved ID, until V10. Scope declarations, `base`, and `scope` are gone (a document that has one is refused
by name), and `exit` stays a reserved word with no use. (The split between an unmodified export's `Plan::hash` and the
default program's is resolved by V6.)

**Left by V6, for later:** elaboration does not leave out what does nothing, so the detached export, which does, is
still a generation of its own that builds no kernel (S0), and `default_program_hash` equals `active_plan_hash` for the
default program and for programs that elaborate to it, such as the unmodified bound export, not for the detached one.
Making the core form leave out what does nothing would change what the running default program reports (telemetry of
nodes that do nothing), so it waits for V10's contract. The validation report is the program's as written, with its
bindings resolved, while compilation and kernels read the core plan; compilation may fold a split a signal decides
(R3) once another rewrite has applied. The export keeps its two forms; `compose` and `Composed` are names for
`elaborate` and `Elaborated`.

| Step | State | Commit |
|---|---|---|
| V1 | not started (V2 went first: opaque hosts carry about 50 tests of the frame and the interpreter's re-admission, which V7 and V8 rework anyway) | |
| V2 | done: amendment 192 | `06e7301501` |
| V3 | done: amendment 196 | `e6599d63f3` |
| V4 | done: amendment 197 | `53cd99b576` |
| V5 | done: amendment 197 | `53cd99b576` |
| V6 | done: amendment 198 | (V6 commit) |
| V7a | not started | |
| V7b | not started | |
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
  is **199**: add it to the [stages 7 and 9
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

### The next step: V10

**Names and a compatibility analysis** ([telemetry contracts as an analysis](#v10-telemetry-contracts-as-an-analysis)).
V10's additive half is committed (`7b1e126d16`: operator reports, the telemetry contract, compatibility roles); the
wiring half (the `dsd_prefix_filter` rename in lowering, the catalog's telemetry contract, telemetry by name, validation
findings, `keeps_identity` in the filter rewrites) runs beside V6. What remains is the deletion half: node IDs are
names, and what only native identity needed goes.

Where to start:

- `lib/saluki-metric-plan/src/ir/id.rs`: `Namespace`, `NodeId::{reserved, policy, namespace, is_reserved}`, and the
  `policy/` prefix go; `NodeId::new(LocalId)` replaces them, serialized as the bare identifier, and `Plan::from_json`
  refuses `policy/x` with a message naming the change. Callers: `normalize.rs`, `export.rs`, `elaborate.rs` (`node_ids`,
  `partial_emit`), `ir/plan.rs`, the syntax crate's `ids.rs`, `compile/rewrite/{harness.rs, filters.rs}`, ADP
  `lowering/mod.rs`, `installer.rs`, and `topology.rs` (which asks the plan's `native_components` instead).
- `lib/saluki-metric-plan/src/elaborate.rs`: amendment 182's adoption (`the_aggregation`, and the `aggregation`
  parameter of `node_ids`) goes; a renamed aggregation is a new lineage, removed and added at a cut.
- `single_aggregation` (amendment 169): `validate/state.rs`, `Rule::SingleAggregation`, `Catalog::single_aggregation`,
  `MemoryCatalog`'s member and `with_single_aggregation`, ADP `catalog.rs`, the plan crate's `testing`, and the demo's
  `export.rs`, `reference.rs`, and `scenario/`. A stale `single_aggregation` key in a catalog document is ignored.
- Naming by namespace: `operators/mod.rs` (`detached`), `compile/frame.rs` (`component_name`), `runtime/bounds.rs`
  (`kernel_accounting_name`, `kernel_accounting_path`), and ADP `telemetry.rs` (`component`): a node reports under its
  ID as a transform and accounts under `<region>.<node>`.
- ADP status gains `telemetry: ContractReport` for the active plan; the `#[allow(dead_code, unused_imports)]` on `mod
  compatibility` goes.
- Tests that invert: `install/program_tests.rs`'s `the_one_aggregation_keeps_the_default_programs_id_and_lineage_...`
  becomes a test that a renamed aggregation starts a new lineage at a cut, and ADP's lifecycle tests likewise; `policy/`
  in the rewrite, explain, normalize, compose, and install tests, and in the demo's `preview/tests.rs` and
  `scenario/destinations.rs`.

V6 left the core plan as V10's input: node IDs are resolved in one place (`elaborate::node_ids`), and kernel keys and
the plan diff read the core plan's parameters directly. Then V7a ([state owners in any
region](#v7a-state-owners-in-any-region)).

### Also deferred by the owner

- V1 (opaque hosts), until V7 and V8 rework the frame tests that use them.
- A scripted video of the demo (a tour mode in the page, recorded with Playwright).
