export const meta = {
  name: 'debate-count-oracle-spec',
  description: 'Draft then adversarially minimize the differential count-oracle spec',
  phases: [
    { title: 'Draft', detail: 'draft the spec from locked decisions' },
    { title: 'Debate', detail: 'parallel lenses each argue for a smaller diff' },
    { title: 'Synthesize', detail: 'fold debate into the most minimal sound spec' },
  ],
}

const LOCKED = `
GOAL: Add a differential COUNT oracle to the Antithesis "differential" scenario
(test/antithesis/scenarios/differential/). Today the scenario compares only the
context SET per lane -- (metric_name, tagset, kind) membership -- and discards
values (intake/src/capture.rs, context_of). The new oracle compares per-context
emission MULTIPLICITY (point-counts) between the two lanes (agent lane =
DD_DATA_PLANE_ENABLED false, adp lane = true), which is invisible today.

LOCKED DECISIONS (do not relitigate the WHAT; only minimize the HOW):
1. Count = number of POINTS emitted per (lane, context), all metric kinds (no kind
   filter; kind is already part of context identity). Tracked in intake by extending
   the value in Lanes.seen from EpochSeconds to a record carrying first_seen + points.
   first_seen MUST be preserved so the existing set oracle is unchanged.
2. Oracle = live aged bounded skew: a shared context is a defect once
   |points_adp - points_agent| > K has persisted longer than ACCEPTABLE_FLUSH_DELAY.
   Encoded as assert_always (green by default), polled continuously (live, not a
   run-end finally sweep).
3. K = ACCEPTABLE_COUNT_SKEW = 1, placed beside ACCEPTABLE_FLUSH_DELAY in
   harness/src/lib.rs. Absorbs steady-state flush-phase + non-atomic double-read.
4. Domain = INTERSECTION of contexts present in both lanes. A single-lane context is
   already a symmetric-difference defect caught by the existing set oracle; the count
   oracle stays silent there so the two do not double-report.
5. Aging state (skew-exceeded-since) lives client-side in the long-running check
   binary (check binaries are outside the SUT fault domain).
6. Fault model: DISABLE all ASYMMETRIC faults -- driver->SUT, SUT->intake, per-lane
   process kill, one-sided clock skew. Only lane-symmetric / self-healing faults
   (global scheduling, cpu, mem) remain. Cumulative counts cannot survive an
   asymmetric fault (permanent skew), so parity between lanes is a hard precondition.

HARD CONSTRAINT: the MOST MINIMAL change possible. Correctness first, then minimal
diff. Prefer extending existing artifacts over adding new ones.

KEY FILES (read them to ground every claim):
- test/antithesis/scenarios/differential/README.md        (scenario spec)
- test/antithesis/scenarios/differential/src/contexts.rs  (Context, Difference::between, delayed(budget), LaneView::fetch)
- test/antithesis/scenarios/differential/src/bin/eventually_differential_contexts.rs  (existing live oracle to mirror/extend)
- test/antithesis/scenarios/differential/src/bin/finally_differential_contexts.rs
- test/antithesis/scenarios/differential/intake/src/capture.rs        (context_of, Lanes.seen)
- test/antithesis/scenarios/differential/intake/src/http/antithesis.rs (LaneView HTTP shape)
- test/antithesis/harness/src/lib.rs                      (ACCEPTABLE_FLUSH_DELAY)
- test/antithesis/scenarios/differential/docker-compose.yaml + the snouty/antithesis fault config
`

phase('Draft')
const draft = await agent(
  `You are drafting a concise, implementation-ready design spec.\n${LOCKED}\n\n` +
  `Read the KEY FILES first so every file path and symbol you cite is real. Then write ` +
  `the spec as markdown with these sections: Summary; Fault model (soundness precondition); ` +
  `Data model change (intake); Oracle (the new/extended check + assertion); Constants; ` +
  `Scope & non-goals; Testing. Cite exact file paths and the specific edit at each site. ` +
  `Keep it tight -- this is a minimal-change spec, not an essay. Return ONLY the markdown.`,
  { label: 'draft-spec', phase: 'Draft' }
)

phase('Debate')
const LENSES = [
  { key: 'surface-area', prompt:
    `Attack the NUMBER of artifacts. Must there be a NEW binary (eventually_differential_counts.rs), ` +
    `or should the count assertion be folded into the existing eventually_differential_contexts.rs so one ` +
    `poll loop and one fetch serve both oracles? Must LaneView gain a new endpoint, or just a field? Must ` +
    `there be TWO new constants or can we lean on one. Every new file/endpoint/constant is guilty until ` +
    `proven load-bearing.` },
  { key: 'data-model', prompt:
    `Attack the intake data-model change. What is the SMALLEST edit to capture.rs / LaneView that carries ` +
    `point-counts while preserving first_seen and the existing set oracle byte-for-byte? Is a new struct ` +
    `justified vs a tuple? Does counting POINTS vs series survive the two lanes being different implementations? ` +
    `Is any change to context_of needed at all?` },
  { key: 'oracle-logic', prompt:
    `Attack the oracle logic. Given segment-1 and segment-2 are fault-free and only lane-SYMMETRIC faults ` +
    `remain, is client-side aging actually needed, or does a plain assert_always(skew <= K) each poll suffice? ` +
    `Can the existing Difference/delayed machinery in contexts.rs be reused for counts instead of new code, or ` +
    `does that couple the two oracles badly? Find the least new logic that stays sound.` },
  { key: 'fault-config', prompt:
    `Attack the fault-config change. Find the current fault configuration in the differential scenario ` +
    `(docker-compose + snouty/antithesis config). What is the SMALLEST, ideally additive, change that disables ` +
    `driver->SUT, SUT->intake, per-lane kill, and one-sided clock skew while leaving symmetric faults on? Is ` +
    `anything already disabled? Flag if this requires touching shared harness config that affects other scenarios.` },
  { key: 'yagni-scope', prompt:
    `Attack scope. What in the draft can be DEFERRED or DELETED without losing the core value (catching ` +
    `emission-multiplicity divergence between lanes)? Consider: a finally_ variant, per-kind special-casing, ` +
    `new telemetry, config knobs, anti-vacuity anchors. Cut anything not essential to the first landing.` },
]
const debates = await parallel(LENSES.map(l => () =>
  agent(
    `${l.prompt}\n\n=== LOCKED CONTEXT ===\n${LOCKED}\n\n=== DRAFT SPEC UNDER REVIEW ===\n${draft}\n\n` +
    `Read the relevant KEY FILES to ground your challenges in the real code. For each challenge give a ` +
    `concrete proposal to cut or shrink, its rationale, and the risk of doing so. Recommend cut / shrink / keep.`,
    { label: `debate:${l.key}`, phase: 'Debate', model: 'sonnet', schema: {
      type: 'object', additionalProperties: false,
      properties: {
        lens: { type: 'string' },
        challenges: { type: 'array', items: {
          type: 'object', additionalProperties: false,
          properties: {
            target: { type: 'string' },
            recommendation: { type: 'string', enum: ['cut', 'shrink', 'keep'] },
            proposal: { type: 'string' },
            rationale: { type: 'string' },
            risk: { type: 'string' },
          },
          required: ['target', 'recommendation', 'proposal', 'rationale', 'risk'],
        } },
      },
      required: ['lens', 'challenges'],
    } }
  )
)).then(rs => rs.filter(Boolean))

phase('Synthesize')
const synth = await agent(
  `You are the spec editor. Fold the debate into the MOST MINIMAL sound spec.\n${LOCKED}\n\n` +
  `=== DRAFT ===\n${draft}\n\n=== DEBATE CHALLENGES ===\n${JSON.stringify(debates, null, 2)}\n\n` +
  `Accept every cut/shrink that keeps the oracle correct and does not break the existing set oracle. ` +
  `Reject any that trades soundness for size and say why in cut_log. The LOCKED WHAT is fixed; you are ` +
  `only minimizing the HOW. Produce the final spec markdown (same section structure as the draft, citing ` +
  `real file paths and the specific edit at each site), a cut_log of what changed vs the draft and why, ` +
  `and any residual open_questions for the human.`,
  { label: 'synthesize', phase: 'Synthesize', effort: 'high', schema: {
    type: 'object', additionalProperties: false,
    properties: {
      spec_markdown: { type: 'string' },
      cut_log: { type: 'array', items: {
        type: 'object', additionalProperties: false,
        properties: { item: { type: 'string' }, decision: { type: 'string' }, reason: { type: 'string' } },
        required: ['item', 'decision', 'reason'],
      } },
      open_questions: { type: 'array', items: { type: 'string' } },
    },
    required: ['spec_markdown', 'cut_log'],
  } }
)

return synth
