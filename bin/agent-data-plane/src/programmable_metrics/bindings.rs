//! The closed registry of configuration bindings that plan parameters may name.
//!
//! A plan parameter bound to configuration names a binding, `{"config": "<binding>"}`, never a configuration key. This
//! module is the only place that maps binding names to what the host knows:
//!
//! - **Every binding has a class** ([`BindingClass`]): a **fact** of the host that a program does not choose (its tags,
//!   whether a failover endpoint is configured), or a **signal** it reacts to (a failover in progress). What to do with
//!   metrics is never a binding: a program writes its policy as literals, and so does the default program, whose
//!   [lowering](super::lowering) writes every native policy setting as its value.
//! - **Names** of facts of the host that configuration does not hold live under `host.`, such as `host.tags`, and
//!   resolve from the [host facts](HostFacts). The others are typed-model paths, below the model's top-level section
//!   (`domains.multi_region_failover.enabled` is `multi_region_failover.enabled`), never Datadog keys.
//! - **Liveness follows native behavior.** `Live` bindings are the switches of the failover branches, which follow the
//!   current configuration since outputs became live; host facts are `Startup`, fixed for the life of the process.
//! - **Credentials are never bindable.** No binding reads an API key, a token, or a secret.
//!
//! Each binding's value is written as the parameter that names it reads it, normalized by decoding and re-encoding it
//! as the binding's type, so that every lowering of the same configuration writes, and hashes, the same plan.

use agent_data_plane_config::SalukiConfiguration;
use saluki_metric_plan::{
    ir::{format_duration, BindingName, ParamType, ParamValue},
    validate::{BindingClass, Liveness},
};
use serde_json::{json, Value};

use super::host_facts::HostFacts;

const STRINGS: ParamType = ParamType::List(&ParamType::String);

/// The configuration snapshots, and the host facts, bindings resolve from.
#[derive(Clone, Copy, Debug)]
pub struct Snapshots<'a> {
    /// The configuration the process started with, which host facts resolve with.
    pub startup: &'a SalukiConfiguration,

    /// The current configuration, which `Live` bindings resolve from.
    pub current: &'a SalukiConfiguration,

    /// The host facts, captured at boot, which facts of the host resolve from.
    pub facts: &'a HostFacts,
}

/// How a binding's value is resolved.
#[derive(Clone, Copy, Debug)]
pub enum Resolve {
    /// The value is read from the current configuration.
    Value(fn(&SalukiConfiguration) -> Option<Value>),

    /// The value is a fact of the host, read from the host facts and the startup configuration: a `Startup` binding,
    /// fixed for the life of the process.
    ///
    /// Returns `None` if the host has no such fact, such as host tags when host tags enrichment is off.
    Fact(fn(&HostFacts, &SalukiConfiguration) -> Option<Value>),
}

/// One entry of the binding registry.
#[derive(Clone, Copy, Debug)]
pub struct BindingSpec {
    /// The binding's name.
    pub name: &'static str,

    /// The type of the binding's value. A parameter bound to it must admit this type.
    pub ty: ParamType,

    /// Whether the value can change while the process runs.
    pub liveness: Liveness,

    /// What the value is to a program: a fact of the host, or a signal it reacts to.
    pub class: BindingClass,

    /// What the binding is, in one sentence, naming the native setting it comes from: tools show it beside a program.
    pub about: &'static str,

    /// How the value is resolved.
    pub resolve: Resolve,
}

impl BindingSpec {
    /// Resolves the value of this binding.
    ///
    /// The value is written in its canonical form. A value that is not of the binding's type is returned as it is, so
    /// that validating a plan that uses it reports the problem.
    ///
    /// Returns `None` if the host has no value for it, such as host tags when host tags enrichment is off.
    pub fn resolve(&self, snapshots: Snapshots<'_>) -> Option<Value> {
        let value = match self.resolve {
            Resolve::Value(read) => read(snapshots.current)?,
            Resolve::Fact(read) => read(snapshots.facts, snapshots.startup)?,
        };
        Some(match ParamValue::decode(&self.ty, &value, self.name) {
            Ok(decoded) => decoded.encode(),
            Err(_) => value,
        })
    }
}

/// The binding registry: every configuration binding a plan parameter may name.
pub static BINDINGS: &[BindingSpec] = &[
    // Multi-region failover: whether it is configured is live, as the branch, which exists only while it is; whether
    // it mirrors metrics now is live, as natively.
    BindingSpec {
        name: "multi_region_failover.enabled",
        ty: ParamType::Bool,
        liveness: Liveness::Live,
        class: BindingClass::Fact,
        about: "Whether the Multi-Region Failover endpoint is configured, from `multi_region_failover.enabled`.",
        resolve: Resolve::Value(|config| Some(json!(config.domains.multi_region_failover.enabled))),
    },
    BindingSpec {
        name: "multi_region_failover.metric_mirroring.enabled",
        ty: ParamType::Bool,
        liveness: Liveness::Live,
        class: BindingClass::Signal,
        about: "Whether a regional failover mirrors metrics now, from `multi_region_failover.failover_metrics`.",
        resolve: Resolve::Value(|config| Some(json!(config.domains.multi_region_failover.metric_mirroring.enabled))),
    },
    // Autoscaling failover: live, as the branch, which exists only while it is enabled and its list is not empty.
    BindingSpec {
        name: "autoscaling_failover.enabled",
        ty: ParamType::Bool,
        liveness: Liveness::Live,
        class: BindingClass::Signal,
        about: "Whether autoscaling failover sends metrics to the Cluster Agent, from `autoscaling.failover.enabled`.",
        resolve: Resolve::Value(|config| Some(json!(config.shared.autoscaling_failover.enabled))),
    },
    // Facts of the host: its tags, fetched from the Core Agent at boot, and the instant until which they are added.
    BindingSpec {
        name: "host.tags",
        ty: STRINGS,
        liveness: Liveness::Startup,
        class: BindingClass::Fact,
        about: "The host's tags, fetched from the Core Agent at boot.",
        resolve: Resolve::Fact(|facts, _| {
            let tags = facts.host_tags.as_ref()?;
            Some(json!(tags.into_iter().map(|tag| tag.as_str()).collect::<Vec<_>>()))
        }),
    },
    BindingSpec {
        name: "host.tags_deadline",
        ty: ParamType::Duration,
        liveness: Liveness::Startup,
        class: BindingClass::Fact,
        about: "When host tags stop being added: the process start plus `expected_tags_duration`.",
        resolve: Resolve::Fact(|facts, startup| {
            let deadline = facts.host_tags_deadline(startup.shared.tags.expected_tags_duration);
            format_duration(deadline).map(Value::String)
        }),
    },
];

/// Returns the registry entry for the binding `name`, if the registry has it.
pub fn lookup(name: &BindingName) -> Option<&'static BindingSpec> {
    let name = name.to_string();
    BINDINGS.iter().find(|spec| spec.name == name)
}

/// Returns what the binding `name` is, as tools show it beside a program: its class and what it is, such as "fact: The
/// host's tags, fetched from the Core Agent at boot.", if the registry has it.
pub fn annotation(name: &BindingName) -> Option<String> {
    let spec = lookup(name)?;
    Some(format!("{}: {}", spec.class.as_str(), spec.about))
}

/// Resolves the value of the binding `name`.
///
/// Returns `None` if the registry has no such binding, or if the host has no value for it.
pub fn resolve(name: &BindingName, snapshots: Snapshots<'_>) -> Option<Value> {
    lookup(name)?.resolve(snapshots)
}

#[cfg(test)]
mod tests;
