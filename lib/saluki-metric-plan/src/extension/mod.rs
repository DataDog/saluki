//! Extension slots.
//!
//! The core reserves a slot for each capability that later stages make programmable. Each slot has a fixed contract and
//! a native default that reproduces today's behavior, and the types here accept only that default until the slot
//! opens. Richer designs plug into a slot later without changing the core.
//!
//! - [`DeliveryPolicy`]: what an output or recipient does when it cannot keep up ([`Overflow`]), and when it closes
//!   ([`Closure`]).
//! - [`AdmissionPolicy`]: whether new keys enter a stateful operator's state.
//! - Signals ([`SignalDecl`]): named, typed state that the host publishes and operators may read.
//! - [`TransitionStrategy`] and [`TransitionClass`]: how state moves between generations.
//! - [`AuthorityScope`]: what an approval covers.
//!
//! **Modules**, reusable and parameterized groups of operators that expand into ordinary operators before validation,
//! are also reserved. They have no types yet: until they exist, every plan is written with expanded operators.

use std::fmt;

use schemars::JsonSchema;
use serde::{de, Deserialize, Deserializer, Serialize};

use crate::ir::SignalName;

/// What an output or recipient does when it is full, and when it closes.
///
/// Only the native policy exists until the slot opens to artifacts.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryPolicy {
    /// The native behavior.
    ///
    /// An output waits for its downstream to accept more. A recipient queues requests in a byte-bounded retry queue
    /// that evicts the oldest entries, or spills them to disk where configured, and sends fresh requests before
    /// retries. On close, in-flight requests finish within the shutdown timeout, and queued work is persisted where
    /// configured and otherwise dropped.
    #[default]
    Native,
}

impl DeliveryPolicy {
    /// Returns `true` if this is the native policy, the default, which serialization leaves out.
    pub fn is_native(&self) -> bool {
        *self == Self::Native
    }

    /// Returns what an output with this policy does when its downstream cannot accept more.
    pub const fn overflow(self) -> Overflow {
        match self {
            Self::Native => Overflow::Wait,
        }
    }

    /// Returns what an output with this policy does with queued work when it closes.
    pub const fn closure(self) -> Closure {
        match self {
            Self::Native => Closure::Native,
        }
    }
}

/// What an output does when its downstream cannot accept more.
///
/// Shedding and spilling are reserved for the stage that opens the delivery-policy slot.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Overflow {
    /// The sender waits until the downstream accepts more, as native components do: a region host awaits each exit's
    /// send in exit order, so one slow output delays the others, as native fan-out does. Nothing is lost.
    Wait,
}

impl Overflow {
    /// Returns the behavior's name, as `explain` shows it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Wait => "wait",
        }
    }
}

/// What an output does with queued work when it closes, at process shutdown or when its generation retires.
///
/// Attempting, persisting, or discarding within a policy's own deadline is reserved for the stage that opens the
/// delivery-policy slot.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Closure {
    /// Native shutdown: in-flight requests finish within the shutdown timeout, and queued work is persisted where
    /// configured, and otherwise dropped. A retiring generation's batches finish in the shared encoder streams, which
    /// do not close, so a live update loses nothing beyond what a native shutdown would.
    Native,
}

impl Closure {
    /// Returns the behavior's name, as `explain` shows it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
        }
    }
}

/// How a stateful operator decides whether a new key enters its state.
///
/// Each admission policy carries its own proof that pushing filters above the operator cannot increase capacity loss.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionPolicy {
    /// Keys are admitted in arrival order until the operator's key limit is reached; later new keys are dropped and
    /// counted as capacity loss until enough keys expire. This is the native behavior.
    #[default]
    FirstCome,
}

/// The type of a signal's value.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalType {
    /// A boolean, such as whether a queue is above its high watermark.
    Bool,

    /// A non-negative count, such as the number of queued bytes.
    Count,

    /// A ratio between 0 and 1, such as a queue's occupancy.
    Ratio,
}

/// The declaration of a named, typed signal that the host publishes.
///
/// Operators declare the signals they read. Reading a signal makes an operator dynamic: its output can change without
/// any change to its input, so rewrites never move it, merge it, or cache its results. Signal changes are trace events,
/// so the reference interpreter can reproduce them.
///
/// No signals are published yet.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SignalDecl {
    /// The signal's name.
    pub name: SignalName,

    /// The type of the signal's value.
    #[serde(rename = "type")]
    pub ty: SignalType,
}

/// How state moves between generations.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionStrategy {
    /// Each state owner is classified as [`TransitionClass::Retain`], [`TransitionClass::Cut`], or
    /// [`TransitionClass::Reject`] by comparing its old and new contracts.
    #[default]
    Native,
}

/// How one state owner moves from one generation to the next.
///
/// Serialized as `retain`, `cut`, or `reject`. The class `split`, versioned windows with merge rules, is reserved for a
/// later stage and rejected when deserializing.
///
/// Classes are ordered from the least to the most restrictive, so the stricter of two classes is their maximum.
#[derive(Clone, Copy, Debug, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionClass {
    /// The old and new contracts are compatible: both generations share one state instance.
    Retain,

    /// The contracts are incompatible, so ownership of state changes at a bucket boundary.
    Cut,

    /// The change cannot be applied while state is retained, and is refused.
    Reject,
}

impl TransitionClass {
    /// Returns the name of this class, as it is serialized.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Retain => "retain",
            Self::Cut => "cut",
            Self::Reject => "reject",
        }
    }
}

impl fmt::Display for TransitionClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for TransitionClass {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match String::deserialize(deserializer)?.as_str() {
            "retain" => Ok(Self::Retain),
            "cut" => Ok(Self::Cut),
            "reject" => Ok(Self::Reject),
            "split" => Err(de::Error::custom(
                "the transition class `split` is reserved for a later stage; use `retain`, `cut`, or `reject`",
            )),
            other => Err(de::Error::custom(format_args!(
                "unknown transition class `{}`; expected `retain`, `cut`, or `reject`",
                other
            ))),
        }
    }
}

/// What an approval covers.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorityScope {
    /// An approval covers a whole artifact: every node it adds or changes gets the artifact's provenance.
    #[default]
    WholeArtifact,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn only_native_defaults_are_accepted() {
        assert_eq!(DeliveryPolicy::default(), DeliveryPolicy::Native);
        assert_eq!(serde_json::to_value(DeliveryPolicy::Native).unwrap(), json!("native"));
        assert!(serde_json::from_value::<DeliveryPolicy>(json!("drop_oldest")).is_err());
        assert_eq!(DeliveryPolicy::Native.overflow(), Overflow::Wait);
        assert_eq!(DeliveryPolicy::Native.closure(), Closure::Native);

        assert_eq!(AdmissionPolicy::default(), AdmissionPolicy::FirstCome);
        assert_eq!(
            serde_json::to_value(AdmissionPolicy::FirstCome).unwrap(),
            json!("first_come")
        );
        assert!(serde_json::from_value::<AdmissionPolicy>(json!("evict_lru")).is_err());

        assert_eq!(TransitionStrategy::default(), TransitionStrategy::Native);
        assert_eq!(AuthorityScope::default(), AuthorityScope::WholeArtifact);
        assert_eq!(
            serde_json::to_value(AuthorityScope::WholeArtifact).unwrap(),
            json!("whole_artifact")
        );
    }

    #[test]
    fn transition_classes_reserve_split() {
        for class in [TransitionClass::Retain, TransitionClass::Cut, TransitionClass::Reject] {
            let value = serde_json::to_value(class).unwrap();
            assert_eq!(value, json!(class.as_str()));
            assert_eq!(serde_json::from_value::<TransitionClass>(value).unwrap(), class);
        }

        let error = serde_json::from_value::<TransitionClass>(json!("split"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("reserved for a later stage"), "{}", error);
        let error = serde_json::from_value::<TransitionClass>(json!("merge"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown transition class `merge`"), "{}", error);
    }

    #[test]
    fn signal_declarations_round_trip() {
        let value = json!({"name": "forwarder_queue_occupancy", "type": "ratio"});
        let declaration = serde_json::from_value::<SignalDecl>(value.clone()).unwrap();
        assert_eq!(declaration.ty, SignalType::Ratio);
        assert_eq!(serde_json::to_value(&declaration).unwrap(), value);
        assert!(serde_json::from_value::<SignalDecl>(json!({"name": "Bad", "type": "ratio"})).is_err());
    }
}
