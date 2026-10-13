# Programmable metric processing: implementation log

The history of the implementation: what was attempted, what did not work and why, what reviews and integration found,
the measured results, and the decisions made along the way. The design as built, the amendments, and the open
decisions live in the documents this log links to; start at the [index](programmable-metrics-index.md).

## Contents

- [Stages 1 to 6](#stages-1-to-6)
- [Stages 7 and 9](#stages-7-and-9)
- [After stage 9: deployment context](#after-stage-9-deployment-context)

## Stages 1 to 6

This half of the log records how stages 1 to 6, the surface syntax, and the demo were built: what each stage tried, what
it dropped and why, what reviews found, what was measured, and what it left open. The design as built, the amendments,
and the risks are in [the implementation design](programmable-metrics-implementation.md); the decisions still waiting on
the owner and the environment notes are in [the status document](programmable-metrics-status.md). The log links to both
rather than repeat them.

| Commit | Time (local) | Content |
|---|---|---|
| `e435160474` | Oct 8, 21:46 | Specification |
| `f11cffe4a1` | Oct 9, 03:44 | Stage 1: semantic core |
| `3e058a556a` | Oct 9, 09:20 | Stage 2: extracted kernels, lowering |
| `a4c72a4f2a`, `d991c4b923` | Oct 9, 09:27 and 10:03 | Surface syntax and demo (first pass); a stack fix for its property tests |
| `f6ef9cddcb` | Oct 9, 18:41 | Stage 3: plan-driven metrics path, live stateless generations |
| `4c64b4c522` | Oct 10, 00:21 | Stage 4: gateways as routing operators, rewrites |
| `26af093620` | Oct 10, 06:00 | Stage 5: artifact channel and lifecycle |
| `b01049372e` | Oct 10, 16:10 | Stage 6: aggregation as a stateful kernel, cuts |

### How the work was organized

Before any code, revision 1 of the implementation design went through a five-lens review. Revision 2 and the stages took
up every blocker and major finding, among them: a physical frame fixed at boot; generations stamped at the admission
point from a shared schedule; the epoch in the batch header; progress and heartbeats on starved branches; routing-group
keys by endpoint digest, and output fingerprints; exits sized to their contents, with no recycling; frozen references in
place of tautological differential tests; the catalog in the export; a loopback-only stub; and a one-to-one reserved-ID
correspondence. Four of its items were still open after stage 6: the Antithesis panic hook, the forwarder dropping
queued requests when its input closes, the native counter back-fill quirks, and the `component_events_sent_total` double
count ([status](programmable-metrics-status.md#decisions-awaiting-the-owner)).

| Stage | Shape | Review |
|---|---|---|
| 1 | Three parts in series (A, B, C) | Four lenses, 36 findings, one fix agent |
| 2 | A and C in parallel, then B, then D | Four lenses, 16 findings (3 major), one fix agent |
| Syntax, demo | One implementer each, on a stage 1 base, beside stage 2 | Two lenses each, then a fix agent (v2) |
| 3 | A and B side by side, then C, D, E in series | 22 findings (2 blockers, one problem found twice), one fix round |
| 4 | Interfaces, then four parts in parallel work trees (implement, review, fix), then integration | Per part |
| 5 | Interfaces and three parts on the stage 3 base while stage 4 ran; integration onto stage 4; three follow-up parts; a merge | Per part, then three lenses (22 findings) |
| 6 | Two competing design drafts, merged and reviewed (33 findings); interfaces in two steps; six parts | Per part, then a final review (24 findings) |

Every part wrote a handoff note and edited the design document in place; the specification was never edited. Reviewers
were read-only and returned findings by severity; the fix agent verified each one before fixing it. In stages 1 and 2
every finding (36 and 16) proved real. The practice is summarized in [How the work was
done](programmable-metrics-status.md#how-the-work-was-done), the gate each stage ran in [The stage
gate](programmable-metrics-status.md#the-stage-gate).

### Stage 1: the semantic core

**Parts.** A created `saluki-metric-kernels` (moving `intake_name` and the sorted-set matcher out of ADP),
`saluki-core::execution` (batch header, generation tokens, the `Kernel` trait, turns, budgets, loss classes), and
`saluki-metric-plan` (model, IR, schemas, canonical JSON and `PlanHash`, predicates, limits). B added kernel parameter
types, the 14-operator registry with effect contracts, the validator (33 rules) with duplicate analysis, and the
default-program fixtures. C added the reference interpreter, the equivalence oracle, and explain with golden files.
Tests in the touched packages: 936 after A, 985 after B, 1,028 after C, 1,048 after the fixes.

**Changed on the way** (the design records only the end state):

- Blobs began as a plan parameter type (`ParamType::Blob`, `Plan.blobs`, rule `blob_integrity`). The fix round removed
  them because no operator could receive one; blobs became artifact-only and composition inlines them.
- `PolicyArtifact::hash()` hashed the re-serialized form, so resolved values could slip past it. `ReceivedArtifact`, a
  digest of the document as received, replaced it (amendment 34).
- Stripping of resolved values first ran over the whole document and dropped IR map entries keyed `resolved`; it was
  narrowed to node parameter values. `LimitSpec.config_key` named an ADP configuration path and became `LimitSource`.
- Transition classes had two sources of truth (`Aggregate::transition`); the class is now derived from state contracts,
  and an operator can only make it stricter. Non-canonical durations such as `"60s"`, used in the fixtures, are now
  rejected.
- Scope exits are `exits` and `exit.<name>`, not the reviewers' `outputs`, because "outputs" already means declared
  outputs in the IR.

**Review findings that mattered.** The interpreter accepted batches and adapter output outside the port type (1, 26;
amendment 36). `policy/` nodes without provenance escaped artifact limits (2; amendment 33). Aggregation's contract left
out its policy loss (4; amendment 22). `Tolerance::accepts` treated infinities as equal to anything (10). Repeated JSON
member names were accepted, last one winning (12). `compare_reordered` was undecided on point-count differences (13;
amendment 31). The coverage checks were tautological and never evaluated the gateway predicates (28). The
duplicate-analysis pair check was quadratic in string comparisons, about 5 s in release and 50 s in debug on the
reviewer's probe; after the fix (each canonical predicate numbered once, crossings as a bitset, pair work charged
against `work_per_input²`) the same probe runs in about 0.3 s in debug.

**Decisions.** Canonical numbers are JSON integers within ±(2^53−1), because `serde_json`'s default float parser reads
`9007199254740991.0` as `...990`, so hashes would depend on the parser (amendment 21). Every native-valued parameter is
required, so there is no second defaults system. Unimplemented trace features are `Unsupported { stage }` errors, not
approximations (amendment 30). `FixedSizeEventBuffer::with_capacity` moved into stage 1 because a default buffer
allocates 1,024 slots (about 336 KiB) up front.

**Left open.** The approved-revisions query, the export bundle, and receipt-time size checks (stage 5); kernel factories
and a single tag-value validator (stage 2); explain without memory bounds; gauge, histogram, and distribution checks in
the reordering oracle (stage 9).

### Stage 2: extracted kernels and lowering

**Parts.** A: the batch header on `FixedSizeEventBuffer` (`EventsBuffer` grew from 40 to 80 bytes), the admission clock,
admission points and schedules, dispatcher stamping under a per-point mutex, blueprint snapshots, and graph checks. C:
the mapper and `AggregateCore` with a resumable `Flush`, the legacy transforms rewired onto them, and 18 hand-written
aggregation compatibility traces. B: the remaining kernels (namespace prefix, timestamp split, tag projection, host
enrichment, the selection matchers), one tag-value validator, and the default-program fixture through the interpreter.
D: ADP's `programmable_metrics` module, test-only: host facts, 26 bindings, the catalog, native outputs and recipients,
telemetry profiles, lowering, a 22-entry configuration matrix, and a structural comparison against `create_topology`'s
blueprint.

**The frozen-reference rule.** Every restructured kernel kept the native code verbatim in a `#[cfg(test)]`
`reference.rs`, with kernel-against-reference property tests, so rewiring the legacy component could not make the
comparison tautological. It worked: C added 3 such tests and B 8, and every hand mutation was caught, though B's
fast-path removal was caught only after non-metric events were added to the split property test.

**Changed on the way.**

- The task forbade awaiting a dispatcher inside the aggregation core and draining a flush into a `Vec`. The state
  therefore became a `hashbrown::HashTable` iterated by bucket index under a `Flush` cursor, with removals deferred so
  that `abandon` after a failed delivery leaves the native post-error state. This became stage 6's drain seam.
- The epoch at publish was first tracked by schedule pointer identity; republishing a cut then sent a reset-switched
  point back to the old generation (finding 4, major). It is now an `ObservedCut` keyed by the new generation (amendment
  42).
- Binding names became typed-model paths, and routing groups are keyed by a digest of their endpoints, stable across
  renumbering (amendment 37).
- `git apply --3way` refused A's patch because C's uncommitted work did not match the index; plain `git apply` applied
  it cleanly.

**Review findings that mattered.** Heartbeats kept flowing after their owner stopped when the downstream channel was
shared (5). An unbounded heartbeat period panicked the driver (6; now 1 ms to 1 h). The host-tags deadline was rarely a
plan duration with a real clock (7, major; now whole seconds, rounded up). Telemetry profiles named native metrics that
nothing in the plan path could produce (11, major; each metric now has a `MetricSource`). With two configuration errors,
lowering reported a different one than native (10; amendment 38). Lowered plans were never executed (14; an every-branch
interpreter test was added, with per-gateway adapters that stage 4 replaced).

**Left open.** The extracted cores keep native quirks on purpose: `flush_open_windows` has no observable effect,
counters get leading and back-filled zeros, and the by-type context gauges drift after a kind conflict
([follow-ups](programmable-metrics-status.md#follow-ups-not-tied-to-a-stage)). Durations a plan cannot represent fail
lowering (amendment 14). The admission mutex's cost on the DogStatsD decoder pool was never measured.

### The surface syntax and the demo (first pass)

Both were isolated first passes of layers meant to change easily, each with one implementer, two reviewers with
different lenses, and a fix agent (v1, then v2). Both started on stage 1 and ran beside stage 2; the demo was built on
syntax v2. Later demo work happened inside stages 5 and 6 (and 7 and 9).

**Syntax.** v1 had the lexer, a recursive-descent parser, the builder, the printer, span maps, and diagnostics; v2 fixed
12 findings and added a source size limit. The review found quadratic parsing and printing of wide tee and merge nodes
(2 × 10,000 ports: 6.98 s, then 41 ms); valid artifacts printing to text the parser rejected (U+007F prints at six times
its JSON size, hence a source limit of eight times the artifact limit); fractional numbers drifting on every round trip;
`matches(...)` leaves that never broke past 120 columns; and quadratic diagnostic columns (26,000 statements: 243 ms,
then 11 ms). Rejected: order-independent artifact equality, since approval binds to the document as received; canonical
order is enforced instead, and lowering and exports must write it. Also rejected: filling lists several elements per
line, which would make allowlist diffs no longer line-based. The second syntax commit boxed the arms of proptest unions
and ran the tests in a 1 MiB thread, because the inline value trees needed more than 2 MiB of stack per draw in debug.
Printed text is about 60% of compact JSON (2,106 against 3,492 bytes for the default program's scope); deeply nested
predicates print in O(depth × size) ([follow-ups](programmable-metrics-status.md#follow-ups-not-tied-to-a-stage)).

**Demo.** v1 had the launcher, recording intakes, traffic, the scraper, the durable stub, structural checks, and the
single-page UI; with no artifact channel in ADP yet, it was checked against a fake poller. v2 fixed 22 findings and was
driven in headless Chrome against a real ADP and a Python stand-in for the channel. The review found ADP orphaned on a
signal during the ready wait, one approval able to publish several revisions, store writes that reported failure after
taking effect, the IPC private key briefly world-readable, intake rates shown after aggregation (so traffic changes
barely showed), and a green "checked against ADP's catalog" status for programs ADP would reject. The port pre-check
exists because v1 took a stale ADP for the one it launched. A 20-byte malformed v3 payload aborted the whole demo
through `stele`; that decoder fix shipped with the demo ([changes outside the new
code](programmable-metrics-status.md#changes-outside-the-new-code)). The aggregation window became 1 s and counter
expiry 0, because 10 s windows left the 5 s columns empty and idle counters sent zeros for 300 s. The macOS boot needs
and the agent-tooling quirk that rewrote `"/` string literals are in [Environment
notes](programmable-metrics-status.md#environment-notes).

### Stage 3: the plan-driven metrics path

**Parts.** A: the generic region host in `saluki-core` (driver trait, control mailbox, `catch_unwind` supervision). B:
the frame (R1), placement, region programs, plan diff, and an I/O-free `InstallerCore` (amendments 43 to 46). C:
`MetricRegionDriver` (kernel slab, per-generation programs, panic budgets, region bounds), a frame-of-drivers property
test against the interpreter, and the `region_driver` bench. D: ADP builds its metrics path from the frame, with the
legacy path moved under `#[cfg(test)]`. E: the exit-gate tests and measurements ([stage 3 exit
evidence](programmable-metrics-implementation.md#stage-plan)). A worked in its own work tree and touched only the
"Region host" section to avoid colliding with B; nothing was committed until after the fix round.

**Changed on the way.** The design's control mailbox (a watch of the latest control plus a queue) became an unbounded
queue with a one-shot acknowledgment per message, because every installer step needs its own acknowledgment. Fusing
maximal synchronous sets formed a cycle through the passthrough fence with `dsd_agg`, so slots became sets of equal
fence depth (amendment 43), and the fix round made them cost-staged. Configurations above 2^53−1 fail at boot (amendment
14), chosen over keeping the legacy builders in production.

**The throughput regression.** Part E measured one saturated DogStatsD stream at 0.68 to 0.72 of legacy throughput on
four workers (5.86 M against 8.62 M metrics/s in one run) while using 1 to 13% less worker time per metric; on one
worker it was 1.02 to 1.05. Legacy ran the five DogStatsD transforms as five tasks, which pipelines consecutive batches
across cores; the fused `dsd_pre_aggregation` region ran them serially in one task at about 250 ns per metric. Paced
phases cost 2 to 7% more CPU, because passthrough left as one buffer per input batch instead of one per 1,024, but p99
fell to 0.19 to 0.47 of legacy. E left the four-worker test asserting only a 0.6 floor and proposed amendment 47 with
two options: restate the gate per unit of CPU, or bring stage 9's parallel regions (partitioned by context) forward. The
fix round took neither and gave R1 a cost model: `OperatorDef::cost` in estimated nanoseconds per event,
`REGION_COST_FLOOR` 64, a budget equal to the costliest native component of the boot plan, and
`FusionVerdict::Pipelined` cutting a chain into stages that run as separate regions, never cutting a native component.
The stage 3 costs put the default configuration in two regions (costs 55 and 36) and the every-branch fixture in five.
The same round moved buffers instead of swapping elements (`retain` and `move_into` over a `Vec`), routed one-way
timestamp batches whole, and rebuilt the measurement on the production runtime layout with five alternating rounds;
legacy against itself stayed within 1% throughput, 4% worker time, and 1% p99, so the asserted tolerances became 7%, 7%,
and 10%. After the fix, plan over legacy was 1.04 throughput on four workers and 1.27 on one, at 0.77 to 0.94 of the
worker time and 0.82 of the bytes allocated per metric. The full table, the region driver's overhead (at most 1.5% over
hand-written kernel calls), and the host's (about 70 ns per buffer) are in
[Risks](programmable-metrics-implementation.md#risks) and the pipelining paragraph of [Physical frame and region
programs](programmable-metrics-implementation.md#physical-frame-and-region-programs). Stage 9 reopened whether R1 stays
([status](programmable-metrics-status.md#decisions-awaiting-the-owner)).

**Findings that mattered.**

- A pre-existing `saluki-core` bug: the blueprint recalculated topology bounds before inserting each component, so the
  last one added was never counted (21). Every ADP topology now reports one more interconnect.
- Retired generations were released only by later batches, so an idle region held them (2). Preparations sent before the
  region hosts ran timed out and filled the overlap budget (3, 9).
- Panic recovery rebuilt every kernel unsupervised (4). A commit over a failed active generation kept it as
  last-known-good (12, major).
- Fused kernels' memory was charged to the region, so per-component accounting disappeared (6, 22; per-kernel resource
  groups, amendment 19).
- The filterlist update counters were missing until the first change, and `metric_filterlist_size` vanished from
  `/metrics` because internal telemetry evicts metrics with no live handle.

**Left open.** The Antithesis hook decision (deferred to stage 5, never taken); bounds of non-boot generations (stage
7); a differential matrix that runs standalone only; a frame property test that never generates candidates with node
sets other than the boot plan's.

### Stage 4: gateways and stateless rewrites

**Parts.** The interfaces step fixed the rewrite framework (`optimize`, the pipeline R3, R5a, R6, R7, revalidation after
each application), the decision types, and the capacity hook. Four parts then ran in parallel: decisions (R2 interning,
decision-time R3, the R4 memo, `DecisionFunction`, the selection bench); rewrites A (R3 in structure, R7); rewrites B
(R5a, R6, the real capacity bound, the rewrite harness); and ADP policy (the gateways lowered as `split` plus a `_true`
emit and a `_false` drop). Integration applied the fixed patches with 0, 3, 4, and 5 conflicts. The stage took about 5 h
40 min from the stage 3 commit.

**Changed on the way.**

- The interpreter's delivery order went breadth-first, then depth-first, then pass order. Breadth-first let a merge
  receive the shorter path's records first, so a fold that changed a path's length changed which records a capacity
  bound cut. Rewrites A first worked around it (batch-aligned bounds, an order-insensitive check, an amendment) and
  withdrew that after its review; rewrites B had switched to depth-first on its own. Integration kept pass order, since
  depth-first makes output order depend on how a kernel cuts its output into buffers
  ([interpreter](programmable-metrics-implementation.md#reference-interpreter-and-equivalence)).
- `MEMO_COST_FLOOR` went from 20 to 30 to 25. At 20 the listener filterlist and the post-aggregation filter memoized.
  The decisions review measured a hit at about 10 ns and a miss adding about 4 ns, giving the rule `p(t-10) > (1-p)4`
  and a floor of 30, while ADP policy mirrored the rule in R1 with its own floor. Integration set one floor of 25
  (allowlists memoize, one-leaf filters do not), so R1 prices exactly what regions run. A two-pass batch memo lookup
  made misses 1.4 to 1.6 times slower and was replaced by one probe per lookup.
- R1's frame for the lowered gateways took three versions. At first each routing filter (and sometimes MRF or AF) landed
  in a region of its own, including an emit-only region with two routing groups, which a "zero added cost never moves"
  rule patched. The ADP policy review then found the policy region was never one decision (finding 1, major) and that
  the zero-cost rule could fuse a join past the budget (finding 2). R1 now never cuts a decision, prices it as one memo
  lookup, and accounts per region; the every-branch frame went from 8 slots to 5.
- R3 widened from literal constants to splits that validated port types prove constant, so kind-decided splits fold.
  Three parts claimed amendment 49; the final numbers are 49 to 52.

**Performance.** The machine was shared (load 10 to 47), so criterion runs were unstable and the parts built
alternating-rounds harnesses from the benches' own setup; the one behind the published selection table was never checked
in. The first selection result (1.25 to 2.1 times faster than hand-written) measured a frame the default program never
compiles to (decisions finding 1, major); on the real frame the gate was missed (1.6 to 2.3 times slower with hits)
until the R1 fix. The final table is under Selection cost in [Risks](programmable-metrics-implementation.md#risks). The
ADP gateway measurement never ran on an idle machine, since legacy against itself failed its own tolerances on every
run; before the R1 fix, worker time reached 1.20 of legacy and one live-update p99 2.96 times, and after it throughput
was 1.13 to 1.23. Live updates allocate 1.16 to 1.37 times legacy per metric, because every change lowers, compiles, and
prepares every region. `PROPTEST_CASES` has no effect where a test sets `ProptestConfig::with_cases`, so early runs
claiming 5,000 cases ran the configured counts.

**Findings that mattered.** R7's reordering was invisible to the program fingerprint, so regions kept the old program
(rewrites A 2). R5a and R6 ignored the filter's own ports at scope boundaries, pulling work into a scope or leaving a
dangling scope input (rewrites B 1, major). A whole move across an R1 cut moved a kernel to another resource group
without a record (rewrites B 4). The capacity property tests could not detect the breadth-first mutation (rewrites B 6),
and no property test caught a memo key that ignored the kind (decisions 3). Region bounds missed decision scratch and
count tables (decisions 6), and matcher bytes were undeclared.

**Left open.** Two random-plan harnesses (R6 applies in 9 of 512 pipeline runs); nothing compiles with an operand
profile; memo memory accounted for the boot generation only; R1 pricing a memoized decision as a hit; stage 3's
opaque-gateway fixture still in use.

### Stage 5: the artifact channel

**Shape.** A first workflow ran on the stage 3 commit while stage 4 was built: an interfaces step (channel types,
envelope, transport hash, acknowledgment types, and correspondence map implemented, the rest stubbed, with a
file-ownership table), then three parts in parallel (compose and export, installer artifacts, ADP channel), each
implemented, reviewed, and fixed. The parts kept to their modules and renamed nothing, so integration onto stage 4 had
one textual conflict, in ADP's installer telemetry, and every suite passed, because the parts already used stage 4's
default-program form. A second workflow ran three follow-ups in parallel (lifecycle tests, demo end to end, burst
slack), a merge without conflicts, a three-lens review, and one fix agent. The commit came 11 h 20 min after stage 3's,
about 3 h of it waiting for stage 4.

**Changed on the way.**

- `base.program_hash` was designed as the hash of the whole plan. It hashes only the exported scope, so changes outside
  the region do not make an artifact stale, and covers output fingerprints by hashing the whole export without the field
  (amendment 61).
- Correspondence first compared only operator and parameters, so a kept lossy node rewired to another stream kept native
  authority (compose finding 1, major). The fix is the `Feeds` rule; the reviewer's stricter "every upstream edge
  unchanged" was rejected because with stage 3's opaque gateways it made every tee edit invalid.
- Panic budgets went from per generation to per region to per node, because `metrics_enrich` mixes host enrichment with
  policy nodes; nobody owned that gap until the final review. Re-compositions cancelled preparations, so under an
  artifact a configuration stream never committed (major); they now queue, latest wins (amendment 45). Re-approving the
  same document left the status at the old revision; documenting it was rejected because the demo shows the revision,
  and `reapprove` was added (amendment 60).
- The demo's check was meant to compose and diff like ADP, but `compose` needs the default program and no route served
  it. A `/programmable-metrics/default-program` route was added rather than putting the whole plan in the export bundle,
  which stays what authors edit. The approved-revisions query was deferred: composition is the only writer of artifact
  provenance, so the query would check that path against itself
  ([Validation](programmable-metrics-implementation.md#validation)).
- Burst slack (amendment 48): with fusion's queues gone, a burst backed up to the source after 257 batches on the plan
  path against 644 on legacy. The region inputs follow `n(c+1)-1` along the longest path, so totals equal legacy exactly
  (515 into `dsd_pre_aggregation`, 257 into `metrics_enrich`, 386 with MRF or AF).
- Tests that built any HTTP client failed under full parallel runs on macOS (28, then 32 failures), because the proxy
  connector loaded the platform roots even without proxies. The fix changed `saluki-io` rather than adding a stub-only
  client ([the root-certificate change](programmable-metrics-status.md#the-root-certificate-change)).
- The installer part's amendments 49 to 56 became 53 to 60, since stage 4 had taken 49 to 52. The disk filled during the
  parts, so `make fmt` could not run its update step and its other steps ran by hand.

**Findings that mattered.** A revoked artifact came back into effect or as the rollback target (installer 1, blocker;
amendment 58). Blob inlining multiplied artifact size (compose 2, major). A rejected newer artifact cleared `degraded`
(installer 2, major). The strengthened property test found a same-document composition left in flight under a newer
revision (amendment 59); before that, the property model was nearly vacuous, since absolute revisions made almost every
delivery stale (installer 8). A stalled response body stopped polling for good, and a 200 answer without `products`
revoked. The explain route described the plan without stage 4's rewrites. Artifact kernel sizes were unbounded, so a
huge mapper interner could abort ADP; the 64 MiB `artifact_memory` limit came from this (final review 1, major).

**Measured.** Burst parity at 644, 386, and 515 batches on both paths; demo check runs of 67 to 73 s, with publish to
acknowledgment in 0.8 to 2.1 s and canaries balanced; 3,257 of 3,257 tests in the full run after the fixes.

**Left open.** Fallible kernel construction (an allocation failure still aborts); provenance after a re-approval; panic
charging in mixed decision groups; receipt limits that count unmodified nodes; a preview that does not compile into the
frame; an exact whole-run canary check.

### Stage 6: stateful kernels

**The design.** Two drafts were written independently on the stage 3 commit. Draft A, correctness first, chose an
explicit protocol wherever a cheaper design's argument could not be made exact: a three-phase cut armed over every
admission point, cut messages to regions, R5b by ghost entries. Draft B, minimal change first, observed that the
aggregation core already takes time as an argument and changed only the two clocks it receives, reusing stage 1 to 3
machinery: kernel keys as lineages, the schedule's `Cut`, kernel-owned ticks, a cut on any parameter change, `lcm` up to
900 s, and R5b in the specification's removal form. The merge compared them in a 29-row table and filled gaps neither
covered: the driver's start hook, the heartbeat period, cut polling that does not block on a stalled dispatcher,
idempotent re-issue after a worker restart, and the tick gate. Its review raised 33 findings. The blockers: a weak
`Retiring.by` let a commit after finalization strand the old lineage (now held strongly from prepare); a skipped tick
kept a past deadline and spun the host; and a failure of the replaced generation during a cut took stage 3's one-store
rollback. The review also found that parts based on interface patches would conflict with stages 4 and 5 in the core
paths, so the later parts started from the integrated tree. Lowering the clock-reset threshold from 600 to 300 s was
left to the owner and still waits ([status](programmable-metrics-status.md#decisions-awaiting-the-owner)).

The committed [Decisions](programmable-metrics-implementation.md#decisions) list the choices, and the [cut
protocol](programmable-metrics-implementation.md#the-cut-protocol) and
[R5b](programmable-metrics-implementation.md#r5b-emission-suppression) sections give the reasons for arming only
affected points, heartbeat recipients, the cancel lock order, rollback to the last-known-good plan, and R5b as emission
suppression. The rejected alternatives they do not record:

| Chosen | Rejected | Why |
|---|---|---|
| Heartbeats carry their scheduled generation | No generation (B) | A slot could not tell a reset after publication at an idle point from one before it |
| Progress throttle 100 ms | 1 s (B) | Ten times less closure lag under fully filtered traffic, for about 10 header-only buffers per second |
| Reset through `control(ClockReset)`, instance kept | Retire and rebuild (A) | Same output, no rebuild on the data path, continuous budget credits and gauges |
| Lineage key: node plus state contract | Any parameter change is a cut (B) | The specification classifies transitions by contract |
| Retirement at the crossing | First tick after every point switched (A) | Two lineages would hold contexts at once under one budget, losing capacity near the limit |
| `by` held strongly | Latching `at` | Simpler, and keeps `T` readable through shutdown |
| `lcm` at most 300 s | Up to 900 s (B) | Keeps the specification's cut-wait bound and bounded overlap |
| Re-composition waits for finalization | Re-arm a replacement at the same `T` | A cancel transaction per native change and a second token with equal `T` |
| Driver-owned ticks, gated by in-band headers | Kernel-requested waits (B); driver-requested heartbeats | The interpreter's `advance` means every state owner ticks now; a driver cannot await or reach the registry |
| One revision high-water mark across scopes | Per-scope marks | One mark satisfies "revisions strictly increase" and keeps the protocol field |
| A panic is charged to the generation whose call ran | Any state-owner panic fails the generation | Lineage keys cover only the window, so a kept default program would exit the process |

Also dropped in the merge: A's spread guard and installer wall clock (one point per slot made them unnecessary), A's
lock order for re-admitting points (a cut with such a point is now a capability error), and B's sum-of-limits placement
rule. Insertion buckets by admission time in both drafts; the review moved the flush from `floor(W)` to the tick gate,
because lag at a tick shifted back-fill by a bucket and changed leading zeros.

**The implementation.** Phase A ran on the stage 3 commit, overlapping stages 4 and 5: interfaces for `saluki-core` and
the kernels crate, then P1 (the kernel and its configuration) and P2 (the admission registry, ordered-lock transactions,
heartbeats with generations, the deadline guard) in parallel. Phase B ran on the stage 5 commit: a second interfaces
step that applied phase A's fixed patches (one hunk by hand, since stage 5 had moved the code), then P3 (isolation
slots, lineage-keyed kernels, the frame harness), P4 (the interpreter with lineages, cuts, resets, the tick gate, traces
1 to 15), P5 (the cut protocol in the installer core, one configuration per scope), and P6 (ADP's worker, snapshot
bridge, telemetry) in parallel. Integration then made [the flip](programmable-metrics-implementation.md#the-flip),
followed by a final review; from the end of the first interfaces step to the end of the final fixes took about 18 hours.
Merging P4 after P3 conflicted on the compatibility tests, and P6's driver change gave way to P3's state-binding
carrier.

**Changed on the way.** R5b came in three steps: P4 keyed it on the operator name, P3's fix added site filters, and
integration replaced both with the kernel's emission-filter flag. An arm that can never succeed was first a timeout
deferral, then a generation failure, and finally `ArmOutcome::Stranded`, which aborts and prepares again. The deadline
rule "later than `turn.now()`" was dropped because the designed driver breaks it legitimately; the host now enforces
only that a fired deadline is not kept. The kernel accounting path rule also matched the `metrics_enrich` slot and was
restricted to isolation slots. The heartbeat period went from 5 s to 1 s. ADP-level fault tests (failed-cut loss
windows, arming under blocked sources, a bridge panic) and the shadow-native measurement were planned but not built
([exit evidence](programmable-metrics-implementation.md#exit-evidence)).

**Findings that mattered.**

- A clock reset on an empty core kept its last flush time, so 1 point became 61 (P1 1, major). An empty final retirement
  latched `T`, stamping later output about an hour ahead (P3 2).
- `artifact_memory` rejected every artifact that changed aggregation (208 MB of state at 1M contexts against 64 MiB), so
  state bounds were split out of it (P3 1, major). A state owner fed straight by an admission point could not be placed
  (P3 3).
- `cancel` started the waiting install, so a revoked revision could come back (P4 1, major).
- Under the single high-water mark, redelivering a lower-revision scope's active configuration was rejected as stale,
  and one invalid artifact in a snapshot rejected another scope's valid one for good (P5 1 and 2, major).
- `Installer::prepare` dropped the state bindings, so no retirement ever happened at the crossing (P6 1, major).
- Revoking back to the default program could be refused by the 300 s width bound; `check_way_back` makes revocation
  always possible (final 1, major; amendment 71). A newer artifact that `start` rejects still cancelled the cut in
  flight (final 7). ADP reported no base hash for the pre-aggregation scope (final 15, major). `property_test_r5b_exact`
  was flaky because its generator put two advances at one time (final 2).

**Measured.** First bench passes on a busy machine were noisy (a 100,000-context flush took 43.7 ms in drain steps, then
20.8 ms); integration's figures and the five passing measurement tests are in [exit
evidence](programmable-metrics-implementation.md#exit-evidence). Live updates allocate 1.3 to 2.1 times legacy per
metric. The integration gate ran 3,258 tests, with 4 sandbox failures in `saluki-components` proxy tests.

**Left open.** Several upstream points per isolation slot and their reset rule (stage 9); multi-hop pass-through
placement; trace 14's irrevocable phase outside the interpreter; per-scope attribution of a joint two-scope candidate
rejected at `start`; a demo check step for the pre-aggregation scope ([deferred
work](programmable-metrics-status.md#deferred-work-by-stage)).

### Lessons from stages 1 to 6

- The serial chain of implementation agents limited wall-clock time, not the build: each took 40 to 85 minutes, 10 to 27
  of them reading before the first edit. The interfaces-first fan-out of stages 4 to 6 roughly halved later stages, and
  a short orientation brief kept parts from reading every earlier handoff.
- Overlapping stages works when the next stage's parts own separate modules and rename nothing (stage 5 on stage 4), not
  when they change the same core paths (stage 6's second phase).
- Reserve amendment number ranges per part; parallel parts collided in stages 4 and 5.
- Mutation checks found vacuous tests that review alone missed: stage 1's coverage checks, stage 4's memo key, stage 5's
  installer property model, stage 6's R5b residual.
- Benchmarks need an idle machine and an A/A baseline: a shared machine made criterion unusable in stage 4, and the
  stage 3 regression showed only because the measurement compared against legacy on the production runtime layout.
- Per-part work trees fill the disk, so delete their build directories when they finish. `git apply --3way` fails when
  the index holds uncommitted work, and fast-forward merges fail once a work tree's branch diverges.

## Stages 7 and 9

How stages 7 and 9 were designed and built, from October 10 to 13, 2026. The design as built is in
[the stages 7 and 9 plan](programmable-metrics-stages-7-and-9.md): its "as built" sections, the measurement results
(8.5, 8.6), the alternatives (4.18, 5.16), the amendments, risks, and open questions. What is left for the owner is in
[the status document](programmable-metrics-status.md#decisions-awaiting-the-owner). This log records the history
those documents mostly leave out: what was tried, what failed and why, what reviews and integration found, and why
each decision went the way it did. Where a document already covers something, the log links to it.

| When (October) | Step | Commit |
|---|---|---|
| 10, 16:10 | Stage 6 committed: the base for the design | `b01049372e` |
| 10, 22:30 to 11, 01:45 | Surveys, drafts, judges, synthesis, first review round | `2729a33271` |
| 11, 02:05 to 02:49 | Second, focused review round, overlapping I7, P9.0, and P9.F | `d7316c474f` |
| 11, 03:50 to 19:45 | Phase A merged into the stage 7 base; stage 7 parts, I7-Z, review, fixes | `b7e63f147b` |
| 11, 20:45 to 22:48 | P9.F merged onto stage 7; first measurements | `b523d9002b` |
| 12, 00:00 to 13, 06:22 | I9, the parts, O1, O4, O5, investigations, Z1 to Z3, review, fixes | `8b44c153aa` |

### Design

#### Surveys and drafts

Four read-only surveys of the code at `b01049372e` came first: outputs (the encoder, the Datadog and Cluster Agent
forwarders, what is live and what needs a restart), runtime (topology lifecycle, the execution layer, watermarks and
epochs, R1's cost model), DogStatsD to aggregation (where stage 9 would cut the path, the Agent's own parallel
precedent, kernel costs), and prior art (aggregators, stream processors, shard-per-core runtimes). The prior-art survey
recommended one shard per core, with producers routing by the final key through fixed key groups. Three harvests (the
repository, the stage 1 to 3 notes, the stage 4 to 6 notes) went into the status document. They reached the drafts
only through the facts they recorded, such as R1's measured 0.68 to 0.72 of legacy before fusion.

Stage 7 had one draft and no judges. It took the owner's direction as given: a physical frame fixed at boot with
multiplexed logical outputs and recipients, the forwarder turned into a host. Its leases, fingerprints over the
selector, the input barrier, the `resources` product, and the 30 s drain all survived. Two of its choices did not. A
removed output first fell back to the default program, until the first review round made removal fail closed. The
native set was published at the commit, until resource transactions took it over, because a cut would otherwise hold
back an endpoint change for logs and traces by up to 300 s
([4.18](programmable-metrics-stages-7-and-9.md#418-alternatives-considered)).

Stage 9 had three drafts, each written under one lens. All three routed by the context key at the exit into `dsd_agg`
with no exchange stage. None used partial aggregation per producer, a shared concurrent table, or a flush barrier. All
three fixed lane counts at boot and removed R1. They differed here:

| Question | A: least coordination | B: least disruption | C: semantics first |
|---|---|---|---|
| Admission | a point per decoder, plan code inline | one shared point | a point per output, one clock |
| Key groups | 256 | none (`fastrange`) | 256 |
| Coalescing | across batches, up to the second | none | within one host turn |
| Budget | credit cells | exact global compare-and-swap | exact global compare-and-swap |
| Reset | one shared epoch atomic, epoch cut | an epoch-close header | shared clock, a reset window per lane |
| Post-aggregation | replicas inside the partitions | its own slot with lanes | its own slot with lanes |
| Identity | one component | one component | components with an alias |

A also raised the decoder default to `max(vcpus - 1, 2)`. B shipped in measured increments. C alone found that the
aggregation kernel assigns `last_seen` rather than taking the maximum, and wrote an exactness ledger and oracles.

#### Judges and synthesis

Two judges scored the drafts on six criteria, weighting coordination three times and scaling twice (owner direction):

| Judge | A | B | C | Verdict |
|---|---|---|---|---|
| Correctness (out of 105) | 68.5 | 63.5 | **75** | base C; B's increments; A's single stamper and one `dsd_agg` |
| Performance | **64** | 51 | 61 | base A, but relay-first (no plan code in the source); C's verification whole |

They agreed on more than the scores suggest: B's delivery shape, C's semantics, single-stamper admission, the isolation
fence kept (no replicas), and lane keys of Saluki's own. The correctness judge found five bugs in all three drafts.
One was the node context limit's check-then-act, which became the bounded node acquire. Another was that every
harness ran lane calls one at a time and so could never race, which led to loom models and a stress run. The
performance judge checked the code. CPU-time reads are Linux-only, so the gate could not be decided on the Mac.
Workers inherited a contended resource group. The isolation driver cloned its tracker per batch. The region host's
turns rarely held more than one ready batch, so C's coalescing within one turn would rarely engage.

The synthesis took C's producer lanes in the relay-first shape, with A's single stamper, C's `SourceClock`, A's and C's
256 key groups, A's coalescing across batches (bounded at 5 ms after review), one `dsd_agg` component with an exact
budget, C's shared lineage records and reset window, C's post-aggregation lanes, and B's increment order, telemetry
rule, and sampled CPU time. [5.1](programmable-metrics-stages-7-and-9.md#51-strategy-and-decisions) lists the
decisions and [5.16](programmable-metrics-stages-7-and-9.md#516-alternatives-considered) the rejections, with their
sources. A's inline decoders survived only as option O10. B's shared point was dropped because, as the performance
judge put it, B's own fallback amounted to a point per lane anyway. C's reuse of `dogstatsd_pipeline_count` was dropped
because its default would have turned state lanes off. All three drafts expected R1 to go; it was kept (below).

#### Two review rounds

The first round reviewed the synthesis before it was committed, through five lenses. Its fixes are in `2729a33271`.

| Lens | Blockers | Majors | Minors |
|---|---|---|---|
| Completeness | 0 | 8 | 14 |
| Feasibility | 1 | 8 | 16 |
| Operability | 0 | 6 | 20 |
| Performance | 0 | 7 | 9 |
| Semantics | 2 | 9 | 14 |
| **Total** (about ten overlapping) | **3** | **38** | **73** |

The blockers: the hosts could not use the control mailbox, which was private to the region host. It moved to
`execution::control`. A cut's old and new lineages hold the same context whenever D > 1, which was accepted as
capacity behavior, counted, and recorded as an amendment. The reset window was not invariant to the lane count, so the
final old-epoch header now goes to every lane. Among the majors:

- Removal fails closed.
- Resource transactions publish independently of metrics generations.
- Declared outputs became plain identifiers, because the lexer has no `/`.
- The shared encoder keys stay restart-only.
- The gate reaches W = 16 and 32.
- CPU measurement counts the whole process and runs real encoders.
- Lease shard channels shrank to the native 8 buffers (about 47 MB each at 136).
- P7.1, P9.1, and P9.3 were split in half, because no part had a size estimate and three were far larger than earlier
  stages' parts.

Credential stripping at rest, stable directory names, upgrade adoption, and a janitor were judged over-design and
deferred (A18, A19).

The second round had two focused lenses and ran against `2729a33271`, after I7 had started. Most of what it found was
in the mechanisms the first round's fixes had added.

| Lens | Blockers | Majors | Minors |
|---|---|---|---|
| Stage 7 resource lifecycle | 1 | 13 | 8 |
| Stage 9 time and state | 1 | 10 | 12 |

The lifecycle blocker was that recipients had no incarnation, so a quick add, remove, and add again could release a
budget twice or start two loops on one directory. Removal still failed open until the next commit, so it moved to the
delivery host as a withdrawn set. The time and state blocker was that after a reset, a lineage's lanes could split
across two records; stage 6's trace 11 failed when walked by hand at D = 3, P = 2. `d7316c474f` changed 1,108 lines of
the plan and added the table [Changes after I7
started](programmable-metrics-stages-7-and-9.md#changes-after-i7-started) (C1 to C9, absorbed by I7's fix step and the
parts) and a paragraph of changes after P9.F started. Rows C10 to C14 were added later by the parts.

### Phase A: interfaces, harness, and foundations

**I7** (on `2729a33271`) built every stage 7 interface with behavior unchanged. Its only data-path change: exits to
outputs wrote a lease into `header.entry`, which nothing read yet. The plan changed under it while it ran, so it re-read
4.2 to 4.13 at the end. It recorded 23 deviations. One matters for compatibility: `IR_VERSION` stayed 1, since the new
members are optional and omitted when empty, and a version bump would have changed every base program hash and made
every stage 5 and 6 artifact stale. Its review found 12 problems and the fixes found 2 more. Both fake delivery hosts
broke the host contract and were rewritten. Plausible stub return values were replaced by `Failed` exits and warnings
naming the owning part. A user-visible acknowledgment had changed its wording from region to component. The
resource-group unit test could not detect the bug it was for, so I7 added an integration test with the tracking
allocator, `saluki-core/tests/worker_resource_group.rs`, which `make test` does not run. It later led to a CI job.

**P9.0**, the measurement harness, was built while the machine ran at a load of 8 to 33. It ran no series, only smoke
runs, mostly in debug. Its review found eight problems, all confirmed:

- After a pause the generators burst to catch up, so a paced phase ran at 142,410 a second against a pace of 104,369.
- M8 could not see drops, because waiting senders hide them. While fixing this, the review found that on macOS a
  blocking connected Unix datagram socket refuses rather than waits, so that transport had been silently lossy.
- M8 never reached a tick with 1M contexts.
- Workspace builds always compiled the `measurement` feature.

Its smoke runs also found that `dsd_debug_log_out` clones every buffer beside `dsd_stats_out`. That clone later
turned out to be the cause of the memory investigation's symptom.

**The stage 7 base** merged I7 and P9.0, both clean. It dropped P9.0's CI job on the instruction of the orchestrating
session. One semantic conflict appeared: P9.0's collector built a `Delivery` without I7's new `entry`. C1 to C9 were
checked against the code. Two of them changed here: aborts became per creator (`Creator::{Generation, Transaction}`), so
ADP need not keep a copy of every preparation across worker restarts, and a prepare timeout became a separate input.

**P9.F** built the foundations, each neutral at one lane ([P9.F as
built](programmable-metrics-stages-7-and-9.md#84-stage-9-foundations-interfaces-step-and-parts)). Taking the maximum
of `last_seen` everywhere broke four frozen-reference properties, whose generators step the clock back as a native
wall clock can, so the maximum became opt-in. The review found five problems, the major one that the fused slot's
periodic kernel publish was untested: deleting the loop left every test passing. At the merge onto stage 7, P9.0's
`state_budget` bench was found still running the old check-then-act admission, so M4 would have measured a protocol no
longer in use.

**The first measurements** are in [8.6, first
results](programmable-metrics-stages-7-and-9.md#first-results-stage-7-with-p9f-before-i9). The runbook's "load
below about 2" could not be met: an idle desktop on this Mac sits near 3. Tests waited for a load below 4 and recorded
it. M2 triggered O4 and O5, and M4 triggered O1 (104 to 1,340 ns per admission per lane at P = 2 to 16, against 4.5 ns
alone), and all three moved into phase C so that the gate would measure the budget and ingest that would ship. The
container smoke could not decide the gate. The harness's context limit (contexts plus a tenth plus 100) dropped about
a sixth of the metrics on both images; it was raised in Z2.

### Stage 7 parts

Each part was built in its own work tree against the stage 7 base, reviewed read-only, and replaced by a fixed patch.
Every fix was checked by mutation. The design outcomes are in [Stage 7 as
built](programmable-metrics-stages-7-and-9.md#stage-7-as-built). Its "note" and "R" references are these parts'
notes.

- **P7.1a, recipient loops** (8 findings). Pinning tests showed three facts the plan had wrong. Queue directories
  include the data plane's version, so an upgrade moves them. Persisted retries hold the API key. Opening a queue
  does not trim it. Live concurrency took three attempts:
  - A fresh tower limit per publication let requests in flight overshoot the new limit.
  - A shared semaphore still let a lowered limit leak, because tower keeps a dropped `ready()` registered as a waiter
    (2 in flight under a limit of 1 for a second).
  - The fix: the loop does not poll the service while it owes permits. Its first test missed the bug in 2 of 3 runs,
    until the test server finished requests one at a time.

  Rebuilding the service on each change also reset the circuit breaker, so settings are now updated in place.
- **P7.1b, the delivery host** (3 major, 7 minor). The first commit rule leaked superseded recipients and never
  committed reopens. Skipping the loop's protocol filter rested on a wrong argument: after a live V3 flip, the stale
  filter dropped native-set metrics. A failed recipient's slot was freed before the input barrier, so payloads were
  delivered to the wrong recipient; tombstones replaced the review's proposed refusal, which would have rejected a
  declared revision for good. Renaming a declared credential always failed as a duplicate, a plan error in 4.8.
- **P7.2, the encode host** (4 minor). It found that P9.0's `encode_host` bench had measured V3 series under the name
  `series`, so any earlier M3 baseline was V3. Its resource groups were first registered as roots, and it first dropped
  metrics of a kind its lease had no variant for without counting them.
- **P7.3, the plan crate** (8 findings). Lease identity ignored the recipients' protocols, so the digest methods were
  removed outright, making stale callers fail to compile. A dual-ship addition dropped every emit to the main output
  from a version 1 base. `canonical_endpoint` echoed credentials (`hunter2`, `api_key=`) in errors.
- **P7.4, the installer core** (1 blocker, 3 major, 3 minor). The resource-table property test (30,000 cases, about
  245 s) found 12 defects in the new code before review. The blocker: a commit did not commit the failed incarnation
  a transaction replaced, so later candidates waited forever. A review major could not be fixed in the core's files:
  the delivery host's two contracts (an incarnation created at most once, an abort restoring a failed marker) were
  missing. With either rule off in the fake, the property test failed at case 66 or 610. Integration added them.
- **P7.5, ADP** (8 findings). It was built against seams it marked, with copies of other parts' helpers. Native
  recipient membership depended on API key values, so one blank key dropped its recipient and an invalid primary
  failed every later lowering. Membership now goes by configured position (amendment 134).
- **P7.6, the demo** (6 findings). It could not run against a real ADP; a Python stand-in and a harness without a DOM
  exercised it. It proposed the recipients status shape, which P7.5 had already built, and which became C10.

### Stage 7 integration and review

**I7-Z** merged in the order base, P7.1a, P7.1b, P7.3, P7.4, P7.2, P7.5, P7.6: 261 files. P7.1a had rewritten
`io.rs`, so P7.1b's changes there were re-applied by hand onto P7.1a's closer and settings. A checklist of 41 items
from the handoffs was closed, deferred, or rejected (the idle host's 8-buffer input saves nothing). The telemetry
parity test found a key validated twice. Two changes against native were found and release-noted: a burst reaching
several outputs of one encode host backs up sooner (451 batches against 515, amendment 135), and a removed endpoint
loses what its old leases build after the withdrawing transaction (137). The gate's results are in
[8.3](programmable-metrics-stages-7-and-9.md#83-stage-7-gate) and the status document. The first A/A run failed on
paced p99 (1.176) at load 2.5 to 3.9 and was judged noise. Panoramic used the same image's Agent as baseline, not a
`main` image, and `make test-integration` was not run.

**The integration review** ran four lenses (lifecycle, delivery, operability, tests and rules): 17 findings, 2
blockers, 2 majors, 13 minors.

- Blocker: respelling `dd_url` live made a second primary that waited forever for the first one's directory, cutting
  the primary off from all data.
- Blocker: HTTP request counters lived about 3 s, because every request took a fresh copy of the switchable client
  and the registry evicted its telemetry. The parity test missed it because its recorder never evicts.
- Major: after an abort, the next preparation was refused, 20 times in 20 with real loops.
- Major: the reclamation test's budget check could never fail.

The property test at 20,000 and 30,000 cases then found a lost failed marker when a replacement also failed. A ring of
1,024 remembered identities failed reclamation by 127 KB, so it is 128. The final 1,000-cycle reclamation passed in
682.9 s, and the demo check passed twice.

### Stage 9 interfaces and parts

The design outcomes of every review fix below are in [Stage 9 as
built](programmable-metrics-stages-7-and-9.md#stage-9-as-built).

- **I9** fixed every signature with behavior unchanged at one lane; a path only lanes reach returns an error naming its
  part. It built some pieces for real rather than as stubs, such as the lane layout and a keyed exit without coalescing,
  so P9.3b and P9.4 could run producer regions into frames with lanes at once. `Frame::compute` kept its shorthands (56
  call sites).
- **P9.1a, lanes in the topology** (1 major, 3 minor). State lane inputs count B events per slot, but senders sent
  1,024-event allocations: up to four times the bound at P = 8. The fix made senders allocate at most B, rather than
  charge 1,024, which would have quadrupled the bound. Its first non-blocking send reserved room "all or none" across
  fan-out, which reordered a tap (1, 3, 2), so an output feeding several components now always returns the item.
  `replace_transform` left bounds stale (1.7 MB against 0.37 MB).
- **P9.1b, the coordinator and admission** (2 major, 5 minor). Routing was configured twice, on the host and on the
  edge, and a mismatch dropped every buffer; the host-side declaration was removed. No per-lane telemetry was readable,
  and no M10 probe existed. Loom caveat: the first status model explored 5 schedules and missed a double epoch read,
  because loom checks a store only against the last access to an atomic.
- **P9.2, the frame with lanes** (1 major, 1 minor). Its note 2 accepted a state owner fed straight by a point at one
  state lane; at D > 1 that layout cannot be built, and would silently have run at P = 1. Now refused. It fixed two
  I9 bugs: a stateless producer slot got post-aggregation lanes at D = 1, giving a point several stampers, and lane
  routing errors masked the more basic error that a state owner had too many points.
- **P9.3a, keyed exits** (1 major, 2 minor). The plan's exactness property was false without sending an open buffer
  once its point moves a progress period past it (a counterexample at 1.000, 1.150, 1.160 s). Review: a lane that
  received none of a new second's events stayed up to 1 s behind, so exits also send when the admission second
  advances.
- **P9.3b, state lanes** (5 major, about 8 minor). Majors: the successor's first tick depended on which lane closed
  its window first (45 s against 30 s); a reset during a gated tick added a flush stage 6 does not make; a skipped
  epoch's data was lost as invalid; the crossing-overlap gauge cost O(contexts) per second even at one lane. Moving
  the budget's loom models into the kernels crate broke O1's patch and was reverted.
- **The mid merge** applied I9, P9.1a, P9.1b, and P9.2 (74 files, no conflicts) as P9.5's base, so ADP could integrate
  the merged `saluki-core` before the P9.3 parts and P9.4 landed in Z1.
- **P9.4, the semantics oracles** (16 findings). Candidate mode first recorded input kinds, which never match
  aggregation's output (a counter emits as a rate). The seeded schedule first delivered too little per event, so nearly
  every case was excluded. A claim that one fixture always failed came from a stale proptest regression file. In
  review, the kind-conflict rule accepted total loss, and generated traces never ticked before the last reset. Its
  oracles found six bugs in P9.3b, among them the late lane that recreated an epoch's record on another timeline
  (amendment 149) and a reset overflow during a cut that freed an instance still holding data (150).
- **P9.5, ADP and DogStatsD** (5 major, 5 minor). It found the plan's memory table about six times too low (one more
  producer lane adds 9.89 MiB, mostly the tag filterlist's memo), which pulled M11 forward. `spawn_blocking` lost
  thread-local recorder handles, so builds use `block_in_place`. Review majors: kernel credits were admitted once per
  slot rather than per lane (6.5 MB admitted where about 105 MB would be built at D = 16); the stress canary was a
  timestamped counter that bypassed aggregation; 6.6's matrices and most 6.8 fault tests did not run on lanes.
- **P9.6, the demo** (2 major, 6 minor). A one-context canary covered one state lane, so canaries became 16 shards.
  The whole-run canary range would have exceeded the 300 s history. S12b could not run at all, since the base still
  refused a second state lane per isolation slot. The cut acknowledgment timeout of 40 s was too short (cuts took 24 s
  to acknowledge, plus the windows). The canary loss surfaced here; see the investigations.
- **O1, budget reservations** (3 major, 3 minor). It was not neutral at one lane at first: two state owners refused
  each other while the global budget had room. M4 had been measured at batch size 1,024 where P = 8 routes 256. After
  the fixes, warm reservations cost about 5% on aggregation at P = 8 and 16, which kept P capped at 4. Credit cells
  were not built, and a larger block was rejected, since it changes little at routed sizes and quadruples the refusal
  bound.
- **O4 and O5, ingest** (2 major, 5 minor). A shared lock-free queue and per-decoder queues were alike within 3% at
  D = 4 and 8; the shared one led at D = 14 (5.91 against 4.20 M/s) and was kept provisionally. Review: `ArrayQueue`
  allocated 4.25 MiB uncounted at start (130 MiB at 1M buffers), replaced by a bounded `SegQueue`. CPU per metric
  regressed 20 to 55% at one metric per datagram, from a wake-up per push. One pending wake-up fixed CPU but capped
  throughput near 2.3 M/s, because a woken decoder sat in the reader thread's LIFO slot, which no other thread can
  steal. The cooperative budget recovered D = 2 only. A backlog boost (one more wake-up once 8 KiB is queued) finished
  it. Final M2 on macOS at 50 per datagram: 2.50, 3.16, 4.43, 4.90 M/s at D = 1, 2, 4, 8, from 2.65, 2.95, 1.61, 1.65.

### Stage 9 integration: Z1, Z2, Z3

**Z1** merged every part at one lane (157 files). The conflicts were mechanical, except O4's datagram queue against
P9.5's lane outputs in the DogStatsD source. O1's adapter patch was not applied, since P9.3b's fix already held it. One
test broke: the firm-bound pin at W = 32 and 64 did not count O5's front caches. Z1 closed 18 handed-on items, among
them the rule that a lane ticks nothing while its reset window is open (147), records kept until every lane has passed
their epoch (149), the debug-log tap gate (158), and the stress run's capacity and routing checks. Two P9.5 tests were
defective. A parity test compared a configuration whose context limit binds, which made it fail in half the processes,
and a panic test fed its later contexts in the same second, so they joined the buffer that panicked. S12b passed for the
first time. A new failing seed of stage 7's resource-table property also fails at `b523d9002b`, so it is not stage 9's
([known limitations](programmable-metrics-status.md#known-limitations-and-risks)).

**Z2** enabled the increments one at a time
([8.5](programmable-metrics-stages-7-and-9.md#the-increments-as-enabled-i9-z),
[8.6](programmable-metrics-stages-7-and-9.md#results-of-the-increments-stage-9-merged)). Two changes came of it. At
W = 2, two producer lanes cost a tenth more CPU per metric and scaled at 0.86 to 0.93 of legacy, so `auto` takes one
producer lane below three workers. The first version capped D at W - 1, which also lowered explicit decoder counts and
broke four lane tests, so it was narrowed to what was measured. Separately, the container harness's context limit was
raised tenfold. Two bugs came up on the way: every dropped context resolver leaked a telemetry task and its interner,
D times per live change; and the gateways profile could not run on lanes, from a duplicate `experimental` key that
concatenated configuration text produced.

**Z3** decided P9.7 by an in-process rule, since no Linux gate host existed
([P9.7](programmable-metrics-stages-7-and-9.md#p97-r1s-cost-model)). It kept R1 because the defaults profile ran 3
to 10% behind `lanes.mode: off` at W = 2, and wrote the documents and the stage 9 release note. The review then showed
the reading was wrong (below).

### Investigations

**The canary loss.** P9.6 lost canaries in 2 of 18 first launches (55 and 110), right after S10a, with no warning or
drop counter. A poller showed a different handful of shards missing each second, which pointed to datagrams lost before
aggregation. A transition loss at S10a was the first suspect. Baselines with a state dumper passed 22 of 22 on stage 7
and 21 of 21 on P9.6, and an S10a stress driver ran 64 cycles over six ADP processes without loss, so no bisection was
needed. Timing found the cause instead: each failure ended within 1 to 2 s of another agent's M2 run at one metric per
datagram, about 340,000 datagrams a second over loopback UDP. During a flood, datagrams sent minus those `dsd_in`
received grew by as many as the canaries missing (+40 against 43, +42 against 39). A 40-line loopback probe lost 158 of
801 datagrams in two floods and none otherwise, and `netstat`'s "dropped due to full socket buffers" grew by about 5
million per flood. No stage lost data. The fix is in the demo only: a loopback witness sends a control datagram of the
same size to the demo's own socket, and a canary range counts as host-dropped only if the witness lost datagrams around
it and the source's receipts account for the shortfall. Interpolating the source counter at a range's ends failed under
load, because a scrape's time is not its count's time, so the check waits for scrapes that bracket the range. A/B runs
under floods: the old check failed (40 and 28 missing), the new one passed. Limit: the source counter is an upper bound,
so an ADP loss during a host flood could be masked.

**The memory at N = 2.** The first container runs showed the plan's peak at two cores at 190 to 237 MiB against
legacy's 107 to 155. A sidecar sampled the cgroup, the live bytes per resource group, and the datagram pool every
second; `jemalloc`'s own statistics could not be read under load. Every group matched within about 1 MiB except
`dsd_in`, which holds the elastic datagram pool (up to 32,768 buffers of 8 KiB). The plan's pool held a median 9,100
to 10,100 buffers against legacy's 3,800 to 5,900. The cause: the debug-log tap's clone is made while the dispatcher
holds the admission lock (33 µs per buffer, p99 74), so both decoders stalled while the reader read ahead. With the tap
off, the pools matched (peaks 121 and 134 against 103 and 125 MiB). The tap discards everything by default, so Z1 gated
it on `dogstatsd_metrics_stats_enable` (amendment 158), and producer lanes take the lock off the decoders' path
anyway. Legacy fills the pool too: at N = 4 it sat at its cap, 384 MiB peak. Capped at 512 buffers, both peak at 63
to 64 MiB. Each buffer is also rounded up to the 10,240-byte size class of `jemalloc`, so the firm bound (258 MiB)
understates the resident worst case (about 330 MiB). The pool's ceiling is the owner's decision.

**The allocator setting.** `.cargo/config.toml` compiles `narenas:1,tcache:false` into every Linux build (#378, for
low RSS on the 4-CPU targets of SMP), the test binaries included, so every allocation takes one arena's lock:

| ns per allocation and free | 1 thread | 2 | 4 | 8 | 16 |
|---|---|---|---|---|---|
| As compiled | 31.6 | 512 | 1,496 | 3,648 | 7,015 |
| `tcache:true`, one shared resource group | 7.1 | 33 | 127 | 289 | 601 |

With a resource group per thread, `tcache:true` costs 7.5 to 14.7 ns, so the shared group counters of the tracking
allocator are the next choke point. `narenas:4` alone was worse (152 ns at one thread). In ADP at N = 2, the thread
cache gave legacy 7.1% more throughput and the plan 2.8%, for about 8 MiB more peak (11 to 13 MiB at N = 4). The
recommendation, still open: measure `MALLOC_CONF=tcache:true` with per-decoder groups on the gate host and in the RSS
gates of SMP ([risk 32](programmable-metrics-stages-7-and-9.md#11-risks)).

### The stage 9 review and fixes

Four lenses (semantics, performance, operability, tests) found 23 problems, all real: 0 blockers, 4 majors, 19
minors. The fixes are amendments 163 to 167 and the "Review hygiene" row of [Stage 9 as
built](programmable-metrics-stages-7-and-9.md#stage-9-as-built).

- Major: an idle producer lane held every flush back by up to its heartbeat period (950 ms in a probe, 875 ms end to
  end), now at most the progress throttle (166).
- Major: Z3 read the W = 2 gap as R1's pipelining, but R1 cuts no slot in the defaults profile. The two layouts there
  have the same slots and fusion, so the gap was the lane runtime at one lane per slot.
- Major: so `auto` below three workers had run that runtime for nothing; it now runs the layout without lanes where
  every count is one (167).
- Major: the front cache test failed in 20 of 200 processes, by hash seed.
- Semantics minors: Z1's record keeping still lost a late lane's epoch (83 flushes against 82), and a second reset's
  successor took its schedule from the oldest open epoch (163, 164).
- M6 rerun: about 10 µs of the per-buffer cost of lanes is idle workers waking and parking around one serial lane.
- A W = 16 figure cited in the documents had never been measured, so the caps' decision logs now say provisional.
- An over-broad `Strict` fix broke a fit test in the fixes gate and was narrowed.

### CI jobs and `make` targets

Agents added CI jobs on their own; none was asked for. All are in `.gitlab/test.yml`:

- `measurement-tests`, committed in `b7e63f147b`. P9.0's review found that workspace builds had always compiled the
  `measurement` feature, so the tests got `make test-measurement` and a job. The stage 7 base dropped the job on the
  instruction of the orchestrating session; the stage 7 review put it back (its finding 15: the tests ran nowhere).
- `integration-test-targets-linux-amd64`, committed in `b7e63f147b`, with `make test-integration-targets`. The stage 7
  review (finding 16) found that I7's `tests/worker_resource_group.rs` never ran, since `make test` skips `tests/`
  targets.
- `stress-tests`, committed in `8b44c153aa`, with `make test-stress`. The stage 9 review (finding 22) found that the
  stress run of [6.5](programmable-metrics-stages-7-and-9.md#65-concurrency-loom-models-and-a-multi-thread-stress-run)
  (30 s) ran only in the manual gate.

The three targets are also in `make test-all`. `make test-loom` grew from two crates to five: the kernels crate and
the plan crate (P9.3b), `saluki-components` (O4), joined into one line in Z1. No owner decision on the jobs is
recorded. `make check-deny` failed in every gate on RUSTSEC-2024-0449; no exception was added, since that is the
owner's call.

### Decisions and their reasons

- **R1 kept, off whenever lanes are on.** No gate host existed, and the in-process rule did not show lanes matching R1
  everywhere. After the review, the remaining evidence is the mapper profile, equal at one and two workers; deleting
  R1 waits for the gate host at W = 1 to 3.
- **P capped at 4.** At W = 8 the sweep over P gained nothing past 4, warm reservations cost about 5% at P = 8, and
  skew grows at 8. M = 8 and D = 16 are provisional, since nothing ran above W = 8.
- **The layout without lanes where every count is one.** The lane runtime at one lane per slot costs 3 to 10% and buys
  no parallelism. The owner can revert it with a release note.
- **`lanes.mode: auto` by default** before the gate ran: chosen on macOS and the VM, with CPU per metric on lanes at
  up to 1.30 of legacy paced against the gate's 1.07
  ([decisions](programmable-metrics-status.md#decisions-awaiting-the-owner)).
- **Kept per lane, or not built:** memos per lane (M11: 0.886 against 0.887 hit rate shared); no clocks per lane
  (M10: 19 to 47 ns of wait per stamp); no credit cells (O1 sufficed); O4's shared queue, provisionally; CPU-time
  sampling 1 in 16 on Linux.

### How the work went

- The design took about four hours, from the surveys to the second review's commit. The judges disagreed on the base,
  but merging their verdicts left few open choices.
- Review rounds found most problems in what the previous fixes had added. Their tables of interface changes let I7
  absorb C1 to C9 while it ran.
- Every part's review found real defects; most reviews proved them with probes written outside the tree. Mutation
  checks caught weak tests repeatedly. Several first tests could not fail, and P9.4's oracles found six bugs in P9.3b.
- Parallel work trees filled the disk, which broke the ADP delivery test with false failures: above
  `forwarder_storage_max_disk_ratio` (0.8) the forwarder drops rather than persists. Mutation scripts once restored
  files with an older modification time, so a run used a stale build.
- Never run a UDP benchmark beside the demo check ([environment
  notes](programmable-metrics-status.md#environment-notes)).
- A `preview_start` in two parts started the main tree's demo for about 9 s and rewrote a run directory's
  configuration.

### Left open

- The gate's items 1 to 4 and 6 on a Linux host with at least 20 homogeneous cores (Q2, Q7), with M2's rerun for O4,
  M4 at P = 8, the allocator setting, and the per-buffer cost of lanes (risk 31) at the automatic counts.
- R1's deletion; O6 (triggered by M8); O7, O9, O10 unevaluated.
- Stage 7's deferrals: static mode not folded into the host, growth in place, structured prepare failures, a declared
  directory named after its recipient, platform roots loaded once, 4.13's bound validation, A18 and A19
  ([deferred work](programmable-metrics-status.md#deferred-work-by-stage)).
- Not run: Panoramic against a `main` image, `make test-integration`, the demo's UI in a real browser 1,280 pixels wide,
  and 6.11's routing-group reclamation cycle and non-metrics delivery during a cut.
- Not in the documents, found by the parts: a credential written into an endpoint's path is not redacted; a plain
  `http` request through an `http` proxy carries no proxy credentials natively, so a password rotation shows only for
  `https` targets; a state owner fed by a point and by another owner's time-driven output passes the frame checks at
  one lane (`state_upstream` counts only points), left for stage 10.

## After stage 9: deployment context

On 2026-10-13 the ADP team described how ADP is deployed: no production container has a CPU limit above 1.2 cores,
most are at 0.3, and Nodeless guarantees 50 millicores, with the largest bursts arriving during CPU crunches. That
changed what the work should optimize: efficiency per metric and worst-case behavior matter most, stage 9's multi-core
lanes matter little (and are off at those sizes), R1's cost model adds hops for nothing at one core, and the serial, blocking
aggregation flush is the biggest practical problem. The consequences and the next chunk of work are in
[deployment context and next steps](programmable-metrics-next.md).

