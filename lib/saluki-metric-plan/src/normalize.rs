//! Normalization: one form for programs that differ only in how they are written.
//!
//! The [pipeline syntax](../../../docs/design/programmable-metrics-pipeline-syntax.md#normalization) prints one form
//! of each graph, and equivalent programs must give the same document, and the same hash, whichever way they were
//! written: a default program lowered from native configuration, its export, and an unchanged program parsed back from
//! that export. Normalization runs at the end of native lowering and on every artifact before composition. It never
//! adds or removes a node, and never changes a node ID apart from renaming a derived terminal whose port it moves.
//!
//! The rules, each checked against the equivalence oracle and for idempotence:
//!
//! 1. **Fill in defaults**: every parameter left out that has a [default](crate::ir::ParamSpec::default) is written.
//! 2. **Literal set matches** become `Eq`, `Prefix`, `In`, or `PrefixIn`, or `Const(false)` when empty; one-element
//!    `In` and `PrefixIn` become `Eq` and `Prefix`. Bound values are left alone.
//! 3. **Predicates are simplified, keeping their operands' order**: junctions of one kind are flattened, constants
//!    folded, double negations removed, and `IsEmpty` of a literal list is a constant.
//! 4. **Split polarity**: a `split(p)` whose `true` port leads to a `drop` and whose `false` port does not becomes
//!    `split(not p)` with its ports swapped.
//!
//! Part E of the pipeline syntax work implements the rules; until then both functions return their input unchanged.

use crate::ir::{Plan, PolicyArtifact};

/// Returns the normal form of an artifact.
pub fn normalize_artifact(artifact: &PolicyArtifact) -> PolicyArtifact {
    artifact.clone()
}

/// Returns the normal form of a plan.
pub fn normalize_plan(plan: &Plan) -> Plan {
    plan.clone()
}
