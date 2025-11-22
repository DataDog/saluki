/// Sampling priority
///
/// Reference code: https://github.com/DataDog/datadog-agent/blob/main/pkg/trace/sampler/sampler.go#L47
#[repr(i8)]
#[allow(unused)]
pub enum SamplingPriority {
    // PriorityNone is the value for SamplingPriority when no priority sampling decision could be found.
    PriorityNone = i8::MIN,
    // PriorityUserDrop is the value set by a user to explicitly drop a trace.
    PriorityUserDrop = -1,

    // PriorityAutoDrop is the value set by a tracer to suggest dropping a trace.
    PriorityAutoDrop = 0,

    // PriorityAutoKeep is the value set by a tracer to suggest keeping a trace.
    PriorityAutoKeep = 1,

    // PriorityUserKeep is the value set by a user to explicitly keep a trace.
    PriorityUserKeep = 2,
}
