//! Operators for host enrichment: `host_enrichment` and `host_tags`.

use saluki_core::{
    data_model::tags::{SharedTagSet, Tag},
    execution::AdmissionTime,
};
use saluki_metric_kernels::host_enrichment::{HostEnrichment as HostEnrichmentKernel, HostTags as HostTagsKernel};

use super::{
    contract::{EffectContract, Identity, PerKind, TimeContract},
    names,
    ports::{PortMapping, Ports},
    support::unresolved,
    KernelSpec, OperatorDef,
};
use crate::{
    ir::{DecodedParams, ParamSpec, ParamType, ParamValue},
    model::{Field, FieldSet, PortType},
};

/// The `host_enrichment` operator: the metric part of native host enrichment, which leaves metrics unchanged.
///
/// It takes no parameters. Native host enrichment only sets the host of events and service checks, which are outside
/// the plan. Its kernel is [`HostEnrichmentKernel`], which leaves each batch as it is.
#[derive(Clone, Copy, Debug, Default)]
pub struct HostEnrichment;

impl OperatorDef for HostEnrichment {
    fn name(&self) -> &'static str {
        names::HOST_ENRICHMENT
    }

    fn summary(&self) -> &'static str {
        "Leaves metrics unchanged: native host enrichment only sets the host of events and service checks."
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
        Some(KernelSpec::new(|_context| Ok(Box::new(HostEnrichmentKernel))))
    }
}

const HOST_TAGS_PARAMS: &[ParamSpec] = &[
    ParamSpec::required("tags", ParamType::List(&ParamType::String)),
    ParamSpec::required("deadline", ParamType::Duration),
];

/// The `host_tags` operator: adds the host's tags to every metric admitted before a deadline.
///
/// Parameters: `tags` (the host tags) and `deadline` (an instant, written as the time since the Unix epoch, such as
/// `"1700000300s"`). Both are host facts fixed for the life of the process: the deadline is the process start time
/// plus the configured duration for which host tags are expected. A batch whose admission time is before the deadline
/// gains the tags; later batches pass unchanged. The tags are appended to the instrumented tags as they are, without
/// removing values the metric already has.
///
/// Its kernel is [`HostTagsKernel`], which decides per batch by the batch's admission time.
#[derive(Clone, Copy, Debug, Default)]
pub struct HostTags;

impl OperatorDef for HostTags {
    fn name(&self) -> &'static str {
        names::HOST_TAGS
    }

    fn summary(&self) -> &'static str {
        "Adds the host's tags to every metric admitted before a deadline."
    }

    fn params(&self) -> &'static [ParamSpec] {
        HOST_TAGS_PARAMS
    }

    fn ports(&self, _params: &DecodedParams) -> Ports {
        Ports::single(PortType::ANY, PortMapping::IDENTITY)
    }

    fn contract(&self, _params: &DecodedParams) -> EffectContract {
        EffectContract {
            writes: PerKind::uniform(FieldSet::of(&[Field::Tags])),
            identity: PerKind::uniform(Identity::Quotient),
            time: TimeContract {
                reads_admission_time: true,
            },
            ..EffectContract::passthrough()
        }
    }

    fn kernel(&self, params: &DecodedParams) -> Option<KernelSpec> {
        let tags = params.list("tags").and_then(|tags| {
            tags.iter()
                .map(|tag| match tag {
                    ParamValue::String(tag) => Some(Tag::from(tag.as_str())),
                    _ => None,
                })
                .collect::<Option<SharedTagSet>>()
        });
        // A deadline beyond what admission times can represent is never reached.
        let deadline = params
            .duration("deadline")
            .map(|deadline| AdmissionTime::from_unix_nanos(u64::try_from(deadline.as_nanos()).unwrap_or(u64::MAX)));
        Some(KernelSpec::new(move |_context| {
            let tags = tags.clone().ok_or_else(|| unresolved("tags"))?;
            let deadline = deadline.ok_or_else(|| unresolved("deadline"))?;
            Ok(Box::new(HostTagsKernel::new(tags, deadline)))
        }))
    }
}
