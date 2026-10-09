//! The `aggregate` operator.

use std::{collections::BTreeSet, num::NonZeroU64};

use saluki_metric_kernels::{
    aggregate::{AggregateParams, HistogramConfiguration},
    params::{HistogramAggregate, HistogramParams, MAX_PERCENTILE},
};

use super::{
    contract::{
        Boundary, Cardinality, EffectContract, Identity, KeyedState, Loss, LossClass, LossDecl, OrderContract, PerKind,
        Reducer, ReducerKey, StateContract, StateTime,
    },
    names,
    ports::{KindMap, PortMapping, Ports, TimestampMap},
    support::{check_whole_seconds, error},
    OperatorDef,
};
use crate::{
    canonical::MAX_SAFE_INTEGER,
    extension::AdmissionPolicy,
    ir::{DecodedParams, ParamError, ParamSpec, ParamType, ParamValue},
    model::{Field, FieldSet, Kind, KindSet, PortType, TimestampForm},
};

/// The names of the histogram aggregates the `histogram_aggregates` parameter accepts.
pub const HISTOGRAM_AGGREGATES: &[&str] = &["count", "sum", "min", "max", "avg", "median"];

const AGGREGATE_PARAMS: &[ParamSpec] = &[
    ParamSpec::required("window", ParamType::Bindable(&ParamType::Duration)),
    ParamSpec::required("flush_interval", ParamType::Bindable(&ParamType::Duration)),
    ParamSpec::required(
        "context_limit",
        ParamType::Bindable(&ParamType::Integer {
            min: 0,
            max: MAX_SAFE_INTEGER,
        }),
    ),
    ParamSpec::required("counter_expiry", ParamType::Bindable(&ParamType::Duration)),
    ParamSpec::required("flush_open_windows", ParamType::Bindable(&ParamType::Bool)),
    ParamSpec::required(
        "histogram_aggregates",
        ParamType::Bindable(&ParamType::List(&ParamType::Enum(HISTOGRAM_AGGREGATES))),
    ),
    ParamSpec::required(
        "histogram_percentiles",
        ParamType::Bindable(&ParamType::List(&ParamType::Integer {
            min: 0,
            max: MAX_PERCENTILE as i64,
        })),
    ),
    ParamSpec::required("copy_to_distribution", ParamType::Bindable(&ParamType::Bool)),
    ParamSpec::required("copy_to_distribution_prefix", ParamType::Bindable(&ParamType::String)),
];

/// The most series one histogram can expand into per bucket: every aggregate, every percentile, and the distribution
/// copy.
const MAX_HISTOGRAM_SERIES: u32 = HISTOGRAM_AGGREGATES.len() as u32 + MAX_PERCENTILE as u32 + 1 + 1;

/// Decodes the known histogram parameters of an `aggregate` node.
///
/// Returns `None` if any of them is an unresolved binding.
pub fn histogram_params(params: &DecodedParams) -> Option<HistogramParams> {
    let aggregates = params
        .list("histogram_aggregates")?
        .iter()
        .map(|value| match value {
            ParamValue::Enum(name) => HistogramAggregate::from_name(name),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    let percentiles = params
        .list("histogram_percentiles")?
        .iter()
        .map(|value| match value {
            ParamValue::Integer(percentile) => u8::try_from(*percentile).ok(),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    Some(HistogramParams {
        aggregates,
        percentiles,
        copy_to_distribution: params.bool("copy_to_distribution")?,
        copy_to_distribution_prefix: params.str("copy_to_distribution_prefix")?.to_string(),
    })
}

/// Decodes the known parameters of an `aggregate` node into the parameters of native aggregation.
///
/// Returns `None` if any of them is an unresolved binding. The flush interval is not among them: it schedules flushes,
/// which is the host's job. A counter expiry of `0s` keeps counters forever.
pub fn aggregate_params(params: &DecodedParams) -> Option<AggregateParams> {
    let histograms = histogram_params(params)?;
    Some(AggregateParams {
        window: NonZeroU64::new(params.duration("window")?.as_secs())?,
        context_limit: usize::try_from(params.integer("context_limit")?).ok()?,
        counter_expiry: NonZeroU64::new(params.duration("counter_expiry")?.as_secs()),
        flush_open_windows: params.bool("flush_open_windows")?,
        histograms: HistogramConfiguration::from_params(&histograms),
    })
}

/// The `aggregate` operator: native aggregation of each context into time buckets.
///
/// Parameters, all bindable:
///
/// - `window`: the bucket width, a whole number of seconds, at least one;
/// - `flush_interval`: how often closed buckets are flushed, at least one second;
/// - `context_limit`: the most contexts held, including idle counters; new contexts are dropped while it is reached;
/// - `counter_expiry`: how long, in whole seconds, an idle counter keeps emitting zeros; `0s` means counters never
///   expire;
/// - `flush_open_windows`: the native setting of that name, which has no observable effect;
/// - `histogram_aggregates` and `histogram_percentiles`: the statistics computed for each histogram, in emission
///   order (see [`HistogramParams`]), each at most once;
/// - `copy_to_distribution` and `copy_to_distribution_prefix`: whether each histogram is also emitted as a
///   distribution, and the prefix of its name.
///
/// Every output point is timestamped with its bucket start. Counters leave as rates over the window, and histograms
/// expand into one series per statistic, so the output carries rates, gauges, sets, and distributions only. Native
/// aggregation runs in its own host component until it is extracted as a kernel: it assigns buckets by the host's clock
/// when a batch arrives, not by the batch's admission time.
#[derive(Clone, Copy, Debug, Default)]
pub struct Aggregate;

impl OperatorDef for Aggregate {
    fn name(&self) -> &'static str {
        names::AGGREGATE
    }

    fn summary(&self) -> &'static str {
        "Aggregates each context into time buckets, natively: counters become rates and histograms expand into series."
    }

    fn params(&self) -> &'static [ParamSpec] {
        AGGREGATE_PARAMS
    }

    fn check_params(&self, params: &DecodedParams) -> Vec<ParamError> {
        let mut errors = Vec::new();
        check_whole_seconds("window", params.duration("window"), false, &mut errors);
        if params
            .duration("flush_interval")
            .is_some_and(|interval| interval.is_zero())
        {
            errors.push(error("flush_interval", "must be at least one second"));
        }
        check_whole_seconds("counter_expiry", params.duration("counter_expiry"), true, &mut errors);

        for name in ["histogram_aggregates", "histogram_percentiles"] {
            let Some(values) = params.list(name) else { continue };
            let mut seen = BTreeSet::new();
            for (index, value) in values.iter().enumerate() {
                let key = value.encode().to_string();
                if !seen.insert(key.clone()) {
                    errors.push(error(
                        &format!("{}[{}]", name, index),
                        format!(
                            "{} is listed more than once, which would emit the same series twice per flush",
                            key
                        ),
                    ));
                }
            }
        }
        errors
    }

    fn ports(&self, _params: &DecodedParams) -> Ports {
        // Counters leave as rates. A histogram becomes a rate for its count, a gauge for every other statistic, and,
        // when copied, a distribution; which of these appear depends on parameters that can change between
        // generations, so the port declares all of them.
        let kinds = KindMap::IDENTITY.with(Kind::Counter, KindSet::of(&[Kind::Rate])).with(
            Kind::Histogram,
            KindSet::of(&[Kind::Rate, Kind::Gauge, Kind::Distribution]),
        );
        Ports::single(
            PortType::ANY,
            PortMapping {
                kinds,
                timestamps: TimestampMap::Set(TimestampForm::Timestamped),
            },
        )
    }

    fn contract(&self, params: &DecodedParams) -> EffectContract {
        let points = FieldSet::of(&[Field::Points]);
        let writes = PerKind::uniform(points)
            .with(
                KindSet::of(&[Kind::Counter]),
                FieldSet::of(&[Field::Kind, Field::Points]),
            )
            .with(
                KindSet::of(&[Kind::Histogram]),
                FieldSet::of(&[Field::Name, Field::Kind, Field::Metadata, Field::Points]),
            );
        let max = histogram_params(params)
            .and_then(|histograms| u32::try_from(histograms.series_per_histogram()).ok())
            .unwrap_or(MAX_HISTOGRAM_SERIES)
            .max(1);
        // Aggregation runs in an opaque host until it is extracted as a kernel, but it declares this kind-conditional
        // contract rather than reading and writing everything: the duplicate analysis relies on its writes per kind,
        // and its time contract describes the kernel, which buckets by admission time (see `StateTime::Buckets`).
        // Only the expansion maximum of its cardinality applies; see `Cardinality::Expand`.
        EffectContract {
            reads: PerKind::uniform(FieldSet::ALL),
            writes,
            cardinality: Cardinality::Expand { max: Some(max) },
            identity: PerKind::uniform(Identity::Preserving).with(KindSet::of(&[Kind::Histogram]), Identity::Opaque),
            state: StateContract::Keyed(KeyedState {
                key: ReducerKey::Context,
                reducer: Reducer::NativeAggregation,
                time: StateTime::Buckets {
                    width: params.duration("window"),
                },
                admission: AdmissionPolicy::FirstCome,
            }),
            // Gauges keep their latest value, the first metadata seen is kept, and contexts are admitted first come.
            order: OrderContract::Dependent,
            loss: Loss::Lossy(vec![
                LossDecl {
                    class: LossClass::Policy,
                    description: "Keeps only the latest of a bucket's untimestamped gauge points, and replaces a \
                                  context's values when a point of another kind, or a rate with another interval, \
                                  arrives.",
                },
                LossDecl {
                    class: LossClass::Capacity,
                    description: "Drops the points of new contexts while the context limit is reached.",
                },
                LossDecl {
                    class: LossClass::Shutdown,
                    description: "Discards the buckets still open at shutdown.",
                },
            ]),
            boundary: Boundary::OpaqueHost,
            ..EffectContract::passthrough()
        }
    }
}
