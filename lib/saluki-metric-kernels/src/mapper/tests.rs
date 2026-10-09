use std::{collections::HashMap, num::NonZeroUsize};

use proptest::{collection::vec, prelude::*};
use saluki_core::{
    components::{transforms::SynchronousTransform, BuildContext},
    data_model::event::{
        metric::{
            context::{Context, ContextResolverBuilder},
            Metric,
        },
        Event,
    },
    execution::{Budget, PortOutputs, Turn, TurnCounters},
    topology::EventsBuffer,
};
use saluki_metrics::test::TestRecorder;

use super::{reference, validate_profiles, MapperError, MetricMapper};
use crate::params::{MapperProfile, MatchType, MetricMapping};

const INTERNER_BYTES: NonZeroUsize = NonZeroUsize::new(64 * 1024).expect("not zero");

fn mapping(metric_match: &str, match_type: MatchType, name: &str, tags: &[(&str, &str)]) -> MetricMapping {
    MetricMapping {
        metric_match: metric_match.to_string(),
        match_type,
        name: name.to_string(),
        tags: tags
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect(),
    }
}

fn profile(name: &str, prefix: &str, mappings: Vec<MetricMapping>) -> MapperProfile {
    MapperProfile {
        name: name.to_string(),
        prefix: prefix.to_string(),
        mappings,
    }
}

fn simple_mapping_profile() -> Vec<MapperProfile> {
    vec![profile(
        "test",
        "test.",
        vec![mapping(
            "test.job.duration.*.*",
            MatchType::Wildcard,
            "test.job.duration",
            &[("job_type", "$1"), ("job_name", "$2")],
        )],
    )]
}

fn mapper_with_cache(profiles: Vec<MapperProfile>, cache_size: usize) -> MetricMapper {
    MetricMapper::new("test_mapper", &profiles, cache_size, INTERNER_BYTES).expect("profiles should compile")
}

fn counter_metric(name: &'static str, tags: &[&'static str]) -> Metric {
    Metric::counter(Context::from_static_parts(name, tags), 1.0)
}

#[track_caller]
fn assert_tags(context: &Context, expected_tags: &[&str]) {
    for tag in expected_tags {
        assert!(context.tags().has_tag(tag), "missing tag: {}", tag);
    }
    assert_eq!(context.tags().len(), expected_tags.len(), "unexpected number of tags");
}

#[tokio::test]
async fn cache_hit_returns_same_result_as_miss() {
    let mut mapper = mapper_with_cache(simple_mapping_profile(), 1000);
    assert_eq!(mapper.cache_len(), Some(0));

    let metric = counter_metric("test.job.duration.my_type.my_name", &[]);
    let first = mapper.try_map(metric.context()).expect("should have remapped");
    assert_eq!(mapper.cache_len(), Some(1));

    let metric = counter_metric("test.job.duration.my_type.my_name", &[]);
    let second = mapper.try_map(metric.context()).expect("should have remapped");
    assert_eq!(mapper.cache_len(), Some(1));

    assert_eq!(first.name(), second.name());
    assert_eq!(first.name(), "test.job.duration");
    assert_tags(&first, &["job_type:my_type", "job_name:my_name"]);
    assert_tags(&second, &["job_type:my_type", "job_name:my_name"]);
}

#[tokio::test]
async fn negative_results_are_cached() {
    let mut mapper = mapper_with_cache(simple_mapping_profile(), 1000);

    let metric = counter_metric("unrelated.metric.name", &[]);
    assert!(mapper.try_map(metric.context()).is_none());
    assert_eq!(mapper.cache_len(), Some(1));

    let metric = counter_metric("unrelated.metric.name", &[]);
    assert!(mapper.try_map(metric.context()).is_none());
    assert_eq!(mapper.cache_len(), Some(1));
}

#[tokio::test]
async fn cache_disabled_when_size_is_zero() {
    let mut mapper = mapper_with_cache(simple_mapping_profile(), 0);
    assert_eq!(mapper.cache_len(), None);

    let metric = counter_metric("test.job.duration.my_type.my_name", &[]);
    let context = mapper.try_map(metric.context()).expect("should have remapped");
    assert_eq!(context.name(), "test.job.duration");
    assert_tags(&context, &["job_type:my_type", "job_name:my_name"]);

    assert!(mapper
        .try_map(counter_metric("unrelated.metric", &[]).context())
        .is_none());
    assert_eq!(mapper.cache_len(), None);
}

#[tokio::test]
async fn cache_evicts_older_entry_and_retains_newest_within_capacity() {
    let mut mapper = mapper_with_cache(simple_mapping_profile(), 2);

    // Insert three distinct metric names into a capacity-2 result cache, in order a, b, then c.
    for suffix in ["a", "b", "c"] {
        let name = format!("test.job.duration.t.{}", suffix);
        let metric = counter_metric(Box::leak(name.into_boxed_str()), &[]);
        mapper.try_map(metric.context()).expect("should have remapped");
    }

    let a = mapper.cache_contains("test.job.duration.t.a");
    let b = mapper.cache_contains("test.job.duration.t.b");
    let c = mapper.cache_contains("test.job.duration.t.c");

    // The cache must respect its configured capacity...
    assert!(
        mapper.cache_len().unwrap() <= 2,
        "cache should not exceed configured capacity (got {})",
        mapper.cache_len().unwrap()
    );
    // ...eviction must actually have happened (three distinct names cannot all fit in a capacity-2 cache)...
    assert!(!(a && b && c), "at least one older entry must have been evicted");
    // ...the most-recently-inserted name ("c") must be the entry that survives eviction...
    assert!(c, "the most-recently-inserted metric name should survive eviction");
    // ...and with "c" retained at capacity 2, at most one of the two older names may remain.
    assert!(
        !(a && b),
        "only one older entry may coexist with the newest entry at capacity 2"
    );
}

#[tokio::test]
async fn flood_of_identical_names_populates_single_cache_entry() {
    // Many profiles, only the last one matches the test metric. A flood of identical names should be served from the
    // cache after the first call.
    let mut profiles = (0..50)
        .map(|i| {
            profile(
                &format!("noise-{i}"),
                &format!("noise{i}."),
                vec![mapping(
                    &format!("noise{i}.*"),
                    MatchType::Wildcard,
                    "noise.mapped",
                    &[],
                )],
            )
        })
        .collect::<Vec<_>>();
    profiles.push(profile(
        "real",
        "real.",
        vec![mapping(
            "real.metric.*",
            MatchType::Wildcard,
            "real.mapped",
            &[("x", "$1")],
        )],
    ));

    let mut mapper = mapper_with_cache(profiles, 16);

    for _ in 0..10_000 {
        let metric = counter_metric("real.metric.flood", &[]);
        let context = mapper.try_map(metric.context()).expect("should have remapped");
        assert_eq!(context.name(), "real.mapped");
    }

    assert_eq!(
        mapper.cache_len(),
        Some(1),
        "flood of identical names should populate exactly one cache entry"
    );
}

#[test]
fn invalid_profiles_are_rejected_with_the_first_problem_in_order() {
    // Each case is `(description, profiles, expected message)`. Checks run profile by profile: its name, its prefix,
    // then each mapping's name, match, and pattern.
    let valid = mapping("test.valid", MatchType::Wildcard, "mapped", &[]);
    let cases: Vec<(&str, Vec<MapperProfile>, &str)> = vec![
        (
            "profile with an empty name",
            vec![profile("", "test.", vec![])],
            "missing profile name",
        ),
        (
            "profile with an empty prefix",
            vec![profile("test", "", vec![])],
            "missing prefix for profile: test",
        ),
        (
            "mapping with an empty name",
            vec![profile(
                "test",
                "test.",
                vec![mapping("test.job.*", MatchType::Wildcard, "", &[])],
            )],
            "profile: test, mapping num 0: name is required",
        ),
        (
            "mapping with an empty match and an empty name reports the name first",
            vec![profile(
                "test",
                "test.",
                vec![mapping("", MatchType::Wildcard, "", &[])],
            )],
            "profile: test, mapping num 0: name is required",
        ),
        (
            "second mapping with an empty match",
            vec![profile(
                "test",
                "test.",
                vec![valid.clone(), mapping("", MatchType::Regex, "mapped", &[])],
            )],
            "profile: test, mapping num 1: match is required",
        ),
        (
            "wildcard match with disallowed characters",
            vec![profile(
                "test",
                "test.",
                vec![mapping("test.[]duration.*", MatchType::Wildcard, "mapped", &[])],
            )],
            "invalid wildcard match pattern `test.[]duration.*`, it does not match allowed match regex \
             `^[a-zA-Z0-9\\-_*.]+$`",
        ),
        (
            "wildcard match with consecutive wildcards",
            vec![profile(
                "test",
                "test.",
                vec![mapping("test.invalid.**", MatchType::Wildcard, "mapped", &[])],
            )],
            "invalid wildcard match pattern `test.invalid.**`, it should not contain consecutive `*`",
        ),
        (
            "regular expression that does not compile",
            vec![profile(
                "test",
                "test.",
                vec![mapping("test.(unclosed", MatchType::Regex, "mapped", &[])],
            )],
            "Failed to compile regular expression `^test.(unclosed$` for `regex` match type",
        ),
        (
            "an earlier profile's problem wins over a later one's",
            vec![
                profile("first", "", vec![]),
                profile("", "second.", vec![valid.clone()]),
            ],
            "missing prefix for profile: first",
        ),
    ];

    for (description, profiles, expected) in cases {
        let error = validate_profiles(&profiles).expect_err(description);
        assert_eq!(error.to_string(), expected, "[{}]", description);
    }

    assert!(validate_profiles(&[profile("test", "*", vec![valid])]).is_ok());
}

#[test]
fn regex_errors_keep_their_cause() {
    let error = validate_profiles(&[profile(
        "test",
        "test.",
        vec![mapping("(", MatchType::Regex, "mapped", &[])],
    )])
    .expect_err("an unclosed group does not compile");
    match &error {
        MapperError::InvalidRegex {
            pattern, match_type, ..
        } => {
            assert_eq!(pattern, "^($");
            assert_eq!(*match_type, "regex");
        }
        other => panic!("unexpected error: {other}"),
    }
    assert!(std::error::Error::source(&error).is_some());
}

#[tokio::test]
async fn telemetry_is_named_after_the_identity() {
    let recorder = TestRecorder::default();
    let _local = metrics::set_default_local_recorder(&recorder);

    let _mapper = MetricMapper::new("dsd_enrich", &simple_mapping_profile(), 10, INTERNER_BYTES).unwrap();

    assert_eq!(
        recorder.gauge((
            "context_resolver_interner_capacity_bytes",
            &[("resolver_id", "dsd_enrich/dsd_mapper/primary")]
        )),
        Some(INTERNER_BYTES.get() as f64)
    );
    assert_eq!(
        recorder.gauge((
            "cache_weight_limit",
            &[("cache_id", "dsd_enrich/dsd_mapper/result_cache")]
        )),
        Some(10.0)
    );
}

#[tokio::test]
async fn the_kernel_maps_a_batch_in_place() {
    let mut mapper = mapper_with_cache(simple_mapping_profile(), 1000);

    let mut events = EventsBuffer::default();
    assert!(events
        .try_push(Event::Metric(counter_metric("test.job.duration.my_type.my_name", &[])))
        .is_none());
    assert!(events
        .try_push(Event::Metric(counter_metric("unrelated.metric", &["keep:me"])))
        .is_none());

    let header = Default::default();
    let mut budget = Budget::unlimited();
    let mut outputs = PortOutputs::new(1);
    let mut counters = TurnCounters::default();
    let mut turn = Turn::new(
        &header,
        std::time::Instant::now(),
        &mut budget,
        &mut outputs,
        &mut counters,
    );
    let progress = saluki_core::execution::Kernel::consume(&mut mapper, &mut turn, &mut events);
    assert!(matches!(progress, saluki_core::execution::Progress::Done));
    assert!(counters.is_empty());

    let metrics: Vec<Metric> = events.into_iter().filter_map(Event::try_into_metric).collect();
    assert_eq!(metrics.len(), 2);
    assert_eq!(metrics[0].context().name(), "test.job.duration");
    assert_tags(metrics[0].context(), &["job_type:my_type", "job_name:my_name"]);
    assert_eq!(metrics[1].context().name(), "unrelated.metric");
    assert_tags(metrics[1].context(), &["keep:me"]);
}

#[tokio::test]
async fn mapping_keeps_the_host_and_origin_tags() {
    let profiles = vec![profile(
        "test",
        "test.",
        vec![mapping(
            "test.job.duration.*",
            MatchType::Wildcard,
            "test.job.duration",
            &[("job_name", "$1")],
        )],
    )];

    let mut resolver = ContextResolverBuilder::for_tests().build();
    let context = resolver
        .resolve_with_host_and_origin_tags(
            "test.job.duration.worker",
            "host-a",
            &["env:prod"],
            saluki_core::data_model::tags::TagSet::from_iter([saluki_core::data_model::tags::Tag::from(
                "container:one",
            )])
            .into_shared(),
        )
        .expect("context should resolve");

    let mut mapper = mapper_with_cache(profiles, 1000);
    for _ in 0..2 {
        // The second mapping comes from the result cache.
        let mapped = mapper.try_map(&context).expect("should have remapped");
        assert_eq!(mapped.name(), "test.job.duration");
        assert_eq!(mapped.host(), Some("host-a"));
        assert_tags(&mapped, &["env:prod", "job_name:worker"]);
        assert!(mapped.origin_tags().has_tag("container:one"));
        assert_eq!(mapped.origin_tags().len(), 1);
    }
}

// Differential tests against the frozen pre-extraction mapper.

/// Converts kernel parameters into the legacy configuration the frozen mapper takes.
///
/// `explicit_wildcard` chooses, per mapping, between spelling the wildcard match type out and leaving it empty, which
/// natively means wildcard.
fn reference_profiles(profiles: &[MapperProfile], explicit_wildcard: bool) -> Vec<reference::DogStatsDMapperProfile> {
    profiles
        .iter()
        .map(|profile| reference::DogStatsDMapperProfile {
            name: profile.name.clone(),
            prefix: profile.prefix.clone(),
            mappings: profile
                .mappings
                .iter()
                .map(|mapping| reference::DogStatsDMetricMapping {
                    metric_match: mapping.metric_match.clone(),
                    match_type: match mapping.match_type {
                        MatchType::Wildcard if !explicit_wildcard => String::new(),
                        match_type => match_type.as_str().to_string(),
                    },
                    name: mapping.name.clone(),
                    tags: mapping.tags.iter().cloned().collect::<HashMap<_, _>>(),
                })
                .collect(),
        })
        .collect()
}

/// A context's parts, with tags sorted, for comparing contexts beyond their identity hash.
fn parts(context: &Context) -> (String, Option<String>, Vec<String>, Vec<String>) {
    let mut tags = context
        .tags()
        .into_iter()
        .map(|tag| tag.as_str().to_string())
        .collect::<Vec<_>>();
    tags.sort();
    let mut origin_tags = context
        .origin_tags()
        .into_iter()
        .map(|tag| tag.as_str().to_string())
        .collect::<Vec<_>>();
    origin_tags.sort();
    (
        context.name().to_string(),
        context.host().map(str::to_string),
        tags,
        origin_tags,
    )
}

const SEGMENTS: &[&str] = &["a", "b", "c", "job", "12", "x_y", "Up", "é", ""];

fn arb_segment() -> impl Strategy<Value = &'static str> {
    proptest::sample::select(SEGMENTS)
}

fn arb_metric_name() -> impl Strategy<Value = String> {
    vec(arb_segment(), 1..5).prop_map(|segments| segments.join("."))
}

fn arb_wildcard() -> impl Strategy<Value = String> {
    // Mostly valid patterns, with the occasional disallowed character or doubled wildcard.
    let segment = prop_oneof![
        6 => proptest::sample::select(&["a", "b", "c", "job", "12", "*", "*_y", "x_*"][..]).prop_map(str::to_string),
        1 => proptest::sample::select(&["**", "a[b]", "^a", "é"][..]).prop_map(str::to_string),
    ];
    vec(segment, 1..5).prop_map(|segments| segments.join("."))
}

fn arb_regex() -> impl Strategy<Value = String> {
    proptest::sample::select(
        &[
            r"a\.(\w+)",
            r"(.*)\.c",
            r"a\.(\w+)\.(\w+)",
            r"([a-z]+)\.(\d+)",
            r".*",
            r"job\.(?P<kind>\w+)",
            r"(unclosed",
        ][..],
    )
    .prop_map(str::to_string)
}

fn arb_mapping() -> impl Strategy<Value = MetricMapping> {
    let pattern = prop_oneof![
        3 => arb_wildcard().prop_map(|pattern| (pattern, MatchType::Wildcard)),
        2 => arb_regex().prop_map(|pattern| (pattern, MatchType::Regex)),
        1 => Just((String::new(), MatchType::Wildcard)),
    ];
    let name = proptest::sample::select(&["mapped", "mapped.$1", "${2}_y", "m.$1.$2", "$kind", ""][..]);
    let tags = proptest::sample::subsequence(&["k1", "k2", "k3"][..], 0..=3).prop_flat_map(|keys| {
        let values = vec(
            proptest::sample::select(&["$1", "v", "${1}_x", "$2", ""][..]),
            keys.len(),
        );
        (Just(keys), values)
    });
    (pattern, name, tags).prop_map(|((metric_match, match_type), name, (keys, values))| MetricMapping {
        metric_match,
        match_type,
        name: name.to_string(),
        tags: keys
            .into_iter()
            .zip(values)
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect(),
    })
}

fn arb_profiles() -> impl Strategy<Value = Vec<MapperProfile>> {
    let profile = (
        proptest::sample::select(&["p", "q", ""][..]),
        proptest::sample::select(&["*", "a.", "a.b", "job.", "", "c"][..]),
        vec(arb_mapping(), 0..4),
    )
        .prop_map(|(name, prefix, mappings)| MapperProfile {
            name: name.to_string(),
            prefix: prefix.to_string(),
            mappings,
        });
    vec(profile, 0..4)
}

fn arb_context() -> impl Strategy<Value = Context> {
    let tags = proptest::sample::subsequence(&["env:prod", "k1:old", "bare", "k2:"][..], 0..=4);
    let host = proptest::option::of(proptest::sample::select(&["host-a", ""][..]));
    (arb_metric_name(), tags, host).prop_map(|(name, tags, host)| {
        let tags = tags.into_iter().map(saluki_core::data_model::tags::Tag::from);
        let context = Context::from_parts(name, saluki_core::data_model::tags::TagSet::from_iter(tags));
        context.with_host(host.map(stringtheory::MetaString::from))
    })
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("the test runtime builds")
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn property_test_mapper_matches_the_pre_extraction_mapper(
        profiles in arb_profiles(),
        explicit_wildcard in any::<bool>(),
        cache_size in prop_oneof![Just(0usize), Just(2usize), Just(1000usize)],
        contexts in vec(arb_context(), 1..24),
        repeats in 1usize..3,
    ) {
        let runtime = runtime();
        let _context = runtime.enter();

        let expected = reference::DogStatsDMapperConfiguration::new(
            INTERNER_BYTES,
            cache_size,
            reference_profiles(&profiles, explicit_wildcard),
        )
        .build_mapper(BuildContext::test_transform("property_mapper"));
        let actual = MetricMapper::new("property_mapper", &profiles, cache_size, INTERNER_BYTES);

        let (mut expected, mut actual) = match (expected, actual) {
            (Ok(expected), Ok(actual)) => (expected, actual),
            (Err(expected), Err(actual)) => {
                prop_assert_eq!(expected.to_string(), actual.to_string());
                return Ok(());
            }
            (expected, actual) => {
                return Err(TestCaseError::fail(format!(
                    "the frozen mapper built: {}, the kernel built: {}",
                    expected.is_ok(),
                    actual.is_ok()
                )));
            }
        };

        // Map every context several times, so later rounds go through the result caches.
        for _ in 0..repeats {
            for context in &contexts {
                let expected_context = expected.try_map(context);
                let actual_context = actual.try_map(context);
                prop_assert_eq!(
                    expected_context.as_ref().map(parts),
                    actual_context.as_ref().map(parts),
                    "mapping {:?}",
                    context.name()
                );
                prop_assert_eq!(expected.cache_len(), actual.cache_len());
            }
        }
    }

    #[test]
    fn property_test_buffer_mapping_matches_the_pre_extraction_transform(
        profiles in arb_profiles(),
        contexts in vec(arb_context(), 1..24),
    ) {
        let runtime = runtime();
        let _context = runtime.enter();

        let Ok(expected) = reference::DogStatsDMapperConfiguration::new(
            INTERNER_BYTES,
            1000,
            reference_profiles(&profiles, true),
        )
        .build_mapper(BuildContext::test_transform("property_mapper")) else {
            prop_assert!(validate_profiles(&profiles).is_err());
            return Ok(());
        };
        let mut expected = reference::DogStatsDMapper { metric_mapper: expected };
        let mut actual = MetricMapper::new("property_mapper", &profiles, 1000, INTERNER_BYTES).unwrap();

        let mut buffer = EventsBuffer::default();
        for (index, context) in contexts.iter().enumerate() {
            let event = if index % 5 == 4 {
                Event::Metric(Metric::gauge(context.clone(), 2.0))
            } else {
                Event::Metric(Metric::counter(context.clone(), 1.0))
            };
            prop_assert!(buffer.try_push(event).is_none());
        }
        let mut expected_buffer = buffer.clone();
        SynchronousTransform::transform_buffer(&mut expected, &mut expected_buffer);
        actual.map_buffer(&mut buffer);

        let describe = |buffer: EventsBuffer| {
            buffer
                .into_iter()
                .map(|event| {
                    let metric = event.try_into_metric().expect("only metrics were pushed");
                    (parts(metric.context()), metric.values().as_str())
                })
                .collect::<Vec<_>>()
        };
        prop_assert_eq!(describe(expected_buffer), describe(buffer));
    }
}
