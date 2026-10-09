//! Operators for the DogStatsD pre-aggregation path: `dogstatsd_mapper`, `namespace_prefix`, `timestamp_split`, and
//! `tag_projection`.
//!
//! Their parameters use the plan-owned types in [`saluki_metric_kernels::params`], which the kernels consume. The
//! `encode_*` functions turn those types into parameter values, as lowering writes them, and the `decode_*` functions
//! turn decoded parameter values back into them.

use std::num::{NonZeroU64, NonZeroUsize};

use saluki_core::accounting::MemoryBoundsBuilder;
use saluki_metric_kernels::{
    mapper::{result_cache_usage, validate_profiles, MetricMapper},
    params::{
        validate_tag_value_rules, MapperProfile, MatchType, MetricMapping, NamespacePrefixParams, TagFilterAction,
        TagFilterRule, TagValueMismatch, TagValueRule,
    },
    prefix::NamespacePrefix as NamespacePrefixKernel,
    tag_projection::{
        ContextMemo, TagProjection as TagProjectionKernel, TagProjectionRules, TagProjectionTelemetry, ValueRules,
        CONTEXT_CACHE_ENTRY_BYTES, NATIVE_CONTEXT_CACHE_ID,
    },
    timestamp_split::{TimestampSplit as TimestampSplitKernel, TimestampSplitTelemetry},
};
use serde_json::{json, Map, Value};

use super::{
    contract::{Cardinality, EffectContract, Identity, Loss, LossClass, LossDecl, PerKind},
    names,
    ports::{InputPort, KindMap, OutputPort, PortMapping, Ports, TimestampMap, IN},
    support::{check_whole_seconds, error, error_chain, unresolved},
    KernelBuildContext, KernelBuildError, KernelSpec, OperatorDef,
};
use crate::{
    canonical::MAX_SAFE_INTEGER,
    ir::{DecodedParams, ParamError, ParamSpec, ParamType, ParamValue, PortName},
    model::{Field, FieldSet, Kind, KindSet, PortType, TimestampForm},
};

const STRINGS: ParamType = ParamType::List(&ParamType::String);
const COUNT: ParamType = ParamType::Integer {
    min: 0,
    max: MAX_SAFE_INTEGER,
};

const MAPPING_TAG: &[ParamSpec] = &[
    ParamSpec::required("name", ParamType::String),
    ParamSpec::required("value", ParamType::String),
];

const MAPPING: &[ParamSpec] = &[
    ParamSpec::required("match", ParamType::String),
    ParamSpec::required("match_type", ParamType::Enum(&["wildcard", "regex"])),
    ParamSpec::required("name", ParamType::String),
    ParamSpec::optional("tags", ParamType::Set(&ParamType::Record(MAPPING_TAG))),
];

const PROFILE: &[ParamSpec] = &[
    ParamSpec::required("name", ParamType::String),
    ParamSpec::required("prefix", ParamType::String),
    ParamSpec::required("mappings", ParamType::List(&ParamType::Record(MAPPING))),
];

/// The type of the mapper's `profiles` parameter: a list of profiles, each
/// `{"name": .., "prefix": .., "mappings": [{"match": .., "match_type": "wildcard" | "regex", "name": .., "tags":
/// [{"name": .., "value": ..}]}]}`.
pub const MAPPER_PROFILES: ParamType = ParamType::List(&ParamType::Record(PROFILE));

const MAPPER_PARAMS: &[ParamSpec] = &[
    ParamSpec::required("profiles", ParamType::Bindable(&MAPPER_PROFILES)),
    ParamSpec::required("cache_size", ParamType::Bindable(&COUNT)),
    ParamSpec::required(
        "string_interner_bytes",
        ParamType::Bindable(&ParamType::Integer {
            min: 1,
            max: MAX_SAFE_INTEGER,
        }),
    ),
];

fn record(value: &ParamValue) -> Option<&DecodedParams> {
    match value {
        ParamValue::Record(members) => Some(members),
        _ => None,
    }
}

fn strings(values: &[ParamValue]) -> Option<Vec<String>> {
    values
        .iter()
        .map(|value| match value {
            ParamValue::String(value) => Some(value.clone()),
            _ => None,
        })
        .collect()
}

/// Encodes mapper profiles as the value of the mapper's `profiles` parameter.
pub fn encode_mapper_profiles(profiles: &[MapperProfile]) -> Value {
    Value::Array(
        profiles
            .iter()
            .map(|profile| {
                let mappings = profile
                    .mappings
                    .iter()
                    .map(|mapping| {
                        let mut members = Map::new();
                        members.insert("match".into(), json!(mapping.metric_match));
                        members.insert("match_type".into(), json!(mapping.match_type.as_str()));
                        members.insert("name".into(), json!(mapping.name));
                        if !mapping.tags.is_empty() {
                            let tags = mapping
                                .tags
                                .iter()
                                .map(|(name, value)| json!({"name": name, "value": value}))
                                .collect();
                            members.insert("tags".into(), Value::Array(tags));
                        }
                        Value::Object(members)
                    })
                    .collect::<Vec<_>>();
                json!({"name": profile.name, "prefix": profile.prefix, "mappings": mappings})
            })
            .collect(),
    )
}

/// Decodes the known value of the mapper's `profiles` parameter.
///
/// Returns `None` if the value is not a decoded list of profiles, such as an unresolved binding.
pub fn decode_mapper_profiles(value: &ParamValue) -> Option<Vec<MapperProfile>> {
    let ParamValue::List(profiles) = value.known()? else {
        return None;
    };
    profiles
        .iter()
        .map(|profile| {
            let profile = record(profile)?;
            let mappings = profile
                .list("mappings")?
                .iter()
                .map(|mapping| {
                    let mapping = record(mapping)?;
                    let match_type = match mapping.str("match_type")? {
                        "regex" => MatchType::Regex,
                        _ => MatchType::Wildcard,
                    };
                    let tags = mapping
                        .list("tags")
                        .unwrap_or_default()
                        .iter()
                        .map(|tag| {
                            let tag = record(tag)?;
                            Some((tag.str("name")?.to_string(), tag.str("value")?.to_string()))
                        })
                        .collect::<Option<Vec<_>>>()?;
                    Some(MetricMapping {
                        metric_match: mapping.str("match")?.to_string(),
                        match_type,
                        name: mapping.str("name")?.to_string(),
                        tags,
                    })
                })
                .collect::<Option<Vec<_>>>()?;
            Some(MapperProfile {
                name: profile.str("name")?.to_string(),
                prefix: profile.str("prefix")?.to_string(),
                mappings,
            })
        })
        .collect()
}

/// The `dogstatsd_mapper` operator: renames metrics and adds tags by mapping profile.
///
/// Parameters: `profiles` (bindable; see [`MAPPER_PROFILES`]), `cache_size` (the number of mapping results cached by
/// name; `0` disables the cache), and `string_interner_bytes` (the capacity of the interner for mapped names and tags).
/// The caches are unobservable. A metric that no mapping matches passes unchanged. Known profiles are validated as the
/// kernel compiles them, patterns included (see [`saluki_metric_kernels::mapper`]).
///
/// Its kernel is [`MetricMapper`], which maps each batch in place and names its caches after the component it is
/// built for.
#[derive(Clone, Copy, Debug, Default)]
pub struct DogstatsdMapper;

impl OperatorDef for DogstatsdMapper {
    fn name(&self) -> &'static str {
        names::DOGSTATSD_MAPPER
    }

    fn summary(&self) -> &'static str {
        "Renames metrics and adds tags according to mapping profiles matched against the raw name."
    }

    fn params(&self) -> &'static [ParamSpec] {
        MAPPER_PARAMS
    }

    fn check_params(&self, params: &DecodedParams) -> Vec<ParamError> {
        let Some(profiles) = params.get("profiles").and_then(decode_mapper_profiles) else {
            return Vec::new();
        };
        match validate_profiles(&profiles) {
            Ok(()) => Vec::new(),
            Err(problem) => vec![error("profiles", error_chain(&problem))],
        }
    }

    fn ports(&self, _params: &DecodedParams) -> Ports {
        Ports::single(PortType::ANY, PortMapping::IDENTITY)
    }

    fn contract(&self, _params: &DecodedParams) -> EffectContract {
        EffectContract {
            reads: PerKind::uniform(FieldSet::of(&[Field::Name])),
            writes: PerKind::uniform(FieldSet::of(&[Field::Name, Field::Tags])),
            identity: PerKind::uniform(Identity::Quotient),
            ..EffectContract::passthrough()
        }
    }

    fn specify_bounds(&self, params: &DecodedParams, builder: &mut MemoryBoundsBuilder) {
        // The interner's storage is allocated up front, as is the result cache's when it is enabled.
        let mut minimum = builder.minimum();
        if let Some(bytes) = params.integer("string_interner_bytes") {
            minimum.with_fixed_amount("string interner", usize::try_from(bytes).unwrap_or(usize::MAX));
        }
        if let Some(cache_size) = params.integer("cache_size").filter(|size| *size > 0) {
            minimum.with_expr(result_cache_usage(usize::try_from(cache_size).unwrap_or(usize::MAX)));
        }
    }

    fn cost(&self, params: &DecodedParams) -> u32 {
        // Every name is looked up in the mapper's cache; names that match a profile are rebuilt.
        match params.get("profiles").and_then(decode_mapper_profiles) {
            Some(profiles) if profiles.is_empty() => 10,
            _ => 65,
        }
    }

    fn kernel(&self, params: &DecodedParams) -> Option<KernelSpec> {
        let profiles = params.get("profiles").and_then(decode_mapper_profiles);
        let cache_size = params.integer("cache_size").and_then(|size| usize::try_from(size).ok());
        let interner_bytes = params
            .integer("string_interner_bytes")
            .and_then(|bytes| usize::try_from(bytes).ok())
            .and_then(NonZeroUsize::new);
        Some(KernelSpec::new(move |context| {
            let profiles = profiles.as_deref().ok_or_else(|| unresolved("profiles"))?;
            let cache_size = cache_size.ok_or_else(|| unresolved("cache_size"))?;
            let interner_bytes = interner_bytes.ok_or_else(|| unresolved("string_interner_bytes"))?;
            let mapper = MetricMapper::new(context.component().component_id(), profiles, cache_size, interner_bytes)
                .map_err(|problem| KernelBuildError {
                    message: error_chain(&problem),
                })?;
            Ok(Box::new(mapper))
        }))
    }
}

const NAMESPACE_PREFIX_PARAMS: &[ParamSpec] = &[
    ParamSpec::required("namespace", ParamType::Bindable(&ParamType::String)),
    ParamSpec::required("exclusions", ParamType::Bindable(&STRINGS)),
];

/// Decodes the known parameters of a `namespace_prefix` node.
///
/// Returns `None` if either parameter is an unresolved binding.
pub fn namespace_prefix_params(params: &DecodedParams) -> Option<NamespacePrefixParams> {
    Some(NamespacePrefixParams {
        namespace: params.str("namespace")?.to_string(),
        exclusions: strings(params.list("exclusions")?)?,
    })
}

/// The `namespace_prefix` operator: adds a namespace prefix to metric names.
///
/// Parameters: `namespace` (bindable; empty disables prefixing, and a `.` is added if it does not end with one) and
/// `exclusions` (bindable; raw name prefixes, compared before prefixing, that are left as they are). Two names can map
/// to one: an excluded name can equal another name's prefixed form.
///
/// Its kernel is [`NamespacePrefixKernel`], which prefixes each batch in place.
#[derive(Clone, Copy, Debug, Default)]
pub struct NamespacePrefix;

impl OperatorDef for NamespacePrefix {
    fn name(&self) -> &'static str {
        names::NAMESPACE_PREFIX
    }

    fn summary(&self) -> &'static str {
        "Adds a namespace prefix to metric names, except names starting with an excluded prefix."
    }

    fn params(&self) -> &'static [ParamSpec] {
        NAMESPACE_PREFIX_PARAMS
    }

    fn ports(&self, _params: &DecodedParams) -> Ports {
        Ports::single(PortType::ANY, PortMapping::IDENTITY)
    }

    fn contract(&self, _params: &DecodedParams) -> EffectContract {
        EffectContract {
            reads: PerKind::uniform(FieldSet::of(&[Field::Name])),
            writes: PerKind::uniform(FieldSet::of(&[Field::Name])),
            identity: PerKind::uniform(Identity::Quotient),
            ..EffectContract::passthrough()
        }
    }

    fn cost(&self, params: &DecodedParams) -> u32 {
        // Prefixing builds a new name and context for every metric; an empty namespace disables it.
        match params.str("namespace") {
            Some("") => 0,
            _ => 90,
        }
    }

    fn kernel(&self, params: &DecodedParams) -> Option<KernelSpec> {
        let namespace = params.str("namespace").map(str::to_string);
        let exclusions = params.list("exclusions").and_then(strings);
        Some(KernelSpec::new(move |_context| {
            let params = NamespacePrefixParams {
                namespace: namespace.clone().ok_or_else(|| unresolved("namespace"))?,
                exclusions: exclusions.clone().ok_or_else(|| unresolved("exclusions"))?,
            };
            Ok(Box::new(NamespacePrefixKernel::new(&params)))
        }))
    }
}

/// The output port of a `timestamp_split` for untimestamped points.
pub const DEFAULT_PORT: &str = "default";

/// The output port of a `timestamp_split` for timestamped points.
pub const PASSTHROUGH_PORT: &str = "passthrough";

const TIMESTAMP_SPLIT_PARAMS: &[ParamSpec] =
    &[ParamSpec::required("window", ParamType::Bindable(&ParamType::Duration))];

const RATE: KindSet = KindSet::of(&[Kind::Rate]);

/// The `timestamp_split` operator: separates the timestamped points of each metric from its untimestamped points.
///
/// Parameters: `window` (bindable; a whole number of seconds, at least one), the rate interval timestamped counters
/// are given, which must equal the aggregation width.
///
/// Untimestamped points leave on `default` and timestamped points on `passthrough`, so the ports are point-disjoint,
/// and a metric with both kinds of point becomes two records. On `passthrough`, counters become rates over `window`;
/// existing rates keep their own interval.
///
/// Its kernel is [`TimestampSplitKernel`], which sends a batch without timestamped points to `default` whole, and
/// reports the native passthrough telemetry under the component it is built for.
#[derive(Clone, Copy, Debug, Default)]
pub struct TimestampSplit;

impl OperatorDef for TimestampSplit {
    fn name(&self) -> &'static str {
        names::TIMESTAMP_SPLIT
    }

    fn summary(&self) -> &'static str {
        "Separates timestamped points, which bypass aggregation, turning timestamped counters into rates."
    }

    fn params(&self) -> &'static [ParamSpec] {
        TIMESTAMP_SPLIT_PARAMS
    }

    fn check_params(&self, params: &DecodedParams) -> Vec<ParamError> {
        let mut errors = Vec::new();
        check_whole_seconds("window", params.duration("window"), false, &mut errors);
        errors
    }

    fn ports(&self, _params: &DecodedParams) -> Ports {
        Ports {
            inputs: vec![InputPort {
                name: PortName::from_static(IN),
                accepts: PortType::ANY,
            }],
            outputs: vec![
                OutputPort {
                    name: PortName::from_static(DEFAULT_PORT),
                    mapping: PortMapping {
                        kinds: KindMap::IDENTITY,
                        timestamps: TimestampMap::Select(TimestampForm::Untimestamped),
                    },
                },
                OutputPort {
                    name: PortName::from_static(PASSTHROUGH_PORT),
                    mapping: PortMapping {
                        kinds: KindMap::IDENTITY.with(Kind::Counter, RATE),
                        timestamps: TimestampMap::Select(TimestampForm::Timestamped),
                    },
                },
            ],
        }
    }

    fn contract(&self, _params: &DecodedParams) -> EffectContract {
        EffectContract {
            reads: PerKind::uniform(FieldSet::of(&[Field::Kind, Field::Points])),
            writes: PerKind::uniform(FieldSet::EMPTY).with(KindSet::of(&[Kind::Counter]), FieldSet::of(&[Field::Kind])),
            cardinality: Cardinality::Expand { max: Some(2) },
            ..EffectContract::passthrough()
        }
    }

    fn cost(&self, _params: &DecodedParams) -> u32 {
        10
    }

    fn kernel(&self, params: &DecodedParams) -> Option<KernelSpec> {
        // `check_params` has made a known window a whole, positive number of seconds.
        let window = params
            .duration("window")
            .and_then(|window| NonZeroU64::new(window.as_secs()));
        Some(KernelSpec::new(move |context| {
            let window = window.ok_or_else(|| unresolved("window"))?;
            let telemetry = TimestampSplitTelemetry::for_kernel(context.component());
            Ok(Box::new(TimestampSplitKernel::new(window, telemetry)))
        }))
    }
}

const TAG_FILTER_RULE: &[ParamSpec] = &[
    ParamSpec::required("metric_name", ParamType::String),
    ParamSpec::required("action", ParamType::Enum(&["include", "exclude"])),
    ParamSpec::required("tags", STRINGS),
];

/// The type of the `rules` parameter of `tag_projection`: a list of `{"metric_name": .., "action": "include" |
/// "exclude", "tags": [..]}`.
pub const TAG_FILTER_RULES: ParamType = ParamType::List(&ParamType::Record(TAG_FILTER_RULE));

const TAG_VALUE_RULE: &[ParamSpec] = &[
    ParamSpec::required("metric_prefix", ParamType::String),
    ParamSpec::required("tag_name", ParamType::String),
    ParamSpec::required("values", STRINGS),
    ParamSpec::required("on_miss", ParamType::Enum(&["remove", "replace"])),
    ParamSpec::optional("replacement", ParamType::String),
];

/// The type of the `value_rules` parameter of `tag_projection`: a list of `{"metric_prefix": .., "tag_name": ..,
/// "values": [..], "on_miss": "remove" | "replace", "replacement": ..}`, with `replacement` present exactly when
/// `on_miss` is `replace`.
pub const TAG_VALUE_RULES: ParamType = ParamType::List(&ParamType::Record(TAG_VALUE_RULE));

const TAG_PROJECTION_PARAMS: &[ParamSpec] = &[
    ParamSpec::required("rules", ParamType::Bindable(&TAG_FILTER_RULES)),
    ParamSpec::required("value_rules", ParamType::Bindable(&TAG_VALUE_RULES)),
    ParamSpec::required("cache_capacity", ParamType::Bindable(&COUNT)),
];

/// Encodes tag projection rules as the value of the `rules` parameter.
pub fn encode_tag_filter_rules(rules: &[TagFilterRule]) -> Value {
    Value::Array(
        rules
            .iter()
            .map(|rule| json!({"metric_name": rule.metric_name, "action": rule.action.as_str(), "tags": rule.tags}))
            .collect(),
    )
}

/// Decodes the known value of the `rules` parameter.
///
/// Returns `None` if the value is not a decoded list of rules, such as an unresolved binding.
pub fn decode_tag_filter_rules(value: &ParamValue) -> Option<Vec<TagFilterRule>> {
    let ParamValue::List(rules) = value.known()? else {
        return None;
    };
    rules
        .iter()
        .map(|rule| {
            let rule = record(rule)?;
            let action = match rule.str("action")? {
                "include" => TagFilterAction::Include,
                _ => TagFilterAction::Exclude,
            };
            Some(TagFilterRule {
                metric_name: rule.str("metric_name")?.to_string(),
                action,
                tags: strings(rule.list("tags")?)?,
            })
        })
        .collect()
}

/// Encodes tag value rules as the value of the `value_rules` parameter.
pub fn encode_tag_value_rules(rules: &[TagValueRule]) -> Value {
    Value::Array(
        rules
            .iter()
            .map(|rule| {
                let mut members = Map::new();
                members.insert("metric_prefix".into(), json!(rule.metric_prefix));
                members.insert("tag_name".into(), json!(rule.tag_name));
                members.insert("values".into(), json!(rule.values));
                match &rule.on_miss {
                    TagValueMismatch::Remove => {
                        members.insert("on_miss".into(), json!("remove"));
                    }
                    TagValueMismatch::Replace(replacement) => {
                        members.insert("on_miss".into(), json!("replace"));
                        members.insert("replacement".into(), json!(replacement));
                    }
                }
                Value::Object(members)
            })
            .collect(),
    )
}

/// Decodes the known value of the `value_rules` parameter.
///
/// Returns `None` if the value is not a decoded list of rules, such as an unresolved binding, or if a rule replaces
/// values without a replacement.
pub fn decode_tag_value_rules(value: &ParamValue) -> Option<Vec<TagValueRule>> {
    let ParamValue::List(rules) = value.known()? else {
        return None;
    };
    rules
        .iter()
        .map(|rule| {
            let rule = record(rule)?;
            let on_miss = match rule.str("on_miss")? {
                "replace" => TagValueMismatch::Replace(rule.str("replacement")?.to_string()),
                _ => TagValueMismatch::Remove,
            };
            Some(TagValueRule {
                metric_prefix: rule.str("metric_prefix")?.to_string(),
                tag_name: rule.str("tag_name")?.to_string(),
                values: strings(rule.list("values")?)?,
                on_miss,
            })
        })
        .collect()
}

/// The kinds `tag_projection` rewrites.
const PROJECTED: KindSet = KindSet::of(&[Kind::Counter, Kind::Distribution]);

/// The `tag_projection` operator: removes tags and restricts tag values of counters and distributions.
///
/// Parameters: `rules` (bindable; see [`TAG_FILTER_RULES`]), which keep or remove tags of metrics by exact raw name;
/// `value_rules` (bindable; see [`TAG_VALUE_RULES`]), which restrict the values of one tag of metrics by raw name
/// prefix; and `cache_capacity` (the number of contexts whose result is cached; unobservable).
///
/// Only counters and distributions are rewritten; every other kind passes unchanged, even when a rule names it. Rules
/// apply to tags and origin tags alike.
///
/// Its kernel is [`TagProjectionKernel`], with a context memo of `cache_capacity` entries and the native tag filterlist
/// telemetry, reported under the component it is built for. The memo of a reserved node, which stands for the native
/// component, reports under the native memo's identifier, `tag_filterlist/context_cache`; that of a policy node under
/// `<component>/tag_filterlist/context_cache`.
#[derive(Clone, Copy, Debug, Default)]
pub struct TagProjection;

impl OperatorDef for TagProjection {
    fn name(&self) -> &'static str {
        names::TAG_PROJECTION
    }

    fn summary(&self) -> &'static str {
        "Removes tags and restricts tag values of counters and distributions, by rules on the raw name."
    }

    fn params(&self) -> &'static [ParamSpec] {
        TAG_PROJECTION_PARAMS
    }

    fn check_params(&self, params: &DecodedParams) -> Vec<ParamError> {
        let mut errors = Vec::new();
        let Some(rules) = params.list("value_rules") else {
            return errors;
        };
        for (index, rule) in rules.iter().enumerate() {
            let Some(rule) = record(rule) else { continue };
            let path = format!("value_rules[{}].replacement", index);
            match (rule.str("on_miss"), rule.str("replacement")) {
                (Some("replace"), None) => errors.push(error(&path, "a rule that replaces values needs a replacement")),
                (Some("remove"), Some(_)) => {
                    errors.push(error(&path, "a rule that removes tags takes no replacement; remove it"))
                }
                _ => {}
            }
        }
        if errors.is_empty() {
            if let Some(rules) = params.get("value_rules").and_then(decode_tag_value_rules) {
                if let Err(invalid) = validate_tag_value_rules(&rules) {
                    errors.push(error("value_rules", invalid.to_string()));
                }
            }
        }
        errors
    }

    fn ports(&self, _params: &DecodedParams) -> Ports {
        Ports::single(PortType::ANY, PortMapping::IDENTITY)
    }

    fn contract(&self, _params: &DecodedParams) -> EffectContract {
        let projected_reads = FieldSet::of(&[Field::Name, Field::Tags, Field::OriginTags, Field::Kind]);
        EffectContract {
            reads: PerKind::uniform(FieldSet::of(&[Field::Kind])).with(PROJECTED, projected_reads),
            writes: PerKind::uniform(FieldSet::EMPTY).with(PROJECTED, FieldSet::of(&[Field::Tags, Field::OriginTags])),
            identity: PerKind::uniform(Identity::Preserving).with(PROJECTED, Identity::Quotient),
            loss: Loss::Lossy(vec![LossDecl {
                class: LossClass::Policy,
                description: "Removes tags, and removes or replaces disallowed tag values, of counters and \
                              distributions that its rules name.",
            }]),
            ..EffectContract::passthrough()
        }
    }

    fn specify_bounds(&self, params: &DecodedParams, builder: &mut MemoryBoundsBuilder) {
        // Each cached context costs about 64 bytes, as the native component accounts for it.
        if let Some(capacity) = params.integer("cache_capacity") {
            let capacity = usize::try_from(capacity).unwrap_or(usize::MAX);
            builder
                .firm()
                .with_fixed_amount("context cache", capacity.saturating_mul(CONTEXT_CACHE_ENTRY_BYTES));
        }
    }

    fn cost(&self, _params: &DecodedParams) -> u32 {
        // Every counter and distribution is looked up in the context memo.
        15
    }

    fn kernel(&self, params: &DecodedParams) -> Option<KernelSpec> {
        let rules = params.get("rules").and_then(decode_tag_filter_rules);
        let value_rules = params.get("value_rules").and_then(decode_tag_value_rules);
        let capacity = params
            .integer("cache_capacity")
            .and_then(|capacity| usize::try_from(capacity).ok());
        Some(KernelSpec::new(move |context| {
            let rules = rules.as_deref().ok_or_else(|| unresolved("rules"))?;
            let value_rules = value_rules.as_deref().ok_or_else(|| unresolved("value_rules"))?;
            let capacity = capacity.ok_or_else(|| unresolved("cache_capacity"))?;
            let value_rules = ValueRules::new(value_rules).map_err(|problem| KernelBuildError {
                message: problem.to_string(),
            })?;
            Ok(Box::new(TagProjectionKernel::new(
                TagProjectionRules::new(rules, value_rules),
                Some(ContextMemo::new(&tag_projection_memo_id(context), capacity)),
                TagProjectionTelemetry::for_kernel(context.component()),
            )))
        }))
    }
}

/// Returns the identifier the context memo of a `tag_projection` kernel reports under.
///
/// A reserved node stands for the native component, so its memo keeps the native memo's identifier, which is not
/// scoped to a component. A node in the policy namespace has its own, named after the component it is built for.
fn tag_projection_memo_id(context: &KernelBuildContext) -> String {
    if context.node().is_reserved() {
        NATIVE_CONTEXT_CACHE_ID.to_string()
    } else {
        format!("{}/tag_filterlist/context_cache", context.component().component_id())
    }
}
