//! The adapter that runs native aggregation as an opaque host.

use saluki_core::execution::{LossClass, PortIndex};
use saluki_metric_kernels::aggregate::{AggregateCore, AggregateParams, AggregateTelemetry};

use crate::{
    interpret::{AdapterError, HostBatch, HostFinish, HostOutput, HostTick, OpaqueHostAdapter},
    operators::{aggregate_params, names, ConfiguredOperator},
};

/// Native aggregation, run as an opaque host by the reference interpreter.
///
/// The adapter drives the same [`AggregateCore`] the native aggregate transform runs, as that transform does:
///
/// - A batch is admitted at its **arrival** time, floored to whole seconds, because native aggregation assigns buckets
///   by the clock when a batch reaches it, not by admission time. Metrics dropped at the context limit are reported as
///   capacity loss.
/// - A tick flushes at its time, floored to whole seconds, and does nothing while the core holds no contexts. The
///   watermark is ignored: native aggregation closes buckets by its clock.
/// - Finishing runs the final flush at the host time of the last tick, then discards the open buckets and reports
///   their points as shutdown loss, one event per context that held points. An aggregation host never retires before
///   host components have generations, so retiring, under [`DrainPolicy::Retire`], drains it the same way.
///
/// Telemetry goes nowhere: the core reports through [`AggregateTelemetry::noop`].
///
/// [`DrainPolicy::Retire`]: saluki_core::execution::DrainPolicy::Retire
pub struct AggregateAdapter {
    core: AggregateCore,
}

impl AggregateAdapter {
    /// Creates an adapter running native aggregation with the given parameters.
    pub fn new(params: AggregateParams) -> Self {
        Self {
            core: AggregateCore::new(params, AggregateTelemetry::noop()),
        }
    }

    /// Creates an adapter for an `aggregate` node.
    ///
    /// # Errors
    ///
    /// Returns an error if the node is not an `aggregate` node, or if one of its parameters is bound to configuration
    /// and has no resolved value.
    pub fn from_operator(operator: &ConfiguredOperator) -> Result<Self, AdapterError> {
        if operator.name() != names::AGGREGATE {
            return Err(AdapterError {
                message: format!(
                    "the aggregation adapter runs `{}` nodes, not `{}` nodes",
                    names::AGGREGATE,
                    operator.name()
                ),
            });
        }
        let params = aggregate_params(operator.params()).ok_or_else(|| AdapterError {
            message: "a parameter of the `aggregate` node is bound to configuration and has no resolved value; \
                      resolve the plan's bindings before running it"
                .to_string(),
        })?;
        Ok(Self::new(params))
    }

    fn flush(&mut self, now: u64, last: bool, output: &mut HostOutput) {
        self.core
            .flush_into(now, last, |metric| output.push(PortIndex::PRIMARY, metric));
    }
}

impl OpaqueHostAdapter for AggregateAdapter {
    fn admit(&mut self, batch: HostBatch, output: &mut HostOutput) {
        let outcome = self.core.admit(batch.arrived_at.as_unix_secs(), batch.events);
        if outcome.dropped > 0 {
            output.record_loss(
                LossClass::Capacity,
                outcome.dropped as u64,
                outcome.dropped_points as u64,
            );
        }
    }

    fn tick(&mut self, tick: HostTick, output: &mut HostOutput) {
        self.flush(tick.now.as_unix_secs(), false, output);
    }

    fn finish(&mut self, finish: HostFinish, output: &mut HostOutput) {
        // Natively, the final flush is the only drain aggregation has, whatever the reason it stops.
        self.flush(finish.now.as_unix_secs(), true, output);
        let (contexts, points) = self.core.held_points();
        if points > 0 {
            output.record_loss(LossClass::Shutdown, contexts as u64, points as u64);
        }
    }
}
