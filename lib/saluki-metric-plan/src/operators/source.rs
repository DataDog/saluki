//! The `source` operator.

use super::{
    contract::{Boundary, Cardinality, EffectContract, Identity, PerKind},
    names,
    ports::{OutputPort, PortMapping, Ports},
    support::error,
    OperatorDef,
};
use crate::{
    ir::{DecodedParams, ParamError, ParamSpec, ParamType, PortName},
    model::{FieldSet, Kind, KindSet, PortType, TimestampForm},
};

/// The output port of every source.
pub const METRICS_PORT: &str = "metrics";

/// A host source whose metrics can enter a plan.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SourceBinding {
    /// The DogStatsD listener, after decoding.
    Dogstatsd,

    /// The OTLP receiver, after translation into metrics.
    Otlp,

    /// Metrics submitted by checks.
    Checks,

    /// The process's own liveness metric.
    Liveness,

    /// Metrics produced by trace sampling.
    TraceSampler,
}

impl SourceBinding {
    /// Every binding, in declaration order.
    pub const ALL: [Self; 5] = [
        Self::Dogstatsd,
        Self::Otlp,
        Self::Checks,
        Self::Liveness,
        Self::TraceSampler,
    ];

    const NAMES: &'static [&'static str] = &["dogstatsd", "otlp", "checks", "liveness", "trace_sampler"];

    /// Returns the binding's name, as the `binding` parameter names it.
    pub const fn as_str(self) -> &'static str {
        Self::NAMES[self as usize]
    }

    /// Returns the binding with the given name, if there is one.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|binding| binding.as_str() == name)
    }

    /// Returns the kinds of metric the source can produce.
    pub const fn kinds(self) -> KindSet {
        match self {
            // DogStatsD has no rate type: counts, gauges, sets, histograms and timers, and distributions.
            Self::Dogstatsd => KindSet::of(&[
                Kind::Counter,
                Kind::Gauge,
                Kind::Set,
                Kind::Histogram,
                Kind::Distribution,
            ]),
            // Translation produces counts, per-second rates, gauges, and sketches.
            Self::Otlp => KindSet::of(&[Kind::Counter, Kind::Rate, Kind::Gauge, Kind::Distribution]),
            Self::Checks => KindSet::of(&[Kind::Counter, Kind::Rate, Kind::Gauge, Kind::Histogram]),
            Self::Liveness => KindSet::of(&[Kind::Gauge]),
            Self::TraceSampler => KindSet::of(&[Kind::Counter, Kind::Gauge]),
        }
    }

    /// Returns the timestamp form of the source's metrics, or `None` if it depends on how the source is configured, as
    /// the `timestamps` parameter then states.
    pub const fn timestamps(self) -> Option<TimestampForm> {
        match self {
            // DogStatsD parses client timestamps only when configured to.
            Self::Dogstatsd => None,
            // A zero timestamp means the point has none.
            Self::Otlp | Self::Checks => Some(TimestampForm::Mixed),
            Self::Liveness => Some(TimestampForm::Timestamped),
            Self::TraceSampler => Some(TimestampForm::Untimestamped),
        }
    }
}

const TIMESTAMP_FORMS: &[&str] = &["timestamped", "untimestamped", "mixed"];

const SOURCE_PARAMS: &[ParamSpec] = &[
    ParamSpec::required("binding", ParamType::Enum(SourceBinding::NAMES)),
    ParamSpec::optional("timestamps", ParamType::Enum(TIMESTAMP_FORMS)),
];

fn timestamp_form(name: &str) -> Option<TimestampForm> {
    match name {
        "timestamped" => Some(TimestampForm::Timestamped),
        "untimestamped" => Some(TimestampForm::Untimestamped),
        "mixed" => Some(TimestampForm::Mixed),
        _ => None,
    }
}

/// The `source` operator: where the metrics of one host source enter the plan.
///
/// Parameters: `binding` (`dogstatsd`, `otlp`, `checks`, `liveness`, or `trace_sampler`), and, for `dogstatsd` only,
/// `timestamps`: `mixed` when client timestamps are parsed, as the no-aggregation pipeline requires, and
/// `untimestamped` otherwise. The output port `metrics` carries the kinds the binding produces. The source itself is
/// an I/O host: decoding and the filters inside it are host behavior, outside the plan.
#[derive(Clone, Copy, Debug, Default)]
pub struct Source;

impl Source {
    fn port_type(params: &DecodedParams) -> PortType {
        let Some(binding) = params.str("binding").and_then(SourceBinding::from_name) else {
            return PortType::ANY;
        };
        let timestamps = binding
            .timestamps()
            .or_else(|| params.str("timestamps").and_then(timestamp_form))
            .unwrap_or(TimestampForm::Mixed);
        PortType {
            kinds: binding.kinds(),
            timestamps,
        }
    }
}

impl OperatorDef for Source {
    fn name(&self) -> &'static str {
        names::SOURCE
    }

    fn summary(&self) -> &'static str {
        "Where the metrics of one host source enter the plan."
    }

    fn params(&self) -> &'static [ParamSpec] {
        SOURCE_PARAMS
    }

    fn check_params(&self, params: &DecodedParams) -> Vec<ParamError> {
        let Some(binding) = params.str("binding").and_then(SourceBinding::from_name) else {
            return Vec::new();
        };
        let timestamps = params.str("timestamps");
        match (binding.timestamps(), timestamps) {
            (None, None) => vec![error(
                "timestamps",
                format!(
                    "the `{}` source requires `timestamps`: `mixed` when client timestamps are parsed, or \
                     `untimestamped`",
                    binding.as_str()
                ),
            )],
            (None, Some("timestamped")) => vec![error(
                "timestamps",
                format!(
                    "the `{}` source's metrics are never all timestamped; use `mixed` or `untimestamped`",
                    binding.as_str()
                ),
            )],
            (Some(_), Some(_)) => vec![error(
                "timestamps",
                format!(
                    "the `{}` source's timestamp form is fixed; remove `timestamps`",
                    binding.as_str()
                ),
            )],
            _ => Vec::new(),
        }
    }

    fn ports(&self, _params: &DecodedParams) -> Ports {
        Ports {
            inputs: Vec::new(),
            outputs: vec![OutputPort {
                name: PortName::from_static(METRICS_PORT),
                mapping: PortMapping::IDENTITY,
            }],
        }
    }

    fn contract(&self, _params: &DecodedParams) -> EffectContract {
        EffectContract {
            // A source creates its records, so it is the first writer of every field.
            writes: PerKind::uniform(FieldSet::ALL),
            cardinality: Cardinality::Source,
            identity: PerKind::uniform(Identity::Opaque),
            boundary: Boundary::IoHost,
            ..EffectContract::passthrough()
        }
    }

    fn output_types(&self, params: &DecodedParams, _ports: &Ports, _inputs: &[PortType]) -> Vec<PortType> {
        vec![Self::port_type(params)]
    }
}
