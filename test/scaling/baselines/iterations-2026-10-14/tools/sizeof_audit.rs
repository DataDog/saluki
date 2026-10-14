use std::mem::{align_of, size_of};

use saluki_core::data_model::event::{
    eventd::EventD,
    log::Log,
    metric::{context::Context, HistogramPoints, Metric, MetricMetadata, MetricValues, ScalarPoints, SetPoints, SketchPoints},
    service_check::ServiceCheck,
    trace::Trace,
    trace_stats::TraceStats,
    Event,
};
use saluki_core::data_model::tags::{SharedTagSet, Tag, TagSet};
use saluki_core::topology::EventsBuffer;
use stringtheory::MetaString;

macro_rules! report {
    ($($t:ty),* $(,)?) => {
        $( println!("{:<16} size {:>4} align {:>2}", stringify!($t), size_of::<$t>(), align_of::<$t>()); )*
    };
}

#[test]
fn sizeof_audit() {
    report!(
        Event, Metric, MetricValues, ScalarPoints, SetPoints, HistogramPoints, SketchPoints, MetricMetadata, Context,
        TagSet, SharedTagSet, Tag, MetaString, EventD, ServiceCheck, Log, Trace, TraceStats, EventsBuffer,
    );
    println!("metrics per 64-byte line in an Event slot: {:.2}", 64.0 / size_of::<Event>() as f64);
    println!("Event slots in a 1,024-event buffer: {} KiB", 1024 * size_of::<Event>() / 1024);
}
