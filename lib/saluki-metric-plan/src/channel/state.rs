//! What the host reports back through the channel: an acknowledgment per config, and a status summary.
//!
//! These types are the wire form of the local stub protocol (version 1), member for member: every member is written,
//! `null` when it has no value, and readers ignore members they do not know.

use std::{collections::BTreeMap, fmt};

use serde::{Deserialize, Serialize};

use super::snapshot::{ArtifactConfigId, ArtifactProduct};
use crate::{
    canonical::PlanHash,
    ir::{LocalId, ScopeName},
};

/// The scope the top-level members of the [status summary](StatusSummary) describe, as version 1 of the stub protocol
/// defines them: the one scope of the default program, which artifacts replace to change the metrics path after its
/// sources.
pub const POLICY_SCOPE: &str = "metrics";

/// The state an acknowledgment reports for a config.
///
/// The specification's acknowledgment states, `pending`, `prepared`, `active`, `retired`, and `rejected`, plus
/// `failed`, for an artifact whose generation exhausted its panic budget under supervision.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyState {
    /// The artifact is waiting to be prepared, or is being prepared.
    ///
    /// With an error, it is held rather than in progress: it waits for something only another change provides, such
    /// as a program waiting for a recipient that configuration does not declare ("waiting for recipient X"). It is
    /// re-evaluated on every configuration change.
    Pending,

    /// Every region holds the artifact's generation, which is not committed yet, or is kept to roll back to.
    Prepared,

    /// The artifact's generation is the one admission points stamp new batches with.
    Active,

    /// The artifact's generation was superseded and has retired: nothing references it any more.
    Retired,

    /// The artifact was refused: on receipt, at composition, at validation, or at preparation. The active generation
    /// keeps running.
    Rejected,

    /// Supervision failed the artifact's generation, and the installer rolled back.
    Failed,
}

impl ApplyState {
    /// Returns the state's name, as the channel writes it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Prepared => "prepared",
            Self::Active => "active",
            Self::Retired => "retired",
            Self::Rejected => "rejected",
            Self::Failed => "failed",
        }
    }
}

impl fmt::Display for ApplyState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The latest state of one config the host has seen.
///
/// The host keeps at most one acknowledgment per config ID. A missing acknowledgment means the state is unknown.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ConfigAcknowledgment {
    /// The product the config belongs to.
    pub product: ArtifactProduct,

    /// The config's identifier.
    pub id: ArtifactConfigId,

    /// The config's version, as the channel delivered it.
    pub version: u64,

    /// The artifact's revision, once its envelope has been read.
    pub revision: Option<u64>,

    /// The hash of the artifact document as received, once its envelope has been read.
    pub artifact_hash: Option<PlanHash>,

    /// The config's state.
    pub state: ApplyState,

    /// The generation the artifact runs in, once it has one.
    pub generation: Option<u64>,

    /// The admission time at which the cut of the artifact's generation takes effect, in whole milliseconds since the
    /// Unix epoch, while the cut is armed and not finalized.
    ///
    /// Only installations that switch state owners at a bucket boundary, cut installations, have one: `None`
    /// otherwise, and once the cut is finalized.
    pub cut_at: Option<u64>,

    /// Why the config was rejected or failed, in plain text; for a config held `pending`, what it waits for ("waiting
    /// for output X"); and for an `active` artifact composed with some emits dropped, why it runs degraded ("output X
    /// is no longer declared; its emits are dropped until it returns"), with any note on rebasing it
    /// ([`ScopeComposition::notes`][crate::compose::ScopeComposition::notes]). Never contains secrets.
    pub error: Option<String>,
}

/// The artifact a status summary reports as active.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ActiveArtifact {
    /// The config the artifact was delivered in.
    pub id: ArtifactConfigId,

    /// The artifact's revision.
    pub revision: u64,

    /// The hash of the artifact document as received.
    pub artifact_hash: PlanHash,
}

/// The host's status summary, sent with every poll.
///
/// The members are those of the stub protocol's `status` object, plus `stale_review` and `scopes`, additive members:
/// the protocol says a stale review is flagged in status, and readers ignore members they do not know. The top-level
/// `default_program_hash`, `active_artifact`, and `stale_base` describe the policy scope ([`POLICY_SCOPE`]); `scopes`
/// describes every scope.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct StatusSummary {
    /// The base program hash of the current export of the policy scope: what an artifact derived from that export
    /// records as `base.program_hash`. A client refetches the export exactly when it changes.
    pub default_program_hash: Option<PlanHash>,

    /// The highest artifact revision seen, if any. A proposal at or below it is stale.
    pub revision_high_water: Option<u64>,

    /// The active generation.
    pub active_generation: Option<u64>,

    /// The hash of the active composed plan, with resolved values: the hash an approval's `approved_against` names.
    pub active_plan_hash: Option<PlanHash>,

    /// The artifact active for the policy scope, or `None` when the default program's region runs there.
    pub active_artifact: Option<ActiveArtifact>,

    /// Whether the policy scope's active artifact's base no longer matches the current default program. The artifact
    /// keeps running; its author rebases it on the new export.
    pub stale_base: bool,

    /// Whether an active artifact, of any scope, was approved against an active plan other than the one it was
    /// prepared against.
    pub stale_review: bool,

    /// Why the host is degraded, in a few words, or `None` if it is not.
    pub degraded: Option<String>,

    /// The number of generations alive.
    pub live_generations: u64,

    /// The status of each replaceable scope of the default program, by scope: an additive member. The top-level
    /// `default_program_hash`, `active_artifact`, and `stale_base` keep describing the policy scope, `metrics`.
    ///
    /// A host that reports every scope's base writes every scope; one that reports the policy scope's base alone
    /// writes the member only while an artifact is active in another scope. It is left out when empty, so the wire
    /// form of a host with one scope in use is that of the first version of the protocol.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub scopes: BTreeMap<ScopeName, ScopeStatus>,

    /// Which program the active generation runs, and whether the host's native policy settings apply to it: an
    /// additive member, left out by a host that does not say, and while no generation is active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<ProgramStatus>,
}

/// Which program the active generation runs, and whether the host's native policy settings apply to it.
///
/// A published program owns its policy: it writes what to do with metrics as literals, so a change of a native policy
/// setting reaches the default program only, and runs only once the program is revoked, or a program with the new
/// value is published. `differs` says which nodes of the default program that leaves different.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProgramStatus {
    /// Which program runs: the default program, or a published one.
    pub runs: ProgramRuns,

    /// Whether the native policy settings apply: always for the default program, never for a published one.
    pub native_policy: NativePolicy,

    /// The nodes of the default program, by name, whose parameters come from native policy settings and that the
    /// running program does not run as the default program writes them now: left out while they do something, or with
    /// other parameters. Empty for the default program. A host that cannot tell leaves it empty.
    #[serde(default)]
    pub differs: Vec<LocalId>,
}

/// Which program the active generation runs.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgramRuns {
    /// The default program, lowered from native configuration.
    #[default]
    Default,

    /// A program published through the channel: the active artifact's.
    Published,

    /// A value a later host writes that this reader does not know.
    #[serde(other)]
    Unknown,
}

/// Whether the host's native policy settings apply to the program that runs.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NativePolicy {
    /// They apply: the default program runs, and binds them.
    #[default]
    Applied,

    /// They do not: a published program runs, which owns its policy.
    NotApplied,

    /// A value a later host writes that this reader does not know.
    #[serde(other)]
    Unknown,
}

/// The status of one replaceable scope of the default program.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ScopeStatus {
    /// The base program hash of the scope's current export: what an artifact derived from that export records as
    /// `base.program_hash`.
    pub default_program_hash: Option<PlanHash>,

    /// The artifact in effect for the scope, or `None` when the default program's region runs there.
    pub active_artifact: Option<ActiveArtifact>,

    /// Whether the scope's active artifact's base no longer matches the current default program.
    pub stale_base: bool,
}

/// Everything the host reports through the channel: its status summary and the acknowledgment of every config.
///
/// Serialized as the summary's members plus `config_states`. A host's privileged status route may serve it with
/// members of its own added, such as the recipient incarnations its delivery hosts run.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApplyStates {
    /// The status summary.
    #[serde(flatten)]
    pub status: StatusSummary,

    /// The latest acknowledgment of each config seen, at most one per config ID, in config ID order.
    pub config_states: Vec<ConfigAcknowledgment>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const HASH: &str = "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn acknowledgments_and_status_are_written_member_for_member_as_the_stub_protocol_says() {
        let hash: PlanHash = HASH.parse().unwrap();
        let states = ApplyStates {
            status: StatusSummary {
                default_program_hash: Some(hash),
                revision_high_water: Some(7),
                active_generation: Some(12),
                active_plan_hash: Some(hash),
                active_artifact: Some(ActiveArtifact {
                    id: ArtifactConfigId::new("demo"),
                    revision: 7,
                    artifact_hash: hash,
                }),
                stale_base: false,
                stale_review: false,
                degraded: None,
                live_generations: 2,
                scopes: BTreeMap::new(),
                program: Some(ProgramStatus {
                    runs: ProgramRuns::Published,
                    native_policy: NativePolicy::NotApplied,
                    differs: vec!["dsd_prefix_filter_blocklist".parse().unwrap()],
                }),
            },
            config_states: vec![ConfigAcknowledgment {
                product: ArtifactProduct::Policy,
                id: ArtifactConfigId::new("demo"),
                version: 7,
                revision: Some(7),
                artifact_hash: Some(hash),
                state: ApplyState::Active,
                generation: Some(12),
                cut_at: None,
                error: None,
            }],
        };
        let expected = json!({
            "default_program_hash": HASH,
            "revision_high_water": 7,
            "active_generation": 12,
            "active_plan_hash": HASH,
            "active_artifact": {"id": "demo", "revision": 7, "artifact_hash": HASH},
            "stale_base": false,
            "stale_review": false,
            "degraded": null,
            "live_generations": 2,
            "program": {"runs": "published", "native_policy": "not_applied", "differs": ["dsd_prefix_filter_blocklist"]},
            "config_states": [{
                "product": "policy", "id": "demo", "version": 7, "revision": 7, "artifact_hash": HASH,
                "state": "active", "generation": 12, "cut_at": null, "error": null
            }]
        });
        assert_eq!(serde_json::to_value(&states).unwrap(), expected);
        assert_eq!(serde_json::from_value::<ApplyStates>(expected).unwrap(), states);
    }

    #[test]
    fn each_scopes_status_is_written_member_for_member_and_left_out_when_there_is_none() {
        let hash: PlanHash = HASH.parse().unwrap();
        let scope = |name: &str| ScopeName::new(name).unwrap();
        let summary = StatusSummary {
            scopes: BTreeMap::from([
                (
                    scope("other"),
                    ScopeStatus {
                        default_program_hash: Some(hash),
                        active_artifact: Some(ActiveArtifact {
                            id: ArtifactConfigId::new("demo-other"),
                            revision: 6,
                            artifact_hash: hash,
                        }),
                        stale_base: true,
                    },
                ),
                (scope(POLICY_SCOPE), ScopeStatus::default()),
            ]),
            ..StatusSummary::default()
        };
        let document = serde_json::to_value(&summary).unwrap();
        assert_eq!(
            document["scopes"],
            json!({
                "other": {
                    "default_program_hash": HASH,
                    "active_artifact": {"id": "demo-other", "revision": 6, "artifact_hash": HASH},
                    "stale_base": true
                },
                "metrics": {"default_program_hash": null, "active_artifact": null, "stale_base": false}
            })
        );
        assert_eq!(serde_json::from_value::<StatusSummary>(document).unwrap(), summary);
        // Without scopes, the summary is written as the first version of the protocol writes it, and reads back.
        let document = serde_json::to_value(StatusSummary::default()).unwrap();
        assert!(document.get("scopes").is_none());
        assert_eq!(
            serde_json::from_value::<StatusSummary>(document).unwrap(),
            StatusSummary::default()
        );
    }

    #[test]
    fn apply_states_are_named_as_the_stub_protocol_names_them() {
        let states = [
            (ApplyState::Pending, "pending"),
            (ApplyState::Prepared, "prepared"),
            (ApplyState::Active, "active"),
            (ApplyState::Retired, "retired"),
            (ApplyState::Rejected, "rejected"),
            (ApplyState::Failed, "failed"),
        ];
        for (state, name) in states {
            assert_eq!(serde_json::to_value(state).unwrap(), name);
            assert_eq!(state.as_str(), name);
        }
    }
}
