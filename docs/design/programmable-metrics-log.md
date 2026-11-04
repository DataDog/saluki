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

## N1a: measuring at production CPU limits

On 2026-10-13 and 14, N1 of [the next steps](programmable-metrics-next.md#n1-measure-in-the-production-regime)
measured the plan against current `main` at 0.05, 0.3, 0.6, and 1.2 CPUs under the steady loads of the SMP tiers. The
results and findings are in [N1a](programmable-metrics-next.md#n1a-steady-load-against-the-smp-tiers), and the gates
for N2 to N4 in [gates](programmable-metrics-next.md#gates-for-n2-to-n4); the per-run tables are in
`test/scaling/baselines/n1-2026-10-14/`. It was the first of two N1 measurements; the second, N1b, measured bursts
across a flush a week later ([below](#n1b-bursts-across-a-flush)).

### What was built

`test/scaling/run-limits.sh`, with `compose.limits.yaml` over the existing `compose.yaml` and `summarize_limits.py`
([README](../../test/scaling/README.md#runs-at-production-cpu-limits-run-limitssh)), and `make limits-images` and
`make limits-runs`. Unlike `run-curves.sh`, which ties the CPU count, the pinned CPU set, and the worker count to one
integer, it gives ADP a fractional quota on four fixed cores and removes every override (`SALUKI_WORKER_THREADS`,
`DD_DOGSTATSD_WORKERS_COUNT`, `DD_METRICS_LEVEL`, the context limit, CPU-time sampling), so ADP derives its workers and
decoders from the quota as production does: `available_parallelism` follows the cgroup's `cpu.max`, so every limit
up to two cores gives two workers and two decoders, and the plan runs without lanes and with R1's pipelining.

### Decisions

- **Steady loads, not bursts.** The owner decided against modeling bursts: a limit can absorb a burst up to the steady
  load it sustains, which saturate runs measure. The loads are the four SMP quality-gate tiers
  (`quality_gates_rss_dsd_{low,medium,heavy,ultraheavy}`), with their rates, contexts, interner sizes, and memory
  bounds.
- **Current `main` as native**, not the branch's merge base: `main` was six commits ahead, none touching the measured
  path.
- **Production defaults**, including the `info` metrics level. The telemetry needed is all at `info`: the decoded
  counter, aggregation's drops, failed context resolves, and the `dsd_packet_bufs` pool gauges.
- **Local runs only**; CI is deferred. p99 latency is not measured in containers.

### What went wrong, and what was learned

- **Measurement inside the quota.** `run-curves.sh` reads `cpu.stat` with `docker exec`, which runs inside ADP's cgroup
  and is charged to its quota: negligible at four cores, not at 50 millicores. `run-limits.sh` reads the cgroup files
  from the host (`/sys/fs/cgroup/docker/<id>` under Docker's cgroupfs driver) every second, and ADP's own telemetry
  only every five seconds.
- **No drop counter.** A Unix datagram socket blocks the sender rather than drop, so falling behind shows as lading
  sending less than its rate. "Keeps up" is lading at 97% of the rate or more.
- **Stalls shorter than the sampling.** The first stall measure, lading's slowest second, missed flush stalls under a
  second (corrected later: lading's own one-second pacing confounds every per-second measure; see "What stalls
  admission at a flush" below). Lading catches up after each, so its fastest second (1.25 to 1.44 times the rate where both keep up at the
  heavy tiers) was added as the signal. At the low tier, lading's block size makes per-second rates too lumpy for
  either measure to mean much.
- **The first smoke run died silently**: ADP logs JSON here (`"git_hash":"..."`), the grep expected the text format,
  and `pipefail` with `set -e` ended the script with its containers still running. The script now tolerates a missing
  hash and tears down on exit.
- **An overstated finding.** Midway, one series' medians suggested the plan decoded 1.7 times less than `main` at heavy
  0.05. Across all 21 runs at that cell (A/A, paced, saturate) the gap is about 20% with overlapping ranges: the
  collapse cells' noise is 19 to 25%, and conclusions there need every series. The ultraheavy freeze held up.
- **Runs take about a minute longer than their 90 seconds** of warm-up and window (start, health check, lading's block
  cache, teardown): the 192 runs took about nine hours.

### The health check, and the matrix again

Investigating the "freeze" showed two things. First, it was not plan-specific: rerunning the cell, `main` decoded 50
and 191 metrics a second and the plan 315 and 156, and in one run it was `main` whose decoding stopped and whose
aggregation never flushed; only about 18,000 contexts are active at that rate, so the aggregation drain the hypothesis
blamed costs about 4 ms. Second, an idle ADP measured 17 to 22 millicores, but its threads used 3: the rest was
`compose.yaml`'s health check, `test -S` run inside ADP's container every second, each check a process charged to
ADP's quota. Every N1 run had carried about 20 millicores of it, 40% of the quota at 0.05. With checks every second only
while starting (`start_interval`) and hourly after, idle ADP costs 2.3 to 2.8 millicores, and ADP still becomes
healthy in two to three seconds.

The fix is in `compose.yaml`, which `run-curves.sh` shares, so the stage 9 curves had carried the same overhead, a
smaller share at their larger CPU counts. The whole matrix ran again (2026-10-14), and its results replaced the first
run's in [N1a's results](programmable-metrics-next.md#n1a-results) and in `test/scaling/baselines/n1-2026-10-14/`.
Ratios from 0.3 up barely moved; the 0.05 figures and the "fixed cost dominates at small loads" finding (34 millicores
at the low tier, really about 15) did. With the full quota, the plan and `main` are the same within the noise at the
0.05 collapse cells, both erratic.

**Lessons.** Check what the measurement itself costs before trusting a small-quota figure: anything run inside the
container, a health check or `docker exec`, is charged to the quota being measured. And a striking result in the noisiest
cells needs every series behind it before it becomes a finding.

### Left open

- Absolute figures on x86.

## First iterations after N1

On 2026-10-14 the quick loop of [how we iterate](programmable-metrics-next.md#how-we-iterate) was built, validated
against the N1a baseline, and used for the first hypotheses; results are in
[what the first iterations showed](programmable-metrics-next.md#what-the-first-iterations-showed), the findings
independent of the plan in [ADP efficiency findings](adp-efficiency-findings.md), and the tables in
`test/scaling/baselines/iterations-2026-10-14/`.

### What was built

`run-limits.sh` gained named cells (`SCALING_CELLS`), variants of an image with their own environment
(`SCALING_VARIANTS`, through an `env_file` in `compose.limits.yaml`), cells in parallel on disjoint cores
(`SCALING_PARALLEL`), `perf stat` counts in ADP's cgroup (`SCALING_PERF`), and instruction-sampled profiles with call
stacks (`SCALING_RECORD`); `summarize_limits.py` reports instructions, IPC, refills, and back-end stalls per metric and
ratios against a reference variant. The agent-data-plane binary gained an experimental switch,
`SALUKI_EXPERIMENT_FUSE_ALL=1` (removed after the iterations, once H2 was answered; it is in `7c7631a4bb`), which lays
the frame out without R1's pipelining while lanes stay off, so that fusion
can be measured apart from the lane runtime (`lanes.rs`; documented as an experiment, not a setting); the boot log
now reports the pipelining the frame actually uses.

### Decisions

- **Instruction counts as the quick loop's measure.** This VM exposes three hardware counters: instructions and cycles
  are pinned (exact), and L1D and L2 refills, back-end stalls, and TLB walks share the third (scaled estimates).
- **Every arm of a hypothesis runs the same binary**, differing only in environment, so that no difference comes from
  a rebuild's code layout.
- **The owner's direction after the results:** record every impactful finding, but separate those that could land in
  ADP on their own (the new document) from the plan work, and focus the plan work on behavior the plan can improve
  structurally, starting with flush stalls (N2).

### What went wrong, and what was learned

- **Editing a script while it runs.** The quick-loop changes were first about to go into `run-limits.sh`,
  `compose.limits.yaml`, and `summarize_limits.py` while the overnight baseline was executing them: bash reads a
  script as it runs, and each of the baseline's three series started the script afresh. They were staged as `*.next`
  files and swapped in by the chain that ran after the baseline.
- **`pkill -f` matched its own shell.** A pattern given to `pkill -f` also appears in the command line of the shell
  running it; a bracket (`'[b]ash target/...'`) keeps it from matching itself.
- **The quick loop is not valid for CPU time.** Its A/A agreed within 0.3% in instructions, but against the sequential
  baseline its CPU per metric missed by up to 4% at two cells, and its memory by up to 9% even as an A/A.
  The cause, parallel slots or two-core CPU sets, is not yet separated.
- **The plan's unclassified profile samples were shared code:** newline framing (`memchr`), buffer copies, and
  `recvmsg`, not plan overhead. The plan's own runtime is 5 to 6% of instructions.

### Left open

- Separating the quick loop's CPU-time skew (one slot with two cores, against parallel slots).
- A policy-heavy workload (the `mapper` and `gateways` profiles, checked against what production configurations use),
  before H3 and H4.
- N3's effect on stalls and CPU time, in sequential runs.
- The independent findings G1 to G8, for whoever takes them up.

## After stage 9: one scope over the whole metrics path

On 2026-10-14 the owner asked for one editable program covering the whole metrics path, with at most one aggregation
for now. The default program's two scopes, `dogstatsd_pre_aggregation` (DogStatsD up to aggregation, ending at exits)
and `post_enrich` (the policy region after host tags), became one, `metrics`: one input per metrics source the
configuration enables, named after the source's binding and declared with the port type the source produces, and no
exits. Amendments 168 and 169 in [the stages 7 and 9 plan](programmable-metrics-stages-7-and-9.md#10-proposed-spec-amendments)
record it.

- **A spike came first.** Tests of the default-program fixture re-scoped to one scope (now
  `install::unified_scope_tests`) showed that the unmodified export composes back to the same plan and places as the
  boot plan does; that a routing edit causes no cut and a width edit does; and that an artifact that changes both
  installs as one generation whose routing takes effect at the cut's boundary, for every source. No model change was
  needed: the frame, placement, the cut protocol, and scope closure are as they were.
- **Placement does not enforce one aggregation.** A second `aggregate` on the timestamp split's passthrough branch
  places into the one isolation slot, which runs a state owner of each branch by design. So the validator has a new
  rule, `single_aggregation`, which a host turns on through its catalog (`Catalog::single_aggregation`, the
  document's `single_aggregation: true`), as ADP's and the fixtures' catalogs do. It is an error at each extra state
  owner's node, the first being the default program's (the first without artifact provenance). It is the catalog's
  choice rather than a rule for every plan because the plan crate's tests of several state owners (transition traces
  12 and 13, budgets shared by owners, output stamps per lane) keep exercising what the runtime supports.
- **The native-authority rule reaches further.** A width edit in the one scope gives every lossy node fed by the
  timestamp split or aggregation the artifact's provenance, eight nodes in the fixture down to the routing gateways'
  drops; the pre-aggregation scope's artifact stopped at its exits, at three. Accepted and recorded as a behavior change
  (loss attribution, `explain`, the plan diff).
- **Printed exports reorder edges.** The syntax prints the edges from a scope's inputs first, in input-name order, and
  each node's edges in its block, while an export lists them in the plan's order, and the plan's hash covers edge
  order. With one input this never showed; with five, a printed and parsed export composed to a plan equal to the
  default program but for edge order, so the installer would have made a new generation for the demo's unmodified
  program (S0). A first fix put the export's order back in the demo's editor; the review moved it into composition:
  `compose_all` puts an artifact's edges in the default program's order before splicing them in (an edge the region
  has takes that edge's place, a new edge follows the region's edge listed before it), so any tool's printed export
  composes to the default program with its hash. No fixture hash changed. The artifact document's own hash still
  covers the order it was written in. ADP's export test and the syntax crate's fixture tests compose the printed
  export and compare plan hashes, and a plan-crate test composes shuffled artifacts.
- **The first aggregation.** The rule takes as the first state owner the first without artifact provenance, then the
  first with a reserved ID, so an aggregation an artifact adds ahead of a widened `dsd_agg` (which a width change
  gives the artifact's provenance) is the one reported.
- **Tests that are about partial scopes keep them.** The plan crate's composition, export, and installer tests of a
  scope that ends at exits and of artifacts of several scopes run on the fixture with its two former scopes
  (`testing::with_two_scopes`, `two_scope_catalog`), since the plan crate still supports both. The new export golden
  is `fixtures/export/default-program-metrics.json`, derived from the fixture with a script, its hash computed with
  Python as the others are.
- **The demo edits the whole program.** It loads the `metrics` export, prints it with a `#` comment where each section
  begins (sources, DogStatsD processing, aggregation, after aggregation, enrichment, policy; the canonical order was
  already source to policy), and drops the pre-aggregation scope's special cases (its export fetch, the `?scope=`
  query, the scope revoke). Policy edits go in at the edge into the policy `tee`. S5d is now a second aggregation,
  refused by `single_aggregation`, which the editor flags at the line; S5e, the aggregation moved after enrichment,
  keeps a placement refusal. S13's width change and revoke use the one scope, before the routing steps.
- **Release notes.** None of the programmable-metrics notes has shipped, so the stage 5 and stage 6 notes were edited
  in place (the `metrics` scope and route, at most one aggregation) rather than adding a note.
- **Results.** `make demo-programmable-metrics-check` passed on its first run
  (`target/programmable-metrics-demo/check-1792000608`): the first launch in 151.1 s, the second in 313.1 s, every
  canary balanced (2,550 and 5,395 sent and received). S13a's cut took 28.8 s to acknowledge and 65.7 s to show. After
  the review's fixes (edge order in composition, the export reader's `single_aggregation`, the first aggregation's tie
  break), it passed again (`check-1792004710`): 150.2 s and 291.5 s, every canary balanced (2,507 and 4,997), S0 with no
  new generation without the editor's reordering.
  `delivery_tests::a_recipient_whose_intake_stays_down_retires_at_its_deadline_and_persists_what_it_holds` failed on
  this machine with the disk at 88%, above `forwarder_storage_max_disk_ratio` (0.8), as it did before (see above); it
  is unrelated to the change.

## After stage 9: programs replace native policy

On 2026-10-14, after the one-scope change, the owner set the long-term direction: the programmable substrate replaces
ADP's native metrics implementation and its policy configuration, rather than becoming a control surface over native
features. A program's inputs are resources (privileged, referenced by name), facts and signals (bindings), and policy
settings, which a published program owns as literals. A published program detaches from native policy settings, with
no merging; native policy configuration only feeds the default program, which runs when no artifact is published, and
status says when native policy settings are not applied. Machinery for native compatibility belongs in lowering. The
[direction](programmable-metrics-next.md#direction-programs-replace-native-policy) records the decision.

- **What it explained.** Much of what a user reads in the demo exists only for native compatibility: the hash-keyed
  routing group bindings (`metrics_endpoint_routing.group.<16 hex>.names`), outputs shaped by native routing
  (`metrics_routing_encode_<n>`, whose main output sends to every endpoint without a routing policy), native authority
  bookkeeping in composition, version 1 fingerprints, and base staleness.
- **The agreed path** is four steps: a detached export for editing; readable recipient names and emits addressed to
  recipients; authored programs that bind only facts and signals; and one aggregation that keeps its state whatever its
  node is called. A design round wrote [detached programs](programmable-metrics-detached-programs.md), with amendments
  170 to 182 in [the stages 7 and 9 plan](programmable-metrics-stages-7-and-9.md#10-proposed-spec-amendments). Its main
  recommendation for the demo is to declare teams B and C as recipients in configuration, so that the demo shows a
  program routing, while the equivalence of native routing stays proven by tests and fixtures.

## Detached programs, phase 1

On 2026-10-14 the first wave of [detached programs](programmable-metrics-detached-programs.md) was built in the main
checkout, not committed: parts A (binding classes, the facts `host.tags` and `host.tags_deadline`, and the signal
`autoscaling_failover.enabled`), C (the recipient directory, without configuration-declared recipients), F (one
aggregation keeps `dsd_agg`'s identity), G1 (the export's annotations, and the editor's hidden base), and H (version 1
fingerprints removed). [Phase 1 as built](programmable-metrics-detached-programs.md#105-phase-1-as-built) has the
details; the design's section 10.2 now records the composition pass order that B and D share.

- **Owner decisions during the phase.** Additional endpoints are named by their URL, standing for every API key's
  recipient of the endpoint, in place of the designed `ep_<host>[_<port>][_key<i>]` names and their collision
  rule (amendment 176 revised). Configuration-declared recipients (C2) wait on the owner's choice between a new key and
  seeding a resources artifact at the demo's launch; resources validation already refuses a name the directory gives
  a recipient that configuration declares, so either feeds the directory.
- **Simpler than designed.** H has no dedicated refusal: a version 1 fingerprint is an output that delivers elsewhere
  (`ReboundOutput`, or degraded in effect). Recipient notes are not annotations: the editor prints the directory. The
  `cluster_agent` recipient's availability apart from the branch is a directory entry only, since a declared native
  recipient runs a loop whether or not an output names it.
- **Found on the way.** R7's static order puts a kind test before a flag, so the autoscaling failover gateway's new
  flag goes second, or R7 would rewrite the default program. The designed implied output IDs (`to_a__b`) are not
  identifiers, and URL names cannot form IDs: D derives them otherwise.
- **Hashes.** The default-program fixtures, their exports (`default-program-metrics.json` regenerated with the Python
  script used before, `default-program-post-enrich.json` edited and rehashed: `sha256:8b26675f...`), the explain golden
  files, ADP's `every-branch.json`, and the demo's export fixture (annotations and the directory added) changed; the
  pre-aggregation export did not.
- **Review fixes.** A review found no blockers, and these were fixed before commit: names of additional endpoints
  configured without a scheme (`mrf`, `primary`, `team_d`) took, or merged with, `mrf`, `primary`, and declared names,
  so an endpoint is now named by its URL as the forwarder resolves it, with its scheme, and lowering fails rather than
  merge two hosts or sources; the export readers left a whole export unreadable for an unknown member or source of a
  directory entry; formatting dropped a base the source stated; a routing filter's note could carry an endpoint's user
  information; some notes overstated what their nodes do; the demo's export fixture used recipient IDs ADP never
  writes; and only ADP reserved `primary`, `mrf`, and `cluster_agent`. New tests pin each.
- **Two installer core bugs**, older than this phase, were found by
  `property_test_the_high_water_mark_never_decreases_and_only_accepted_plans_run` at more cases: a damaged twin of a
  config's ID in the previous snapshot hid the unchanged config from the rule of one config per scope, so another
  config of the scope installed beside it; and a kept generation that supervision failed did not take its document out
  of effect, so a newer revision of it in flight installed. Both are fixed, each with a regression test and a seed;
  100,000 cases then passed.
- **Results**, after the review fixes. The plan, syntax, and demo crates' tests pass with and without
  `saluki-metric-plan/test-util` (1,166 each), and ADP's 576 of 577, the one failure being
  `delivery_tests::a_recipient_whose_intake_stays_down_retires_at_its_deadline_and_persists_what_it_holds` with the disk
  at 82%, above `forwarder_storage_max_disk_ratio`, as before. `make demo-programmable-metrics-check` passed
  (`target/programmable-metrics-demo/check-1792017162`): the first launch in 150.5 s, the second in 290.3 s, every
  canary balanced (2,848 and 4,997 sent and received), S0 active with no new generation.

## Detached programs, phase 2

On 2026-10-14, after phase 1 was committed (`cfd4a28072`), phase 2 was built in the main checkout, not committed:
parts B (the detached export, composition by value, and `program` in status), D (emits addressed to recipients, demand,
and implied outputs), and E (facts and signals only), with the owner's decisions made after the design. [Phase 2 as
built](programmable-metrics-detached-programs.md#106-phase-2-as-built) has the details; amendments 183 to 188 record
the changes to the model.

- **Owner decisions before the phase.** Recipients are `primary`, `mrf`, `cluster_agent`, and each additional
  endpoint's URL, and native `additional_endpoints` declare destinations (no configuration key for recipients, so C2 is
  dropped). Node provenance goes entirely: the program a generation runs decides panics, and loss is attributed to its
  node and the program; `trusted_host` becomes an operator property. Reuse is decided by what nodes do, so a published
  program equal to the running plan adopts its generation. Standalone mode gains an opt-in configuration reload, for the
  demo only.
- **What removing provenance took.** Besides the authority rules and composition's feed analysis, provenance carried
  which native component each node stands for, which R1's cost model and ADP's region input capacity read at boot:
  it moved to a plan-level `native_components` annotation that composition does not carry and the hash leaves out.
  R5a and R6 keep native filters whole by their reserved IDs. A scope holding a host component cannot be replaced
  even unmodified, which only the plan crate's opaque-gateway fixtures have. Every plan hash changed once.
- **Adoption** keeps the generation ID: the core records the origin and the composed plan, and regions are told the
  program's panic budget (`RegionControl::Adopt`). A new generation ID with no physical change was rejected: the ID
  names the installation.
- **Found on the way.** A program that stops dual-shipping (`emit to [primary]`) and routes to an additional endpoint
  (`emit to ["https://b..."]`) was refused as a duplicate contribution, since the duplicate analysis grouped outputs
  sharing a recipient transitively, through the main output it no longer emits to; it now pairs only outputs that share
  a recipient (amendment 187). The detached export's base is the bound export's, so composing it is no stale base.
  Implied outputs are added on `dd_out` only, for native recipients. The demo check's width change (S13a) looked for
  each `window` on a line of its own, while the detached export prints a split's only parameter inline
  (`timestamp_split(window: 10s)`); the edit now finds it either way.
- **The standalone reload** (`experimental.standalone_config_reload.{enabled, interval}`) was built by a subagent in
  parallel, in the configuration system and ADP's boot, with tests at both levels.
- **First results.** Before the review, the plan, syntax, and demo crates passed 1,177 tests, ADP 582 of 583 (only
  the disk-ratio drain test failing, at 84% full), and the demo check passed both launches in 7 minutes 20 seconds
  (`check-1792026960`), S0 adopting generation 1.
- **Review.** A review found three high-priority problems and several smaller ones, all fixed:
  - An artifact generation the default program adopted after its panic budget ran out, with the revocation processed
    before the failure report, was left failed with nothing to replace it. The core now marks a generation adopted
    from an artifact and asks for the default program when it fails.
  - A kept node that matched the default program's only by value composed as the default program's node, still
    bound, so a native policy change under a detached program made a generation that did nothing new. It now composes
    as the artifact writes it, and the core, the kernel keys, and the plan diff compare plans by what they do, with
    bindings written as values (the operation hash), so S0 still adopts and a native change makes no generation.
  - The artifact limits measured every node of a program's scope, so an unmodified export of a configuration beyond a
    limit, such as a 128 MiB mapper interner, could not be published. Each limit now rises to the default program's
    own measure in the scope (amendment 188).
  - Smaller fixes: the standalone reload applies a change once two reads agree, reads off the async runtime, raises an
    interval under 100 milliseconds, and warns in connected mode; implied output IDs carry a digest of their set, so a
    changed set never inherits an ID; the implied-output limit's refusal says it counts every program held; recipient
    names are escaped where they are quoted; the demo places a policy-binding error inside its node, and writes ADP's
    configuration file atomically; the status enums and the export form read unknown values.
- **Results.** The plan, syntax, and demo crates pass 1,127 tests with the plan crate's `test-util` (property tests
  apart) and 1,180 without it, property tests included; the 53 property tests pass in release. ADP passes 584 of 586:
  the disk-ratio drain test fails, at 86% full, and a telemetry test's flush count, which passes alone, failed once
  under the full run's load. The configuration crates pass 252. Clippy and the format check are clean. The demo
  check passed both launches in 7 minutes 14 seconds (`check-1792033049`: 153.7 s and 269.9 s), S0 adopting
  generation 1 with no new generation.

## Detached programs, phase 3: the demo

On 2026-10-14, after phase 2 was committed (`7f9d67a033`), phase 3, part G2, was built in the main checkout, not
committed, with the owner's decisions for the demo: ordinary `additional_endpoints` as destinations, no reserved
destination, no resources artifacts in the demo, and the standalone configuration reload on. [The demo as
built](programmable-metrics-detached-programs.md#85-the-demo-as-built) has the details, and the status document's
[demo section](programmable-metrics-status.md#the-demo) the recipe and the scenario table.

- **Configuration.** The launcher writes the primary endpoint, teams B and C as additional endpoints (dual shipping,
  no endpoint routing), the failover endpoint, and `experimental.standalone_config_reload` (`enabled: true`, `250ms`).
  The team D port, intake, and credential are gone.
- **Destinations.** A new module, `destinations.rs`, owns the intakes and the live part of the configuration file:
  **Add destination** binds a free port, starts an intake with its own key and label, and rewrites `datadog.yaml`
  atomically; **Remove** takes an additional endpoint out and stops its intake once ADP lists its recipient retired.
  The native settings the new card of the UI changes (the failover signal and the filterlist) are written the same way.
  The demo's status lists, for each intake, whether ADP's directory names it and whether the active plan addresses it.
- **Editor.** The recipient comments carry the demo's labels beside URL names, and count recipients an `emit to [...]`
  names as addressed (they counted output-addressed emits only, so a detached export marked every recipient "not
  addressed"). The export reader keeps the catalog's `detached_programs`, so the editor flags a policy binding as ADP
  does. Composition's waits (an implied output to add, an unknown recipient) are warnings, not errors, with no diff.
- **UI.** The Resources tab and its routes are gone; the destination columns follow the destination list, with
  **Remove** and, for added ones, **Pause**; the header names the program from `program` in status.
- **Check.** S8 to S11 and S14 were rewritten (see the table): destinations under the default and a published program,
  a removal that degrades the program routing to it, a native policy change under a program, and the renamed
  aggregation in the second launch. Four things the first runs found: a revoke can adopt (S6), a step after a program
  with implied outputs sees ADP lower twice, so the check compiles again against the plan ADP runs, the main output's
  unused-output warning is no finding, and the canary rule for other endpoints became: never more than were sent.
- **An installer core fix.** A program asking for implied outputs, rejected at validation, was never acknowledged
  `rejected`: ADP lowered again once its demand was gone, the artifact in effect moved to a new generation, and the
  core showed that move as the config's acknowledgment. The same held for a composition refusal followed by any
  configuration change. `refresh_acknowledgments` now keeps a newer delivery's refusal, rejection, or failure as the
  acknowledgment while the installation before it only changes generation; two lifecycle cases pin it.
- **Kept, by decision.** The demo's export fixture stays a bound export (the reader's and editor's tests use it); the
  composition tests of the scenario edits use the plan crate's default program with a detached export and a
  directory. No release note for `experimental.standalone_config_reload`: it changes only standalone mode, which is
  not for production, and the configuration reference documents it.
- **A property test's oracle.** A full run of the plan crate's tests found a new input for
  `property_test_edited_artifacts_compose_or_are_refused_cleanly`: an edge redirected away from `metrics_policy`, and
  a node made an `aggregate`, left part of the plan unreachable, which the oracle exempted only for removed edges. It
  now exempts any plan whose validation reports unreachable nodes; the seed is in the regressions file, and 20,000
  cases pass.
- **Results.** The demo, plan, and syntax crates pass 1,174 tests, ADP 586 of 586 (the disk at 71%, so the disk-ratio
  drain test passes). Clippy (`-D warnings`) and the format check are clean on the changed crates. The demo check
  passed twice, both launches each, after the code was final but for the acknowledgment fix's second form, which the
  second run includes: `check-1792038037`, first launch 156.8 s and second 289.4 s, 7 minutes 42 seconds with the
  build, canaries 2,792 and 4,999 sent and received; `check-1792038499`, 148.4 s and 285.4 s, 7 minutes 30 seconds,
  canaries 2,810 sent and 2,808 received, 4,797 and 4,798, balanced within the allowance. An interactive run
  (`make demo-programmable-metrics`) through the JSON API: a destination added under the default program received
  everything; the recipe narrowed team B, and the destination then read "declared, not addressed"; removed, its
  intake stopped; a rejected program with implied outputs stayed `rejected`; `SIGTERM` stopped the demo, ADP, and
  every intake, and no process or port was left.

### Phase 3 review fixes

A review of phase 3 found no blockers; its fixes, on 2026-10-15, not committed:

- **An `emit` delivers to the recipients that remain** (amendment 189, decided on the owner's behalf as least
  surprising). Removing team C under the recipe made the whole `emit to [primary, "...17103"]` a counted drop, so the
  primary endpoint went empty and the canary check turned red. Composition now makes such an `emit` of the artifact in
  effect a two-port `tee` to an `emit` of the remaining recipients and a counted drop, which counts what the gone ones
  would have received as policy loss; `demanded_outputs` asks for the remaining set. A first ADP test showed the
  primary endpoint losing batches between two lowerings (the old directory's demand, then the new one's): ADP now
  computes the demand through the new directory and lowers once more before proposing, so the remaining set's output
  exists in the generation that removes the recipient. The new ADP delivery test fails without that second lowering.
  The check's new S10b removes team C under the recipe: the primary endpoint's canaries stay exact.
- **The editor validates programs that need implied outputs**, against stand-ins (`adp_will_add_<n>`, bound and
  fingerprinted as ADP binds implied outputs), with a note per recipient set and a provisional diff: the recipe with a
  duplicate branch now shows ADP's `duplicate_contribution` before publishing. Recompiling the program ADP runs
  composes it in effect, so a degraded program reads degraded.
- **Amendment 190** generalizes the acknowledgment fix to refusals at composition.
- **The property test's oracle** now skips only the aggregations validation reports unreachable, for removed and
  redirected edges alike (80,000 release cases pass).
- **Nits and the video's UX:** the removal waits for a status sent after ADP applied the file; ports are not reused in
  a run; an intake listens before the file names it; the recipient comment labels and aligns every row; an unused
  native output replaced by implied outputs is a note; the waiting notes name labels, not node IDs; a note beside the
  editor names destinations the published program does not address; the header shows the generation only on hover;
  the canary line uses labels; the Remove confirmation says what happens. A restart with `--run-dir` leaves a kept
  program that names an added destination `pending`, which the smoke test confirmed.
- **Found and not fixed:** the duplicate analysis reports a duplicate for copies that reach, through two branches of a
  `tee`, two outputs with no recipient in common when an output on the other side of a `split` shares one with each.
- **Results.** The demo, plan, and syntax crates pass 1,175 tests (the demo's 152 again after the last change), ADP
  587 of 587, clippy (`-D warnings`) and the format check are clean, and the demo check passed twice:
  `p3r2-check-a`, 155.0 s and 292.6 s, 7 minutes 34 seconds, canaries 2,928 and 4,998 sent and received;
  `p3r2-check-b`, 175.0 s and 269.7 s, 7 minutes 30 seconds, canaries 3,147 sent and 3,145 received, and 4,597 and
  4,598, balanced within the allowance. In S10b team C's removal left the primary endpoint's rolling checks balanced.

## Detached programs: what does nothing, left out

On 2026-10-15, after phase 3 (`c9e27cea81`), the owner asked that the detached default program show only what
happens: an empty filterlist, a namespace prefix with no namespace, a mapper with no profiles, a tag filterlist with no
rules, a one-input merge, and host enrichment do nothing as configured. Built in the main checkout, not committed;
amendment 191, and [what does nothing, as built](programmable-metrics-detached-programs.md#107-what-does-nothing-as-built)
has the design.

- **Identity and constant splits.** `OperatorDef::is_identity` for six operators, each backed by a kernel property
  test; `Predicate::literal_value` for splits that literal operands decide, including string matches over a literal
  empty list, folded with R3's own obligations (`r3::fold_split`, split out of R3's `Site::find`; R3 unchanged).
- **One pass, two uses.** `export::eliminate` serves the detached export (`omitted` in the bundle, and ADP's note,
  `annotations.omitted`, printed at the top of the editor) and `export::operation_hash`. ADP's export test checks, for
  every configuration of the lowering matrix, that the export leaves out exactly what the comparison does and that
  the composed program has nothing left to leave out. A random-plan property test checks that leaving nodes out
  changes no output, or its order, with bound operands counted as literals or not.
- **Found on the way.**
  - The comparison needs the host's facts and signals, which the core did not know: `InstallerConfig` gained
    `facts_and_signals`, which ADP sets from its catalog. Without it, a failover flag (a fact) would fold.
  - Revoking a program whose generation was built from the shorter export would have adopted that generation for the
    default program, which would then run without its nodes and report none of their telemetry. The core now keeps
    two hashes per generation (`Does`): a published program is compared by what it does with every record, a default
    program node by node (`export::operation_hash_as_written`). The headless check's S6 and S11b now make a generation
    on revoke, which the check accepts.
  - `program.differs` listed every setting of a default-program node the running plan lacks, so a filterlist set
    natively under the shorter program would have listed its match mode too. It now lists the settings that, turned
    off alone, would make the node do nothing again, or all of them when none would.
  - Program memory limits measure the whole program, so leaving out the tag filterlist frees its context cache's
    declared memory for other nodes; ADP's interner-limit test grows the interner by 64 MiB instead of one byte.
  - The editor's preview diffed plans node by node, so S0 would have shown removed nodes: it now uses
    `install::diff_operations`.
  - The check's S5e bound the filterlist, which the demo's export no longer has; it binds the aggregation's
    percentiles. The S5d and S14 edits found the edge into the aggregation by `-> dsd_agg` on a line of its own; the
    timestamp split now feeds it from its `default` port line.
- **Telemetry, called out.** While a generation built from the shorter program runs, the left-out nodes report
  nothing. The Datadog Agent's `aggregator.filtered_tags_cache_miss`, which counts the tag filterlist memo's lookups
  even with no rules, stops counting; the other remapped series of those nodes are zero or constant then. The default
  program, and any generation that still runs the nodes, report them as before.
- **Results.** The plan, syntax, kernels, and demo crates pass 1,368 tests, with and without
  `saluki-metric-plan/test-util`; ADP 588 of 588; the release property tests of the plan, syntax, and kernels crates
  79 of 79. Clippy (`-D warnings`), `make check-fmt`, and Vale on the changed documents are clean. The demo check
  passed, both launches: `omit-check-a`, 154.7 s and 267.4 s, 7 minutes 56 seconds with the build, canaries 2,926 and
  4,600 sent and received. An interactive run (`omit-interactive-a`) served the shorter program with its note, which
  compiled with no findings and no change.

### What does nothing: review fixes

A review found no blockers; every identity claim checked out against the kernels. The owner then decided how the
shorter program installs, and the fixes followed, on 2026-10-15, not committed:

- **Installation compares as written** (the owner's choice of the review's option (b)). The core compares a candidate
  with the running generation's plan by the operation hash without leaving anything out (nodes and edges sorted), so
  publishing the shorter export over the default program is a generation that builds no kernel and makes no cut; the
  plan running is then the program as published, and `/explain`, telemetry, and the core's active plan agree. The
  two-hash `Does` and `InstallerConfig::facts_and_signals` are gone. Elimination serves only the export, the editor's
  `diff_operations`, and `policy_differences`, each with `export::policy_literals` of one catalog and the host's
  registry (`export_as` and `policy_differences` take it). The editor shows no change in what the program does, with
  a note that ADP drops what does nothing and builds no kernel for it.
- **Tests.** An installer test pins the shorter export as a commit that builds no kernel, aggregation `retain`, then
  an edit that changes only its slot, then the same program adopted; an ADP test does the same through ADP's own
  frame and core, and the artifact test checks deliveries under it. The random-plan generator gained empty-configuration
  mapper, prefix, tag projection, and host tags kernels, which the elimination property test now leaves out.
- **Telemetry.** A gauge the installer sets from a plan (`filterlist.size`, `tag_filterlist.size`, the endpoint
  allowlist sizes) reads zero for a node the plan running lacks, rather than keep a stale value. The documents add
  `metrics_endpoint_allowlist_metrics_filtered_total` (not remapped) and confirm `aggregator.filtered_tags_cache_miss`
  as the only change the Agent sees. `program.differs` is computed once per plan running and default program.
- **The check.** S0 and S14a are generations with no cut now (S0 checks `plan_cuts_total`); S6 and S11b were already.
- **Results.** The plan, syntax, kernels, and demo crates pass 1,369 tests, with and without
  `saluki-metric-plan/test-util`; ADP 590 of 590; the release property tests 79 of 79; clippy (`-D warnings`),
  `make check-fmt`, and Vale on the changed documents are clean. The first check run (`omit-check-b`) failed at S14a,
  which still expected adoption; with S14a expecting a generation with no cut, the check passed (`omit-check-c`):
  152.7 s and 283.5 s, 7 minutes 24 seconds with the build, canaries 2,886 of 2,886 and 4,798 sent with 4,799
  received, within the allowance.

## What stalls admission at a flush

On 2026-10-15 the metrics path was instrumented to find out what happens at a flush, before designing N2 (recorded on
2026-11-04); results are
in [what stalls admission at a flush](programmable-metrics-next.md#what-stalls-admission-at-a-flush) and G9 of the
[ADP findings](adp-efficiency-findings.md), and the tables in `test/scaling/baselines/stalls-2026-10-15/`.

### What was built, and what was kept

A `flow_trace` module in `saluki-core`, off unless `SALUKI_EXPERIMENT_TRACE` names a file: every dispatcher output
counts its time waiting to send (an `Instant` pair per send), every region host publishes its input queue's depth and
records each drain (start, duration, steps, time waiting on its sends, events out), and a thread writes it all every
100 ms with Unix-millisecond stamps. `run-limits.sh` gained `SCALING_FLOW=1`, which turns it on and samples, from the
host about every 100 ms, ADP's CPU time and throttling, lading's bytes, and the socket; and `SCALING_RECORD` gained
monotonic timestamps with the wall clock's offset beside them, so that instruction samples line up with the trace.

The trace answered its question and was **not kept**: it is ad hoc instrumentation in core library code with a check
on every send, and N2's gates should rest on telemetry designed for them, such as the time admission pauses per flush.
It is kept as `flow-trace.patch` beside the results (it applies to `7c7631a4bb`); the profile timestamps were kept.

### Findings

The drain pauses admission for 28 to 76 ms at 0.6 and 1.2 cores (100 to 290 ms at 0.3), about 3% of a flush's
instructions; the encoder's burst after it, two to three seconds of encoding and compression, is about 85%, and starves
admission under the quota: admission halves for about two seconds at 0.6 cores. No component is blocked sending; it is
CPU starvation. `main` and the plan are the same in all of it. A non-blocking drain alone removes the smaller part, so
N2 became the foundation of four layers: the non-blocking drain, pacing, priority for admission, and shedding.

### What went wrong, and what was learned

- **The earlier stall measures were confounded.** lading sends each second's bytes in about half a second and then
  idles, so the fastest and slowest seconds of N1 mixed its pacing with ADP's stalls; the N2 gates that rested on them
  were replaced by admission per 100 ms after each flush.
- **The flush interval is 15 seconds,** not the 10 several notes had assumed.
- **The socket "queue" was not a depth:** for a Unix datagram socket `ss` reports the first datagram's size, and the
  socket's memory is invisible from both ends; an attempt to read lading's send buffer instead showed nothing.
- **This kernel's perf does not take the wall clock;** `-k mono` with a recorded offset does the job.
- **Two apparent differences between the plan and `main` were artifacts:** a one-flush profile made the plan's encoder
  look 2.5 times as costly, and anchoring `main`'s flushes on CPU peaks made the plan's throttling look five times as
  high. Four flushes per profile, and the same anchor (the encoder's onset) for both, erased both.
- **A parsing slip** read a drain's send-wait field as its event count, briefly suggesting drains of 132 metrics.
- **The analysis scripts lived in `/tmp`,** which was cleared before they were committed; they were rewritten from the
  session into the results directory and reproduced the reported figures exactly.

### Left open

- Lower thread priority for the encoder, in the quick loop: the cheapest test of the priority layer.
- The design of the non-blocking drain and of pacing, and the telemetry N2's gates need.
- The instrumentation's own overhead was not measured.

## Version 2: the plan, and V2 (resources artifacts removed)

On 2026-10-15 the owner agreed that the premise behind stages 1 to 9 (programmability inside a native system that
stays the authority) had shifted to one where native configuration is a front end and the program is the system, and
asked for the machinery of coexistence to be removed step by step, in the foreground, keeping program mode compatible
with native mode and differentially testable. [Version 2 simplifications](programmable-metrics-v2.md) lists ten steps.

**V2 went first, not V1.** Opaque hosts (V1) turned out to carry about 50 tests of the frame and the interpreter's
re-admission, which V7 and V8 rework anyway; resources artifacts were a cleaner cut, since the demo no longer used them.

**What V2 removed** (amendment 192):

- The plan crate: `ResourceProvenance` and the `provenance` of recipients and output bindings; `Selector::Declared`;
  `ResourcesArtifact` and its validation; the installer core's `declared` module (the resources product's revisions,
  acknowledgments, credentials, and failure attribution); `ResourcesEvent`; the channel's `resources` product, whose
  feed is now one watch channel; `ResourcesStatus`; the `resources_` output prefix; the reserved-recipient-name hook;
  `RecipientSource`, so directory entries carry no `source`; the resources schema; and the declared-recipient limits.
  `MAX_DECLARED_OUTPUTS` became `MAX_IMPLIED_OUTPUTS`. The keep rule, which native endpoint changes need too, moved to
  `install/core/resources.rs`; every recipient keeps its host's native delivery policy.
- ADP: the resources product's reader (`installer/product.rs`), declared credentials (`credentials/<name>`), the
  declared lowering in `resources.rs`, `Lowering::lower_with`, status provenance and the declared-config view, and the
  `experimental.programmable_metrics.resources.credentials` key with its source model, seed, `SALUKI_KEYS` entry, and
  generated reference.
- The demo: the stub's resources product, store, and views (an old store with resources still reads), and the page's
  declared-output and recipient-provenance text.

**Tests.** Native-path tests that used declared recipients as their vehicle now use native ones: the
reopen test to routing group 0, the explain change test to routing group 1's removal, the placement test to an
implied output of an endpoint of its own (an implied output for group 0's endpoint is a duplicate delivery, which
validation rightly refuses), and ADP's 50- and 1,000-cycle reclamation test to two live endpoints, one with an endpoint
routing group of its own, so that a lease is still prepared and drained each cycle. Tests of the resources product
itself were deleted. The plan crate passes 904 tests (property tests excluded, the resource table's included), ADP's
`programmable_metrics` tests 323, the config crates 250, the demo 146; clippy (`-D warnings`) and `cargo check
--workspace --tests` are clean.

**Left for later**, in the plan: the delivery host's declared origin, bounded delivery policies, and conflict
detection in `saluki-components`, which ADP no longer uses.

## Version 2: V3 (native configuration lowers to literals)

**What went** (amendment 196): every policy binding in ADP's registry (24 of 29 entries), keyed bindings
(`BindingKey`, `Resolve::Keyed`, `metrics_endpoint_routing.group.<key>.*`), derived bindings, and `BindingClass::Policy`;
the catalog's `detached_programs` and `facts_and_signals`; composition's `policy_binding` refusal; the detached export's
conversion of bindings into values (`policy_literals`, `write_bindings_as_values`) and the catalog argument of
`eliminate` and `diff_operations`; `operation_params` writing bindings as their values, which made a literal the same
parameter as a binding of that value; and `policy_differences`, which located settings by their bindings' places.

**What replaced it:** lowering writes each setting as a literal, read from the current configuration for the settings a
native component reads live (the filterlists, the tag filterlist, the output branches' lists) and from the startup one
otherwise, normalized as its type. The installer already lowered and proposed on every configuration change, so a
native policy change is now a generation; the new test
`a_native_policy_change_installs_a_generation_that_rebuilds_only_the_kernels_whose_settings_changed`, written first and
passing on both paths, pins that a filterlist, tag filterlist, routing list, or mirroring list change changes one slot's
program, builds at most that node's kernel, keeps aggregation's state, and makes no cut. Comparison goes through one
function, `compared_params`: decoded when it can be, so a reordered set is the same set, with every binding kept, so a
literal never equals a binding. `node_differences` replaces `policy_differences`, and status names default-program
nodes. The installer's native telemetry reads the plan's literals (`ListLength`, `MatcherEntries`, `ParamChanges`).

**A consequence:** a filter with nothing in it is now a `split` on a constant, which R3 folds when compiling, so the
native counters region hosts keep for its ports were missing (the Agent's `dogstatsd.listener_filtered_points` and
`aggregator.dogstatsd_filtered_metrics`, caught by the remap test). The installer now registers them at zero, as the
native components do.

**Tests.** The plan crate's default-program fixtures, its export fixtures, ADP's `every-branch.json`, and the demo's
export fixture carry literals; explain golden files were regenerated. Tests of policy bindings were rewritten against
literals, or deleted where the behavior is gone (the refusal, keyed lookups, value-versus-binding equality, now tested
the other way). Targeted runs: the plan crate's non-property suite, ADP's bindings, lowering, export, telemetry,
telemetry profile, API, installer, live, and differential tests, and the demo crate.

## Version 2: V4 and V5 (programs are whole, one program document)

**What went** (amendment 197): scopes as replaceable parts (`PolicyArtifact.scope` and `.base`, `ArtifactBase`,
`Plan.scopes`, scope declarations, the catalog's scope inputs and exits, `ArtifactPortRef::Exit`, the validation rules
`ScopeInputs` and `ScopeClosed`, `ScopeName`); output fingerprints and the stale-base check (`stale_base` in status and
in the stub's source); splicing and correspondence (`ScopeRegion`, `in_default_order`, `compose_all`,
`install/correspondence.rs`); the `resources` module; for V5, the per-scope tables, the cross-scope revision mark and
`reject_shared_revisions`, configs moving between scopes, and the "each alone" retry. The syntax lost the scope name,
`base`, and exit declarations.

**What replaced it:** composition is one pass: the default program's sources, then the artifact's nodes and edges in
artifact order; a name no `let` defines is a source. New refusals cover an unknown source, a source's ID reused, two
sources sharing a binding, and an emit to an undeclared output; an unmodified node with no binding value is refused
rather than taking the default program's node. The installer core takes one program document: no config revokes, one
installs, several reject every config and keep what runs, with one revision mark. Versions moved: export 2, explain 3,
store format 3, stub protocol 2. `diff_operations` moved to `install/diff.rs`, `resources::keeps` compares accepts, host,
and selector in place of fingerprints, and ADP's lowering writes sources first. The demo's editor, store, and checks
use one program; S0 compares `program_hash` with `default_program_hash`.

**Tests.** Tests of scopes, partial composition, splicing, staleness, fingerprints, several configs, and configs moving
between scopes were deleted, and the fixtures of stage 9's two scopes with them; the rest were ported to whole programs,
and tests of the new refusals, the one-document rule, and the removal errors were added. Targeted runs passed in the plan
crate, the syntax crate, ADP's `programmable_metrics` tests, and the demo crate; the full run, the release property
tests, and the demo check come at the end of version 2, not here.

**Results.** An unmodified printed export composes with an empty diff and is adopted, but its `Plan::hash` differs from
the default program's (the detached export leaves out what does nothing, and writes edges as written); V6's single hash
resolves that. `policy/<local>` node IDs and the `dsd_agg` adoption stay until V10, and `program.differs` stays.

## Version 2: V6 (elaboration as a phase)

**What went** (amendment 198): the second hash and resolution done outside composition. `ResolvedValues` and
`Plan::hash`'s argument, `strip_node_params`, the explain document's `structural_hash`, `export::operation_hash`,
`operation_params`, and `compared_params`, and the installer core's `does` on its installed and queued generations.
Composition's body left `compose.rs`, which keeps receipt, `Approval`, the outcome and drop types, `ComposeError`,
`demanded_outputs`, and `sources`.

**What replaced it:** `elaborate.rs`, one pass from the program as written to the core program: normal form, blobs
inlined, node IDs, sources, recipients and outputs, bindings from the catalog, then canonical order (the order the
pipeline syntax prints, after the host's sources), validation of the program as written with its bindings resolved,
and the core form: every binding written as its value, recorded in `BindingUses`, every parameter in normal form as its
operator encodes it. `elaborate_default` runs the same passes on a lowered default program, and is a fixed point on a
core plan. `compose` and `Composed` are names for `elaborate` and `Elaborated`. The installer core compares
`plan_hash`; `diff` and `KernelKey::of` read parameters directly; `node_differences` compares two core plans. A
candidate carries its `BindingUses`, adoption replaces them with the plan, and the core exposes `active_bindings` and
`active_report`, so ADP's explain shows the core plan with the bindings it was written with and compiles it again with
the report it was installed with. ADP elaborates every default program it lowers: the core plan installs and lays out
the frame, its hash is `default_program_hash`, and the plan as lowered stays what the export, composition, and the
default-program route use. The demo diffs against the core plan, checks the status by elaborating the default program
with the export's catalog, and checks the export by exporting the default program. Explain is version 4.

**A choice:** validation reports on the program as written, because the core plan writes a signal's value, and a
`split` on a signal that is off would otherwise be a predicate that can never match, a warning on every program while
failover is off. Compilation uses that report, so it treats such a split as the program wrote it unless another rewrite
applies first and validates the core plan again.

**Tests.** New `elaborate/tests.rs`: the unmodified export, also with its nodes and edges reversed, and the default
program elaborate to one plan and hash; the core form is a fixed point; a binding and its value, a parameter left out
and its default, and a set in another order each elaborate to one hash; a signal change changes the hash and only its
node's kernel key. Tests that compared a composition with the default program as lowered compare it with its core
plan, and boot harnesses boot the core plan with the elaboration's report. The syntax crate's fixture tests and the
demo's unmodified-export preview test compare the one hash again, which V4 had relaxed to operation hashes. Tests of
deleted behavior went or turned around: a literal equal to a binding's value is now the same parameter, kernel identity
is the core plan's, and plan order no longer follows the artifact. Explain golden files were regenerated: version 4, no
`structural_hash`, and for the default program core parameters, bindings by pointer, and canonical order. Targeted
runs passed: the plan crate's elaborate, install, explain, compile, IR, compose, export, and normalize tests (and, once,
its whole non-property suite), composition's property tests in debug, ADP's lowering, installer, API, telemetry, and
differential tests, the syntax crate's fixture tests, and the demo crate.

**Results.** An unmodified printed bound export elaborates to the default program's core plan, so
`default_program_hash` equals `active_plan_hash` while either runs. The detached export, which leaves out what does
nothing, is still a generation that builds no kernel (S0); elaboration does not leave nodes out, which would change the
running default program's telemetry. V3's precise updates hold: a native policy change rebuilds only the kernels whose
settings changed.

## Version 2: V10 (names and a compatibility analysis)

V10 was built in three commits: an additive half (`7b1e126d16`: operator reports, the telemetry contract, compatibility
roles), a wiring half beside V6 (`64fe645ca9`: the `dsd_prefix_filter` rename, the catalog's contract, telemetry by
name, validation findings, `keeps_identity` in the filter rewrites, ADP status's `telemetry`), and the deletions,
recorded here.

**What went** (amendment 199): the node namespace, `Namespace`, `NodeId::{reserved, policy, namespace, is_reserved}`,
the `policy/` prefix in `NodeId`'s and `PortRef`'s grammar and schema, and every caller (normalization, export,
elaboration's `node_ids` and `partial_emit`, the filter rewrites' `fresh_id`, the syntax crate's derived IDs, ADP's
lowering annotations, installer notes, and the region input capacity's `replaced`); naming by namespace,
`policy_<local>` components (`KernelBuildContext::detached`, the frame's `component_name`, ADP's `node_component`) and
the shared `<region>.policy` accounting group (`kernel_accounting_name`, `kernel_accounting_path`); amendment 182's
adoption (`the_aggregation`, and the `aggregation` parameter of `node_ids`); `single_aggregation` (amendment 169): the
validation rule, `Rule::SingleAggregation`, `Catalog::single_aggregation`, `MemoryCatalog`'s member and
`with_single_aggregation`, ADP's catalog opt-in, the plan crate's `without_single_aggregation` fixture, the export's
catalog member, and the demo's reference flag, catalog filter, and rule entry.

**What replaced it:** `NodeId::new(LocalId)`, serialized as the bare identifier. A plan document with a `policy/` node
ID is refused by `NodeId`'s parser, with a message naming the change; the IR version stays 1, as the plan proposed,
since elaborated plans are not stored. An artifact node's plan ID is its name, and a name equal to a source's is still
refused (`ReservedSource`). A kernel reports as the transform named after its node and is accounted under
`<region>.<node>`, except a state owner named after its isolation slot, which accounts at the slot's level by name
alone. The region input capacity counts every node of the boot plan by its native component, the one the plan's
provenance names or its own name. An older export's `single_aggregation` member is dropped with the other members the
catalog does not read.

**A choice:** the rewrite harness had told an author's nodes from native ones by the namespace. It now names an
author's nodes `a_<local>` and gives every native filter of a random plan a telemetry contract entry
(`contract_catalog`), so that R5a and R6 keep moving native filters only whole, as before, and `check_structure` still
finds the native nodes by name.

**Tests.** Turned around: the plan crate's one-aggregation tests became a program with three aggregations validating
with no findings (`validate`), a second aggregation validating with no findings at each of five places and placing
only on the timestamp split's passthrough branch (`install::program_tests`), and the installer core installing that one
at a cut while refusing one after enrichment as unplaceable; `the_one_aggregation_keeps_the_default_programs_id_...`
became `a_renamed_aggregation_is_a_new_lineage_that_starts_at_a_cut` (the old node `Removed`, the new one `Added`, both
`cut`, and one `telemetry_contract` warning at `plan`). ADP's lifecycle case "a second aggregation" is now refused by
placement after enrichment. The demo's preview test of a renamed aggregation expects the rename note and the contract
warning; its second-aggregation preview test expects no findings; scenario S5c is refused by placement (the editor
flags nothing), S5d's editor shows the contract warning, and S14a expects a cut (`Compiled::Warned`, then
`plan_cuts_total{outcome="completed"}` grows) while S14b expects no cut from there. New: `NodeId` refusing `policy/`
names, in a plan document too. `policy/` IDs in tests became bare names (`second_agg` where they would collide), the
harness's `a_` names in the rewrite and normalization tests, and the canonical-form fixture's hash was recomputed with
Python. Golden files regenerated: the plan schema (`NodeId` and `PortRef` without the prefix) and
`artifact-like-policy.txt` (names only). Targeted runs passed: the plan crate's IR, validate, elaborate, compose,
compile, install, explain, normalize, operators, export, and canonical tests (545); ADP's installer, lowering, API,
telemetry, compatibility, and topology tests, differential included (225); the syntax crate's non-property suite
(191); and the demo crate but traffic, loopback, and launcher (141). Not run: the release property tests (the rewrite
harness's random plans now carry a contract) and `make demo-programmable-metrics-check` (S14's cut).

**Results.** Every node reports under its own name, and the default program's names are the remapper's, so native
series are unchanged; a program's added nodes report under theirs. A second aggregation validates anywhere and runs
where the boot frame has an isolation slot for it; V7a lifts the frame's limit.

## Version 2: V7a (state owners in any region)

V7a was built in parallel parts on an interfaces commit (`09129a6a71`), from the plan's Part 2 and the owner's answers to
its questions (2026-10-20): P1 the oracle (`305aa1c109`), P2 placement and compile (`0e793ba586`), P3 sub-regions in
the region host (`saluki-core`, before the interfaces), P4 the runtime and the installer's cuts (`6813323ddd`), P5
ADP (`546d5a2e0f`), with `12b073e2b9` rebasing the parts onto V10, and a documents commit, recorded here.

**What went** (amendments 200 and 201): `SlotKind` and `Slot::{is_isolation, output_point}`, `AdmissionPointInfo::feeds_state`
as a placement input, the frame's `state_upstream`, `paths_to`, and `FrameError::{StateOwnerPoints, StateOwnerPaths}`,
the frame's stage 10 refusal of a state owner outside a boot isolation slot, `Frame::{affected_points,
cancel_lock_order}`, `CutPlan::slots`, the `isolation` flag in `runtime/code.rs`, the transitional `RegionConfig`
`upstream`, `progress`, and `state_telemetry`, `MetricRegionDriver.state_node`, ADP's `heartbeat_period` and its
`feeds_state` branches, and `CutPoints::of(frame)`. One test went with the thing it tested: a snapshot that panics in one
of two owners of one region during a yielded drain, since a region now holds one owner.

**What replaced it:**

- **Regions.** Placement no longer distinguishes isolated nodes: a state owner places as any synchronous node and each
  slot's nodes are partitioned into regions (`RegionPlacement`, `RegionKey::{Main, State(node)}`), edges going only to
  later regions. A region holds at most one state owner, and `RegionProgram` per region carries its upstream points,
  progress exits, and output points; `StepExit::Region` writes to an internal exit.
- **Sub-regions** (`saluki-core` `subregion.rs`). A task inside a component with its own driver, input, control queue,
  and resource group, started by the control message that names it (before the prepare is acknowledged) and retired
  when no driver routes to it. ADP builds one per placed owner, with 128 input buffers and one per slot exit drawn from
  the post-boot budget. State telemetry registers under the owner's node name.
- **Time.** `WatermarkTracker::new(&[UpstreamPoint])` groups points by clock, an owner has one `OutputClock`, derived
  points are admitted before their header's ticks, an owner sends tick progress at its derived point, and
  `stamping_generation` takes `BoundaryRefs` for the boundary flush of a kept upstream owner at a cut.
- **Cuts by node.** `DerivedPoints` allocates each owner's output points, held while a live generation has the owner;
  `PlanDiff::place` classifies a retained owner whose region or upstream point IDs differ as cut; `CutPlan::boundary`
  names the kept owners upstream of an affected one; `install::cancel_lock_order` puts points upstream of an owner last.
- **Explain** lists regions with upstream points, derived points, and internal exits.

**Choices.** Re-sourcing compares the region's direct upstream IDs, derived points included: X re-sourced cuts X and
leaves Y, fed by X's derived point, kept, which is exact. A region's watermark follows the union of every live
generation's upstream points. A batch from a point an owner does not follow, whose entry reaches no state step, passes
through. A cut after enrichment is refused where the frame readmits host records (`CutPlan::check_frame`).

**Tests.** The interpreter and the runtime replay four new traces (16 to 19: two sources, an aggregation after
enrichment, a chain in steady state, and cuts through a chain, both cut, upstream cut, and downstream cut with the
boundary flush); `install::cut_tests` gains a chain armed through derived points and an owner cut for a changed region
or upstream. `transition_tests` expects one output clock per owner. Placement's and ADP's tests that refused a second
aggregation after enrichment now install it. Verified in the parts: `cargo check --workspace --tests`, the plan crate's
`compile`, `runtime`, `install`, and `interpret` tests, and ADP's `topology::` tests, with clippy and `make fmt`.
Not run: the release property tests, ADP suites other than `topology::`, and the demo check.

**Left for later:** sub-region buffer credits (kernel credits merge into the slot's, over-counting for a slot with several lanes),
the reason explain gives for cutting a moved owner, the 128-buffer size to confirm against N1, and R1, lanes, and V7b to V9.

## N1b: bursts across a flush

On 2026-10-20 and 21, after V7a and before V7b, N1 was measured a second time, built independently of N1a (the two lines
of work met at a merge on 2026-11-04): native at the merge base against the plan through V7a, at 0.05, 0.3, 0.6, and
1.2 CPUs with lanes off, a steady window at 30% of native capacity and then a burst of four times that, timed across a
flush, at 3,000 and 100,000 contexts, with an A/A pair per image (96 runs in the dory VM). The method, results,
findings, and gates G1 to G6 are in [N1b](programmable-metrics-next.md#n1b-bursts-across-a-flush) and
[gates](programmable-metrics-next.md#gates-for-n2-to-n4); the summaries in `test/scaling/results/n1-20261021/`.

### What was built

`test/scaling/run-regime.sh` (`cfa417d5d7`, [README](../../test/scaling/README.md#bursts-across-a-flush-run-regimesh),
[runbook](programmable-metrics-measurement.md#the-production-regime-in-containers-n1)), which runs ADP under a
fractional CPU limit on two cores with two workers, for native, plan, and a candidate image with an A/A pair of each;
`regime_sampler.py`, a container that reads ADP's cgroup and the generators' bytes written every 100 ms from outside
ADP's cgroup; and `summarize-regime.py`. The compose file gained the burst generator and the sampler (profile
`regime`), a UDP transport, and a health check that runs every second only until ADP is healthy; `run-curves.sh`
renders UDP generators and can be sourced for its helpers.

### What went wrong, and what was learned

- **UDP measured lading.** lading releases each second's bytes in one clump at the start of the second, which
  overflows a UDP socket's default receive buffer at any rate. The series run over Unix datagrams instead, and drops are
  lading's shortfall against its configured rate.
- **The Docker VM restarted twice in four hours**, because the sampler wrote its timeline into a bind mount of the host
  and read a phase file the host rewrote, every 100 ms. Both moved into a volume of the VM.
- **0.6 CPUs was never overloaded.** The series interpolated 0.6's capacity from 0.3 and 1.2 and underestimated it; the
  calibration made after the series showed it. Calibrate every limit.
- **A flush seemed to stall the plan more than native** at 0.3 CPUs with 100,000 contexts, which N1b read as the
  blocking drain. The V7b runs and the stall study later showed the drain to be the smaller part
  ([N1b's findings](programmable-metrics-next.md#what-n1b-says)).

## Version 2: V7b (one region with an interleaved flush)

V7b was built from [its design](programmable-metrics-v7b.md) and the owner's answers (2026-10-21) in parallel parts on
an interfaces commit (`ed58c17def`): P3 the region host's background turns (`a4412d5884`), P1 the frame and placement
(`e5318f28dd`), P2 the aggregation kernel's flush on touch (`19cb393167`), P4 the driver's interleaved passes
(`e087239c37`), then P5 (ADP and configuration), P6 (the demo), and the integration, in one commit (`7e6b437476`), and a documents
commit, recorded here.

**What went** (amendments 203 to 206): R1's cost model whole, `FrameOptions::pipelining`, `Slot::cost`,
`FusionVerdict::Pipelined`, `Frame::cost_budget`, every operator's cost estimate (R4's split estimate kept as
`compile::split_cost`), the cost and pipelining fields of explain, and ADP's cost pins; `lanes.mode: auto` as the default;
the boot log's `pipelining` field; the `dsd_agg` and `metrics_enrich` regions without lanes, and R1's `_region` slots.

**What replaced it:**

- **The frame.** `FrameOptions { isolate_state }`, which ADP sets from the lanes-on flag: without it a state owner
  fuses like any synchronous node, and the default program's metrics path is one slot, `dsd_pre_aggregation`, with 902
  input buffers. Placement runs a state owner in the region of its inputs when that region runs no other; a slot whose
  boot plan runs a state owner keeps its main region for it.
- **The kernel.** Each context carries the number of the pass that last visited it; admission during a pass flushes a
  context it reaches first, a pass removes in place, the table never grows while one is pending, and two fallbacks
  complete the pass inside `consume`. `RoomToAdmit` and `PassProgress` answer the driver.
- **The driver and host.** A tick or boundary flush of an owner not on lanes is a pass, paced to a third of the flush
  interval with a step at least every 100 ms, admitting batches that fit and running ahead of admission once behind;
  touch output is the step's own. The host runs background steps while no batch waits. Interleaving is on by default.
- **Integration's decisions.** A state owner accounts under its own name wherever it runs: fused, beside its region,
  so aggregation's resource group and bounds stay `dsd_agg` (`KernelAccounting`). A downstream owner counts a derived
  point's time only from the header-only batch its upstream owner sends once a drain completed, so a drain split over
  several deliveries never lets it close a bucket early. The fused state slot publishes the state lane series without
  lanes, `plan_lane_flush_touched_contexts_total` among them.

**Found at integration.** A renamed aggregation was placed into the fused main region, which still held `dsd_agg`'s
state, and the preparation failed (ADP's lifecycle test); placement now reserves the main region for the slot's boot
owner. A first fix for split drains held the stamp on the data, which changed the flush time of records a downstream
region passes to an output (trace 17 interleaved); the hold moved to the receiving side. The fused slot was not a
state slot to ADP (`is_state_slot` read the boot output points, which a fused owner has none of), and every stateless
kernel fused beside aggregation drew on the budgets of state; both fixed. Deleting R5a's refusal to move a filter
across an R1 cut let the release property test of the rewrite pipeline find a case where R3 folds a native `split`
and R5a then copies the author's filter left dropping into the native drop; R5a is right by the product's rule (only
contract nodes move whole), and the harness, whose structural check keeps every native node, now names native drops
in its contract too.

**Tests.** New: transition trace i16 (a pass pending at a crossing and at a reset, a boundary flush that interleaves)
and i17 (a chained owner in another region receives a pass in several parts), traces 1 to 16 on the fused layout and
1 to 19 interleaved, `property_test_interleaved_drains_match_the_interpreter`,
`property_test_an_interleaved_flush_is_a_flush_before_its_batches`, the frame property test for both settings, the
host's background-turn tests, a placement test for the renamed owner, and accounting tests for both layouts. ADP's
topology, lane, isolation, installer, and telemetry pins moved to one region; the telemetry tests drop the one remapped
line that goes, `aggregator.processed` for DogStatsD metrics. At integration, all passing: the plan crate 919
(`--features test-util`) and its 52 release property tests, the kernels crate 174 and 26, `saluki-core` 801 and 12,
the syntax crate 191 and 11, ADP's `programmable_metrics::` 337 (the differential tests included), the configuration
crates 332, the demo 162, `make demo-programmable-metrics-check` (both launches, 159 s and 274 s, canaries balanced),
clippy `-D warnings` on the touched crates, `cargo check --workspace --tests`, and `make check-fmt`.

**Left for later:** the conservative room check, several owners in one region, boot-generation sub-regions in the frame
harness, and the `aggregator.processed` remap ([version 2: what is left](programmable-metrics-v2.md#what-is-left));
under saturation a chained owner's tick may wait out its 2 s grace for an upstream pass spread over up to 5 s. Next:
the N1 candidate runs.

## V7b's candidate runs and the layout A/B

On 2026-10-21 V7b was judged in N1b's harness against N1b's plan image, and on 2026-10-22 its fused layout was compared
with an isolated one; results in [V7b against N2](programmable-metrics-next.md#v7b-against-n2) and
`test/scaling/results/v7b-20261021/` and `layout-ab-20261021/`.

### What was built

The Saluki-only `experimental.programmable_metrics.isolate_state` key (`cbe180d2e0`, measurement only), which isolates
state owners without lanes; the regime harness's `REGIME_VARIANTS` and `adp/isolated.yaml`; and the region driver
bench's `layout_steady_admission` group (`ddc74e1851`).

### Findings

- **The candidate missed the exit as G1 to G6 stated it**: blocked flush time 0, but G5 and G6 missed at 0.3 CPUs, G6
  at 1.2, and G1 at 3,000 contexts at 0.3 and 0.6. Admission still fell after the flush on every image, at a full
  quota, and the pace did not spread a pass.
- **G1's miss was noise.** Fused and isolated measured the same steady CPU per metric within the A/A distances, and the
  in-process bench the same region cost (277 against 275 ns a metric in batches of 1,024).
- **The flush count was wrong.** `aggregate_flushes_total` read 6 to 9 where plan read 5: with aggregation fused, a
  gated tick a header released stayed gated during its pass, so every later header released it again, and it flushed a
  second time at the same flush time. Fixed in `cba809db95`, which also times every drain of flush ticks. Alongside,
  `de240e4005` fixed the phase of a lineage a cut creates while a pass is pending, which a clock reset exposed in the
  interleaved property test.

### Decisions

On 2026-10-21 the owner decided that the merged layout stays the default when lanes are off, that V7b is done as
optionality (the physical layout is a plan choice), and deferred flush pacing, encode cost, G5's pool question, and
G6's thresholds ([V7b's outcome](programmable-metrics-v7b.md#65-as-built)).

### Read against the stall study

The stall study, made on N1a's host before V7b and merged on 2026-11-04, explains the miss: the drain is about 3% of a
flush's cost and the encoder's burst about 85%, the same on native. V7b delivered N2's first layer, the non-blocking
drain; pacing and priority for admission are the open layers, and G6's thresholds are superseded.
