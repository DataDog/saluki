//! Operators that exist only in tests, so that the interpreter's kernel path can be exercised before native kernels
//! are extracted.

use std::sync::OnceLock;

use saluki_core::{
    data_model::event::{metric::Metric, Event},
    execution::{DrainPolicy, Kernel, LossClass, PortIndex, Progress, Turn},
    topology::EventsBuffer,
};

use crate::{
    extension::{AdmissionPolicy, SignalDecl, SignalType},
    ir::{DecodedParams, ParamSpec, ParamType, PortName},
    model::{Field, FieldSet, Kind, PortType},
    operators::{
        Cardinality, EffectContract, Identity, InputPort, KernelBuildError, KernelSpec, KeyedState, Loss, LossDecl,
        OperatorDef, OperatorRegistry, OutputPort, PerKind, PortMapping, PortRelation, Ports, Reducer, ReducerKey,
        StateContract, StateTime, IN,
    },
};

/// Returns the native operators plus the test operators.
pub(crate) fn test_registry() -> OperatorRegistry {
    static REGISTRY: OnceLock<OperatorRegistry> = OnceLock::new();
    *REGISTRY.get_or_init(|| {
        let tests: [&'static dyn OperatorDef; 8] = [
            &TestPrefix,
            &TestRoute,
            &TestHold,
            &TestStall,
            &TestSignalReader,
            &TestNeedsRuntime,
            &TestFinishMarker,
            &TestKernelless,
        ];
        let operators = OperatorRegistry::native().iter().chain(tests).collect::<Vec<_>>();
        OperatorRegistry::with_operators(Box::leak(operators.into_boxed_slice()))
    })
}

const PREFIX_PARAMS: &[ParamSpec] = &[ParamSpec::required("prefix", ParamType::String)];

/// `test_prefix`: puts `prefix` in front of every metric's name, transforming its input in place. An empty prefix
/// fails to build.
pub(super) struct TestPrefix;

impl OperatorDef for TestPrefix {
    fn name(&self) -> &'static str {
        "test_prefix"
    }

    fn summary(&self) -> &'static str {
        "Adds a prefix to every metric's name."
    }

    fn params(&self) -> &'static [ParamSpec] {
        PREFIX_PARAMS
    }

    fn ports(&self, _params: &DecodedParams) -> Ports {
        Ports::single(PortType::ANY, PortMapping::IDENTITY)
    }

    fn contract(&self, _params: &DecodedParams) -> EffectContract {
        let name = FieldSet::of(&[Field::Name]);
        EffectContract {
            reads: PerKind::uniform(name),
            writes: PerKind::uniform(name),
            identity: PerKind::uniform(Identity::Injective { image: None }),
            ..EffectContract::passthrough()
        }
    }

    fn kernel(&self, params: &DecodedParams) -> Option<KernelSpec> {
        let prefix = params.str("prefix")?.to_string();
        Some(KernelSpec::new(move |_| {
            if prefix.is_empty() {
                return Err(KernelBuildError {
                    message: "the prefix must not be empty".to_string(),
                });
            }
            Ok(Box::new(PrefixKernel { prefix: prefix.clone() }))
        }))
    }
}

struct PrefixKernel {
    prefix: String,
}

impl Kernel for PrefixKernel {
    fn consume(&mut self, _turn: &mut Turn<'_>, input: &mut EventsBuffer) -> Progress {
        for event in input {
            if let Event::Metric(metric) = event {
                let renamed = metric
                    .context()
                    .with_name(format!("{}{}", self.prefix, metric.context().name()));
                *metric.context_mut() = renamed;
            }
        }
        Progress::Done
    }
}

const ROUTE_PARAMS: &[ParamSpec] = &[ParamSpec::required("route_all", ParamType::Bool)];

/// The port `test_route` keeps its input on.
pub(super) const KEPT_PORT: &str = "kept";

/// The port `test_route` sends gauges to.
pub(super) const GAUGES_PORT: &str = "gauges";

/// `test_route`: a two-port kernel. It pushes gauges one by one to `gauges` and leaves everything else in its input,
/// which goes to its primary port `kept`; with `route_all`, it routes its whole input to `gauges` untouched.
pub(super) struct TestRoute;

impl OperatorDef for TestRoute {
    fn name(&self) -> &'static str {
        "test_route"
    }

    fn summary(&self) -> &'static str {
        "Routes gauges to a second port."
    }

    fn params(&self) -> &'static [ParamSpec] {
        ROUTE_PARAMS
    }

    fn ports(&self, _params: &DecodedParams) -> Ports {
        Ports {
            inputs: vec![InputPort {
                name: PortName::from_static(IN),
                accepts: PortType::ANY,
            }],
            outputs: [KEPT_PORT, GAUGES_PORT]
                .into_iter()
                .map(|name| OutputPort {
                    name: PortName::from_static(name),
                    mapping: PortMapping::IDENTITY,
                })
                .collect(),
        }
    }

    fn contract(&self, _params: &DecodedParams) -> EffectContract {
        EffectContract {
            reads: PerKind::uniform(FieldSet::of(&[Field::Kind])),
            ports: PortRelation::PointDisjoint,
            ..EffectContract::passthrough()
        }
    }

    fn kernel(&self, params: &DecodedParams) -> Option<KernelSpec> {
        let route_all = params.bool("route_all")?;
        Some(KernelSpec::new(move |_| Ok(Box::new(RouteKernel { route_all }))))
    }
}

struct RouteKernel {
    route_all: bool,
}

impl Kernel for RouteKernel {
    fn consume(&mut self, turn: &mut Turn<'_>, input: &mut EventsBuffer) -> Progress {
        const GAUGES: PortIndex = PortIndex::new(1);
        if self.route_all {
            turn.route_all(GAUGES);
            return Progress::Done;
        }
        let events = std::mem::replace(input, EventsBuffer::with_capacity(input.len()));
        for event in events {
            let is_gauge = matches!(&event, Event::Metric(metric) if Kind::of(metric.values()) == Kind::Gauge);
            if is_gauge {
                turn.push(GAUGES, event);
            } else {
                let overflow = input.try_push(event);
                debug_assert!(overflow.is_none(), "the input holds what it held before");
            }
        }
        Progress::Done
    }
}

/// `test_hold`: a stateful kernel that holds everything it consumes. At each tick, and when it retires, it emits what
/// it holds, one event per call, yielding until it holds nothing; at shutdown, it discards what it holds.
pub(super) struct TestHold;

const HOLD_PARAMS: &[ParamSpec] = &[ParamSpec::optional("label", ParamType::String)];

impl OperatorDef for TestHold {
    fn name(&self) -> &'static str {
        "test_hold"
    }

    fn summary(&self) -> &'static str {
        "Holds metrics until the next tick."
    }

    fn params(&self) -> &'static [ParamSpec] {
        HOLD_PARAMS
    }

    fn ports(&self, _params: &DecodedParams) -> Ports {
        Ports::single(PortType::ANY, PortMapping::IDENTITY)
    }

    fn contract(&self, _params: &DecodedParams) -> EffectContract {
        EffectContract {
            reads: PerKind::uniform(FieldSet::ALL),
            state: StateContract::Keyed(KeyedState {
                key: ReducerKey::Context,
                reducer: Reducer::NativeAggregation,
                time: StateTime::Buckets {
                    width: Some(std::time::Duration::from_secs(10)),
                },
                admission: AdmissionPolicy::FirstCome,
            }),
            loss: Loss::Lossy(vec![LossDecl {
                class: LossClass::Shutdown,
                description: "Discards what it holds at shutdown.",
            }]),
            ..EffectContract::passthrough()
        }
    }

    fn kernel(&self, _params: &DecodedParams) -> Option<KernelSpec> {
        Some(KernelSpec::new(|_| Ok(Box::new(HoldKernel { held: Vec::new() }))))
    }
}

struct HoldKernel {
    held: Vec<Event>,
}

impl HoldKernel {
    /// Emits the oldest held event, yielding while more remain.
    fn emit_one(&mut self, turn: &mut Turn<'_>) -> Progress {
        if self.held.is_empty() {
            return Progress::Done;
        }
        let event = self.held.remove(0);
        turn.push(PortIndex::PRIMARY, event);
        if self.held.is_empty() {
            Progress::Done
        } else {
            Progress::Yielded
        }
    }
}

impl Kernel for HoldKernel {
    fn consume(&mut self, _turn: &mut Turn<'_>, input: &mut EventsBuffer) -> Progress {
        let events = std::mem::replace(input, EventsBuffer::with_capacity(0));
        self.held.extend(events);
        Progress::Done
    }

    fn advance_time(&mut self, turn: &mut Turn<'_>) -> Progress {
        self.emit_one(turn)
    }

    fn finish(&mut self, turn: &mut Turn<'_>, policy: DrainPolicy) -> Progress {
        match policy {
            DrainPolicy::Retire => self.emit_one(turn),
            DrainPolicy::Shutdown => {
                let points = self
                    .held
                    .iter()
                    .map(|event| match event {
                        Event::Metric(metric) => metric.values().len() as u64,
                        _ => 0,
                    })
                    .sum();
                turn.counters()
                    .record_loss(LossClass::Shutdown, self.held.len() as u64, points);
                self.held.clear();
                Progress::Done
            }
        }
    }
}

/// `test_stall`: a kernel that yields forever.
pub(super) struct TestStall;

impl OperatorDef for TestStall {
    fn name(&self) -> &'static str {
        "test_stall"
    }

    fn summary(&self) -> &'static str {
        "Never finishes a batch."
    }

    fn params(&self) -> &'static [ParamSpec] {
        &[]
    }

    fn ports(&self, _params: &DecodedParams) -> Ports {
        Ports::single(PortType::ANY, PortMapping::IDENTITY)
    }

    fn contract(&self, _params: &DecodedParams) -> EffectContract {
        EffectContract::passthrough()
    }

    fn kernel(&self, _params: &DecodedParams) -> Option<KernelSpec> {
        Some(KernelSpec::new(|_| Ok(Box::new(StallKernel))))
    }
}

struct StallKernel;

impl Kernel for StallKernel {
    fn consume(&mut self, _turn: &mut Turn<'_>, _input: &mut EventsBuffer) -> Progress {
        Progress::Yielded
    }
}

/// `test_signal_reader`: an operator that declares that it reads the signal `queue_full`.
pub(super) struct TestSignalReader;

impl OperatorDef for TestSignalReader {
    fn name(&self) -> &'static str {
        "test_signal_reader"
    }

    fn summary(&self) -> &'static str {
        "Reads a host signal."
    }

    fn params(&self) -> &'static [ParamSpec] {
        &[]
    }

    fn ports(&self, _params: &DecodedParams) -> Ports {
        Ports::single(PortType::ANY, PortMapping::IDENTITY)
    }

    fn contract(&self, _params: &DecodedParams) -> EffectContract {
        EffectContract {
            signals: vec![SignalDecl {
                name: "queue_full".parse().expect("valid signal name"),
                ty: SignalType::Bool,
            }],
            cardinality: Cardinality::Map,
            ..EffectContract::passthrough()
        }
    }
}

/// `test_kernelless`: a synchronous passthrough without a kernel, which no native operator is.
pub(super) struct TestKernelless;

impl OperatorDef for TestKernelless {
    fn name(&self) -> &'static str {
        "test_kernelless"
    }

    fn summary(&self) -> &'static str {
        "Declares no kernel."
    }

    fn params(&self) -> &'static [ParamSpec] {
        &[]
    }

    fn ports(&self, _params: &DecodedParams) -> Ports {
        Ports::single(PortType::ANY, PortMapping::IDENTITY)
    }

    fn contract(&self, _params: &DecodedParams) -> EffectContract {
        EffectContract::passthrough()
    }
}

/// `test_needs_runtime`: a passthrough whose kernel spawns a task when it is built, so it builds only inside a Tokio
/// context.
pub(super) struct TestNeedsRuntime;

impl OperatorDef for TestNeedsRuntime {
    fn name(&self) -> &'static str {
        "test_needs_runtime"
    }

    fn summary(&self) -> &'static str {
        "Spawns a task when its kernel is built."
    }

    fn params(&self) -> &'static [ParamSpec] {
        &[]
    }

    fn ports(&self, _params: &DecodedParams) -> Ports {
        Ports::single(PortType::ANY, PortMapping::IDENTITY)
    }

    fn contract(&self, _params: &DecodedParams) -> EffectContract {
        EffectContract::passthrough()
    }

    fn kernel(&self, _params: &DecodedParams) -> Option<KernelSpec> {
        Some(KernelSpec::new(|_| {
            let handle = tokio::runtime::Handle::try_current().map_err(|error| KernelBuildError {
                message: error.to_string(),
            })?;
            // A task that is never driven, as a native cache's expiration task is not in the interpreter.
            drop(handle.spawn(async {}));
            Ok(Box::new(PassthroughKernel))
        }))
    }
}

struct PassthroughKernel;

impl Kernel for PassthroughKernel {
    fn consume(&mut self, _turn: &mut Turn<'_>, _input: &mut EventsBuffer) -> Progress {
        Progress::Done
    }
}

/// `test_finish_marker`: a stateless passthrough that emits a gauge named `finished.retire` or `finished.shutdown` when
/// it finishes, so that tests can see when and where kernels finish.
pub(super) struct TestFinishMarker;

impl OperatorDef for TestFinishMarker {
    fn name(&self) -> &'static str {
        "test_finish_marker"
    }

    fn summary(&self) -> &'static str {
        "Emits a marker when it finishes."
    }

    fn params(&self) -> &'static [ParamSpec] {
        &[]
    }

    fn ports(&self, _params: &DecodedParams) -> Ports {
        Ports::single(PortType::ANY, PortMapping::IDENTITY)
    }

    fn contract(&self, _params: &DecodedParams) -> EffectContract {
        EffectContract::passthrough()
    }

    fn kernel(&self, _params: &DecodedParams) -> Option<KernelSpec> {
        Some(KernelSpec::new(|_| Ok(Box::new(FinishMarkerKernel))))
    }
}

struct FinishMarkerKernel;

impl Kernel for FinishMarkerKernel {
    fn consume(&mut self, _turn: &mut Turn<'_>, _input: &mut EventsBuffer) -> Progress {
        Progress::Done
    }

    fn finish(&mut self, turn: &mut Turn<'_>, policy: DrainPolicy) -> Progress {
        let name = match policy {
            DrainPolicy::Retire => "finished.retire",
            DrainPolicy::Shutdown => "finished.shutdown",
        };
        turn.push(PortIndex::PRIMARY, Event::Metric(Metric::gauge(name, 1.0)));
        Progress::Done
    }
}
