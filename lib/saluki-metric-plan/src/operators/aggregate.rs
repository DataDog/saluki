//! The `aggregate` operator.

use std::{any::Any, collections::BTreeSet, num::NonZeroU64, sync::Arc, time::Duration};

use saluki_core::{accounting::MemoryBoundsBuilder, observability::ComponentMetricsExt as _};
use saluki_metric_kernels::{
    aggregate::{
        snapshot_usage, state_usage, AggregateKernel, AggregateParams, AggregateStepParams, AggregateTelemetry,
        ContextBudget, EmissionFilter, HistogramConfiguration,
    },
    params::{HistogramAggregate, HistogramParams, MAX_PERCENTILE},
};
use saluki_metrics::MetricsBuilder;

use super::{
    contract::{
        Boundary, Cardinality, EffectContract, Identity, KeyedState, Loss, LossClass, LossDecl, OrderContract, PerKind,
        Reducer, ReducerKey, StateContract, StateTime,
    },
    names,
    ports::{KindMap, PortMapping, Ports, TimestampMap},
    support::{check_bucket_width, check_whole_seconds, error, unresolved},
    KernelSpec, OperatorDef,
};
use crate::{
    canonical::MAX_SAFE_INTEGER,
    extension::AdmissionPolicy,
    ir::{DecodedParams, ParamError, ParamName, ParamSpec, ParamType, ParamValue, Params},
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
/// copy. A drain never splits one context's expansion, so one drain step holds at most this many records beyond its
/// budget.
pub const MAX_HISTOGRAM_SERIES: u32 = HISTOGRAM_AGGREGATES.len() as u32 + MAX_PERCENTILE as u32 + 1 + 1;

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

/// Returns the known context limit of an `aggregate` node, or `None` if it is bound to configuration without a resolved
/// value.
fn context_limit(params: &DecodedParams) -> Option<usize> {
    params
        .integer("context_limit")
        .and_then(|limit| usize::try_from(limit).ok())
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
/// The window must be an allowed bucket width ([`ALLOWED_BUCKET_WIDTHS`][crate::limits::ALLOWED_BUCKET_WIDTHS]), so
/// that a change of width can take effect at a boundary both widths share.
///
/// Every output point is timestamped with its bucket start. Counters leave as rates over the window, and histograms
/// expand into one series per statistic, so the output carries rates, gauges, sets, and distributions only.
///
/// # Kernel and state
///
/// The operator's kernel is native aggregation driven by admission time ([`AggregateKernel`]): a batch is admitted at
/// its admission time, and a flush tick, every `flush_interval`, closes the buckets its host's watermark has passed.
/// Its state is keyed by context ([`StateContract::Keyed`]), and a lineage of it by the window alone
/// ([`lineage_params`][OperatorDef::lineage_params]): every other parameter reaches the kernel with each call, from the
/// generation the call runs under ([`step_params`][OperatorDef::step_params]), so a change of them alone keeps the
/// state. The kernel draws its contexts from the state budget its host gives it, shared by every lineage of the node
/// and every state owner of the host, and owns them alone when built outside any host. Its memory bounds are the
/// kernel itself, and, as state drawn from that budget ([`specify_state_bounds`][OperatorDef::specify_state_bounds]),
/// its state map and a snapshot of it at its context limit.
///
/// Its boundary is a declared isolation fence ([`Boundary::Isolated`]): it never fuses with other operators, and runs
/// in an isolation slot of its own, whose region host drives its ticks from the watermark of its upstream admission
/// points.
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
        check_bucket_width("window", params.duration("window"), &mut errors);
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
        // A kind-conditional contract rather than reading and writing everything: the duplicate analysis and R5b rely
        // on its writes per kind, and its time contract describes the kernel, which buckets by admission time (see
        // `StateTime::Buckets`). Only the expansion maximum of its cardinality applies; see `Cardinality::Expand`.
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
                emission_filter: true,
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
            // A declared isolation fence: aggregation runs as a kernel in an isolation slot of its own, never fused.
            boundary: Boundary::Isolated,
            ..EffectContract::passthrough()
        }
    }

    fn specify_bounds(&self, params: &DecodedParams, builder: &mut MemoryBoundsBuilder) {
        // A context limit bound to configuration and not resolved has no size: its kernel cannot be built either.
        if context_limit(params).is_none() {
            return;
        }
        // The state map starts empty and grows only by contexts drawn from the host's state budget, which
        // `specify_state_bounds` declares.
        builder.minimum().with_single_value::<AggregateKernel>("kernel struct");
    }

    fn specify_state_bounds(&self, params: &DecodedParams, builder: &mut MemoryBoundsBuilder) {
        let Some(limit) = context_limit(params) else {
            return;
        };
        builder
            .firm()
            // The aggregation state map, as native aggregation accounts it.
            .with_expr(state_usage(limit))
            // A snapshot is built while the state stays live, so its peak allocation is additive.
            .with_expr(snapshot_usage(limit));
    }

    fn kernel(&self, params: &DecodedParams) -> Option<KernelSpec> {
        let aggregate = aggregate_params(params).zip(params.duration("flush_interval"));
        // The first parameter bound to configuration without a resolved value, which a kernel cannot be built without.
        let unresolved_param = AGGREGATE_PARAMS
            .iter()
            .map(|spec| spec.name)
            .find(|name| params.get(name).is_some() && params.known(name).is_none());
        Some(KernelSpec::new(move |context| {
            let (params, flush_interval) = aggregate
                .clone()
                .ok_or_else(|| unresolved(unresolved_param.unwrap_or("window")))?;
            // A host that runs state owners gives each one its budgets; a kernel built outside any host, such as the
            // interpreter's, owns its contexts alone, as native aggregation does.
            let budget = context.state_budget().cloned().unwrap_or_else(ContextBudget::exclusive);
            let telemetry = AggregateTelemetry::new(&MetricsBuilder::from_component_context(context.component()));
            Ok(Box::new(AggregateKernel::new(
                params,
                flush_interval,
                budget,
                telemetry,
            )))
        }))
    }

    fn lineage_params(&self, params: &DecodedParams) -> Params {
        // The window is the state contract's time, the only parameter the state depends on: a lineage is a window.
        let mut lineage = Params::new();
        if let Some(window) = params.known("window").or_else(|| params.get("window")) {
            let name = ParamName::new("window").expect("`window` is a parameter name");
            lineage.insert(name, window.encode());
        }
        lineage
    }

    fn step_params(
        &self, params: &DecodedParams, filter: Option<Arc<dyn EmissionFilter>>,
    ) -> Option<Arc<dyn Any + Send + Sync>> {
        let (_, admit, mut flush) = aggregate_params(params)?.split();
        flush.filter = filter;
        Some(Arc::new(AggregateStepParams {
            admit,
            flush,
            flush_interval: params.duration("flush_interval")?,
        }))
    }

    fn flush_interval(&self, params: &DecodedParams) -> Option<Duration> {
        params.duration("flush_interval")
    }
}
