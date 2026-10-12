//! Resources: what an output delivers through, as plan parameters.
//!
//! A plan carries every resource its outputs resolve to, so that a change of an endpoint, a credential reference, an
//! encoder setting, or a forwarder setting lowers into a new plan hash, goes through prepare and commit, and shows in
//! the diff and in `explain`:
//!
//! - each [`OutputDecl`](super::OutputDecl) bound to a delivery host carries its [`OutputBinding`]: the host, the
//!   [`Selector`] its fingerprint covers, its encoder parameters, and the identity its encoder telemetry reports under;
//! - the plan member [`Resources`] holds each delivery host's shared settings ([`HostBinding`]) and every recipient the
//!   plan's outputs and native recipient sets name ([`RecipientDecl`]).
//!
//! The plan crate is protocol-agnostic: encoder parameters, host settings, and recipient protocols are typed
//! [`Params`] that a schema the host provides validates (`Catalog::host`), so the crate never reads their meaning.
//! Credentials stay references ([`CredentialRef`]): a plan never contains a credential's value.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    id::{CredentialRef, HostId, NativeComponentId, RecipientId, RoleName},
    params::Params,
    plan::ArtifactRevision,
};
use crate::{canonical::PlanHash, extension::DeliveryPolicy};

/// The key of a routing group: a digest of the set of endpoints the group delivers to.
pub type GroupKey = PlanHash;

/// An output's resolved binding: where and how the records emitted to it are encoded and delivered.
///
/// An output with a binding runs as a **lease** in the encode host of its delivery host: one encoder stream with the
/// binding's parameters. A binding change creates a new lease, and the old one drains once no generation uses it.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OutputBinding {
    /// The delivery host the output delivers through.
    pub host: HostId,

    /// What the output's declaration says it delivers to, which its fingerprint covers.
    pub selector: Selector,

    /// The encoder's parameters, as the host's schema declares them: protocol variants, compression, payload limits,
    /// and flush timeout, for example.
    #[serde(default, skip_serializing_if = "Params::is_empty")]
    pub encoder: Params,

    /// The component identifier the output's encoder telemetry reports under.
    pub telemetry: NativeComponentId,

    /// Who declared the output: native configuration, or a resources artifact.
    pub provenance: ResourceProvenance,
}

/// What an output's declaration says it delivers to.
///
/// An output's fingerprint covers its selector, not its resolved recipients: a role delivers to whatever native
/// configuration resolves for it, so its recipients can change without changing its fingerprint, while an explicit
/// recipient list is part of the declaration.
///
/// Serialized as an object with one member: `{"role": "main"}`, `{"group": "sha256:<hex>"}`, or
/// `{"declared": [<recipient ID>, ...]}`.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Selector {
    /// A role of the delivery host, such as `main`, `multi_region_failover`, or `autoscaling_failover`.
    Role(RoleName),

    /// A routing group, by its key.
    Group(GroupKey),

    /// The recipients a resources artifact declared for the output, sorted and without duplicates.
    Declared(Vec<RecipientId>),
}

/// The resources of a plan: the settings of each delivery host, and every recipient the plan names.
///
/// Serialized as the plan member `resources`, left out when empty.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    /// The shared settings of each delivery host, at most one per host.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<HostBinding>,

    /// Every recipient the plan's outputs, and the native recipient sets of its delivery hosts, name, at most one per
    /// recipient ID.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recipients: Vec<RecipientDecl>,
}

impl Resources {
    /// Returns `true` if the plan declares no resources, which serialization leaves out.
    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty() && self.recipients.is_empty()
    }

    /// Returns the settings of the given delivery host, if the plan declares them.
    pub fn host(&self, host: &HostId) -> Option<&HostBinding> {
        self.hosts.iter().find(|binding| &binding.host == host)
    }

    /// Returns the recipient with the given identifier, if the plan declares it.
    pub fn recipient(&self, id: &RecipientId) -> Option<&RecipientDecl> {
        self.recipients.iter().find(|recipient| &recipient.id == id)
    }
}

/// A delivery host's shared settings: its client profile (proxies, TLS, timeouts, HTTP protocol), and its retry and
/// concurrency settings, as the host's schema declares them.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostBinding {
    /// The delivery host.
    pub host: HostId,

    /// The host's settings.
    #[serde(default, skip_serializing_if = "Params::is_empty")]
    pub settings: Params,
}

/// One recipient of a delivery host: one endpoint, with its credential, its queue, and its persisted directory.
///
/// Recipients are shared by every output whose binding names them: outputs that share a recipient feed the same
/// delivery queue.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecipientDecl {
    /// The recipient's identity.
    pub id: RecipientId,

    /// The delivery host it belongs to.
    pub host: HostId,

    /// The endpoint, as configured for a native recipient, or in canonical form for a declared one.
    pub endpoint: String,

    /// A reference to the credential the recipient sends with: never the credential's value.
    pub credential: CredentialRef,

    /// Which payloads the recipient receives when a payload does not name its recipients.
    pub classes: PayloadClasses,

    /// The recipient's protocol, as the host's schema declares it: which protocol variants it accepts, and how its
    /// requests are formed, for example.
    #[serde(default, skip_serializing_if = "Params::is_empty")]
    pub protocol: Params,

    /// The recipient's delivery policy: its only knobs beyond its host's settings. Defaults to the native policy.
    #[serde(default, skip_serializing_if = "DeliveryPolicy::is_native")]
    pub delivery: DeliveryPolicy,

    /// Who declared the recipient: native configuration, or a resources artifact.
    pub provenance: ResourceProvenance,
}

impl RecipientDecl {
    /// Returns `true` if the recipient is declared as `other` is, but for its provenance: a recipient declared again
    /// so is the same recipient with the same settings, whoever declared it.
    pub fn same_settings(&self, other: &RecipientDecl) -> bool {
        self.id == other.id
            && self.host == other.host
            && self.endpoint == other.endpoint
            && self.credential == other.credential
            && self.classes == other.classes
            && self.protocol == other.protocol
            && self.delivery == other.delivery
    }

    /// Returns `true` if the recipient has the identity and the protocol `other` has: what a kept generation needs of
    /// a recipient it delivers to, since a recipient's protocol decides which payloads its leases resolved for it.
    pub fn same_protocol(&self, other: &RecipientDecl) -> bool {
        self.id == other.id && self.host == other.host && self.protocol == other.protocol
    }
}

/// Which payloads a recipient receives when a payload does not name its recipients itself.
///
/// Metrics payloads from leases name their recipients, so classes decide where every other payload goes: logs, events,
/// service checks, traces, and APM statistics, for example.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PayloadClasses {
    /// Metrics only: a recipient that only metrics are routed to, such as one a resources artifact declares.
    MetricsOnly,

    /// Every class of payload, as an additional endpoint natively receives.
    All,
}

/// Who declared a resource, which is also the authority that can remove it.
///
/// Serialized as `"native"`, or as `{"declared": {"revision": <n>, "hash": "<hash>"}}`.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ResourceProvenance {
    /// Native configuration.
    Native,

    /// A resources artifact, by its revision and hash.
    Declared(ArtifactRevision),
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const HASH: &str = "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn selectors_and_provenance_are_written_as_one_member_objects() {
        let role = Selector::Role(RoleName::new("main").unwrap());
        assert_eq!(serde_json::to_value(&role).unwrap(), json!({"role": "main"}));
        let group = Selector::Group(HASH.parse().unwrap());
        assert_eq!(serde_json::to_value(&group).unwrap(), json!({"group": HASH}));
        let declared = Selector::Declared(vec![
            RecipientId::new("dd_out/declared/team_d/http://127.0.0.1:17105").unwrap()
        ]);
        assert_eq!(
            serde_json::to_value(&declared).unwrap(),
            json!({"declared": ["dd_out/declared/team_d/http://127.0.0.1:17105"]})
        );
        for selector in [role, group, declared] {
            let value = serde_json::to_value(&selector).unwrap();
            assert_eq!(serde_json::from_value::<Selector>(value).unwrap(), selector);
        }
        assert!(serde_json::from_value::<Selector>(json!({"role": "main", "group": HASH})).is_err());

        assert_eq!(
            serde_json::to_value(ResourceProvenance::Native).unwrap(),
            json!("native")
        );
        let declared = ResourceProvenance::Declared(ArtifactRevision {
            revision: 3,
            hash: HASH.parse().unwrap(),
        });
        let value = json!({"declared": {"revision": 3, "hash": HASH}});
        assert_eq!(serde_json::to_value(&declared).unwrap(), value);
        assert_eq!(serde_json::from_value::<ResourceProvenance>(value).unwrap(), declared);
    }

    #[test]
    fn a_recipient_leaves_out_its_defaults_and_reads_them_back() {
        let value = json!({
            "id": "dd_out/primary/0/https://app.datadoghq.com",
            "host": "dd_out",
            "endpoint": "https://app.datadoghq.com",
            "credential": "native:api_key",
            "classes": "all",
            "provenance": "native"
        });
        let recipient = serde_json::from_value::<RecipientDecl>(value.clone()).unwrap();
        assert!(recipient.protocol.is_empty());
        assert!(recipient.delivery.is_native());
        assert_eq!(serde_json::to_value(&recipient).unwrap(), value);

        let resources = Resources {
            hosts: Vec::new(),
            recipients: vec![recipient.clone()],
        };
        assert!(!resources.is_empty());
        assert_eq!(resources.recipient(&recipient.id), Some(&recipient));
        assert!(Resources::default().is_empty());
        assert_eq!(serde_json::to_value(Resources::default()).unwrap(), json!({}));
    }
}
