# Rust test suite cleanup plan

This file tracks an in-progress cleanup of the Rust unit and property test suite, produced by an exhaustive
crate-by-crate, module-by-module audit (44 units covering all 35 workspace crates, cross-referenced against doc
comments on the code under test). The work is carried out as a stack of local Graphite-tracked branches on top of
`tobz/test-cleanup`, one branch per group below. Each group lands as its own commit; check it off here as part of
that commit. **This file is deleted in the commit that lands the last group.**

Conventions established by this cleanup are documented in `docs/development/testing-patterns.md` — read that first
if a group's fix isn't obvious from its one-line description.

Groups are ordered so that a group establishing a shared pattern or helper always lands before the groups that would
otherwise keep hand-rolling the thing it replaces.

## Groups

- [ ] **GROUP-000 — Docs groundwork** (`tobz/test-cleanup-docs-groundwork`, `docs(test)`)
  Add `docs/development/testing-patterns.md` (new unit/property test style guide), add Loom/Miri/Antithesis sections
  plus a cross-link to `docs/development/testing.md`, and add the `property_test_` CI-load-bearing note plus an
  Antithesis-vs-unit-test disambiguation pointer to `AGENTS.md`'s Gotchas section.

- [x] **G1 — Expose `ComponentContext` test-util helpers cross-crate + heartbeat/context coverage**
  (`tobz/test-cleanup-componentcontext-test-util`, `test(core)`)
  Foundational: unblocks every later group that constructs a `ComponentContext` in a test.
  - [medium/small] `ComponentContext::test_source`/`test_relay`/`test_decoder`/`test_transform`/`test_encoder`/
    `test_forwarder`/`test_destination` (`lib/saluki-core/src/components/mod.rs:127-187`) are individually
    `#[cfg(test)]`-gated, invisible cross-crate; 5 of 7 are never called anywhere. About 8 downstream sites
    (`saluki-components` dogstatsd/otlp/aggregate/dogstatsd_mapper, `agent-data-plane` ottl processors) hand-roll the
    identical `SubsystemIdentifier`/`ComponentId` construction instead (KI-1).
  - [medium/medium] The heartbeat source (`lib/saluki-components/src/sources/heartbeat/mod.rs`) has zero tests
    because there's no way to construct a `SourceContext` for it outside `saluki-core` — a direct casualty of KI-1.
  - [medium/small] Documented double-take panics on `*Context::take_health_handle`/`take_shutdown_handle` are
    untested in all 7 context files.

- [x] **G2 — Shared config test-helper foundation** (`tobz/test-cleanup-config-test-helper-foundation`, `test(config)`)
  - [low/small] A `xxx_config_from(value) -> XxxConfiguration` async test helper is duplicated verbatim across 15+
    config/component test files.
  - [medium/small] The env-var test mutex guarding `ConfigurationLoader` is fragmented into multiple unsynchronized
    `Mutex` instances instead of one shared one.
  - [medium/trivial] `MrfConfiguration::metrics_endpoint_override`'s disabled short-circuit branch is never tested.
  - [medium/small] Documented guard/precedence branches are untested elsewhere in the config module (e.g.
    `DatadogRemapper` first-write-wins).
  - [high/small] `DataPlaneConfiguration`'s documented pipeline-requirement predicates and stop-timeout
    default/overflow branches have zero test coverage.
  - [low/trivial] Repeated per-test setup/construction boilerplate isn't extracted into a shared helper.

- [x] **G3 — saluki-tls test suite cleanup** (`tobz/test-cleanup-tls-self-signed-test-helper`, `test(tls)`)
  - [medium/small] Self-signed TLS certificate generation via `rcgen` is duplicated across `saluki-tls`, `saluki-app`,
    and `saluki-components`, and the one copy that could be shared is trapped in a `tests/` directory (KI-5).
  - [medium/small] The public root-cert wrapper and TLS builder security/config methods are never exercised.
  - [low/medium] The FIPS-compliance check functions' documented error path is never triggered.
  - [medium/small] `ClientTLSConfigBuilder` methods' documented behavior is never verified end-to-end.
  - [low/trivial] Repeated key-log test setup boilerplate is duplicated across `saluki-tls` tests.

- [ ] **G4 — saluki-core observability/metrics & pooling test cleanup**
  (`tobz/test-cleanup-observability-metrics-pooling-tests`, `test(core)`)
  - [low/small] `DebuggingRecorder`/`TestRecorder` metrics-snapshot test helpers are duplicated ad hoc or left unused
    (KI-6) instead of standardizing on one.
  - [low/small] Documented health telemetry metrics are never verified by any test.
  - [medium/small] `FixedSizeObjectPool` and `OnDemandObjectPool` have zero test coverage; `ElasticObjectPool`'s
    EWMA-driven background shrinker is never actually run in tests.
  - [high/small] The histogram eviction-race fix has no regression test or loom model, unlike the counter-path fix
    landed in the same commit.
  - [medium/small] `histogram.rs` and `remapper.rs` have zero direct unit tests despite non-trivial,
    production-load-bearing logic (bucket math, `RemapperRule` fan-out matching).
  - [low/trivial] A small metrics-processor test-harness helper was independently reimplemented in two sibling files.
  - [low/small] Several small test groups differ only by an input value/scenario and duplicate setup —
    table-driven/merge candidates.

- [ ] **G5 — saluki-core runtime supervision & topology test cleanup**
  (`tobz/test-cleanup-runtime-topology-wait-until`, `test(core)`)
  - [medium/medium] No shared `wait_until`/`assert_eventually` helper exists repo-wide; bespoke polling loops and
    blind fixed-duration sleeps proliferate as startup/shutdown barriers instead of readiness polling (KI-7).
  - [medium/medium] Config-update-then-await-watcher boilerplate is duplicated 5x across 3 components.
  - [low/small] Spawn-supervisor/shutdown/timeout scaffold is copy-pasted across 4 `blueprint.rs` tests, and a
    similar run/sleep/shutdown-assert suffix repeats across ~10 restart-type tests in `supervisor.rs`.
  - [medium/medium] `dedicated.rs` (the OS-thread dedicated runtime) is entirely untested, including via
    `supervisor.rs`'s own integration tests.
  - [low/small] Documented restart-policy edge cases are never exercised (dynamic children lost on one-for-all
    restart; `intensity == 0` immediate shutdown).
  - [medium/small] `DataspaceRegistry::current_values<T>()`'s documented snapshot-read API is never called directly
    by any test.
  - [low/trivial] A broken intra-doc link references a nonexistent type, `FutureProcess`.
  - [medium/trivial] `Graph::add_edge`'s terminal-node-as-source rejection branch is untested and undocumented.
  - [medium/small] `TopologyBuildState::recalculate_bounds`'s memory-bounds arithmetic has zero test coverage.
  - [low/small] `WorkerPoolConfiguration`'s `Ambient`/`Dedicated`/`Explicit` resolution branches are untested.
  - [medium/small] The dispatcher's documented `add_output`/`attach_sender_to_output` error paths are never tested,
    and `ComponentOutputId::from_definition`'s documented error branch has no direct test.
  - [low/small] Defensive invariant-violation guards in `dispatch_one_inner`/`EventBufferManager::try_push` are
    untested (reachability/documentation status unclear — worth confirming during implementation).
  - [medium/medium] The dispatcher's default-output/named-output test pairs are structural clones: 22 functions,
    11 pairs, that could collapse to one table-driven test (KI-11).

- [ ] **G6 — dogstatsd component test cleanup** (`tobz/test-cleanup-dogstatsd-test-cleanup`, `test(components)`)
  - [medium/medium] `MockWorkloadProvider` is hand-rolled with 3 divergent shapes across `origin.rs`,
    `replay/reader.rs`, and `replay/writer.rs`, because `NoopWorkloadProvider` offers no configuration seam (KI-2).
  - [high/small] `OriginTagsResolver::resolve_origin_tags`'s live, non-replay production path is never exercised
    through the real entry point — only through the replay bypass.
  - [medium/small] Test-support helpers (`shared_tags`, `wait_until_inactive`) are duplicated verbatim across the
    dogstatsd `replay/*` files (KI-3).
  - [high/small] `capture_api.rs`'s `trigger_handler` has zero tests, unlike its structurally identical sibling
    `replay_api.rs`.
  - [medium/small] `read_next`'s documented corrupt/oversized length-prefix branch is untested, and the existing
    test's name mischaracterizes what it actually covers.
  - [medium/medium] `io_buffer.rs` has zero unit tests despite a fully documented retain/release/collapse contract;
    `PacketForwarder::connect`'s timeout, failure, and idempotency branches are never exercised.
  - [low/small] `ContextResolvers::new`, the real production constructor, has no direct test — all tests use a
    test-only bypass, so its documented error path and interner-sharing contract are untested.
  - [high/medium] The origin-metric eviction test's name implies registry-level verification, but it only checks
    local LRU bookkeeping — it never verifies actual metric-registry deletion.
  - [medium/medium] `dogstatsd_mapper/mod.rs` has 24 near-identical config-driven tests, the flagship KI-11
    table-driven candidate; separately, `MapperProfileConfigs::build`'s custom empty-field validation branches are
    unreachable in tests because serde's required-field check short-circuits first.
  - [medium/small] `DogStatsDMapper`'s public `SynchronousTransform` entry point (`transform_buffer`) is never
    exercised by any test — only its internal helpers are.

- [ ] **G7 — saluki-components transforms test cleanup**
  (`tobz/test-cleanup-transforms-test-cleanup`, `test(components)`)
  aggregate, host_enrichment, chained, metric_router, checks_ipc, trace_obfuscation, trace_sampler, apm_stats
  - [low/trivial] `metric_router`'s `test_input_validation` inlines three near-identical validation cases instead
    of a case table/loop.
  - [high/medium] `SynchronousTransform` components' core `transform_buffer` logic goes untested across multiple
    transforms; `HostEnrichment`'s only test never exercises its documented hostname-enrichment behavior.
  - [medium/medium] The `chained` transform has zero tests despite documented ordering and id-generation behavior.
  - [medium/small] `HistogramConfiguration`'s `TryFrom` error paths, `try_split_timestamped_values`, and
    `AggregationState::flush`'s documented default (`flush_open_buckets=false`) path all lack test coverage in the
    aggregate transform.
  - [low/small] Hostname propagation/fallback test pairs are duplicated verbatim across event kinds in `checks_ipc`.
  - [high/medium] trace_obfuscation's span-dispatch logic and `Obfuscator` facade have zero direct tests; the Valkey
    obfuscation path is entirely untested even though its doc mirrors the tested Redis path; `sql_filters.rs` and
    `sql_tokenizer.rs` have no test functions of their own, and `ObfuscatedSQL.table_names` is documented but never
    asserted by any test.
  - [medium/small] trace_sampler's `score_sampler.rs` pre-sampler/global-rate weighting and `core_sampler.rs`
    bucket-rotation/TPS-redistribution math have no direct unit tests despite documented worked examples; a stale
    module `TODO` claims already-implemented, already-tested features are still missing.
  - [low/small] `apm_stats/statsraw.rs` has 5 tests that hand-roll verbose 12-15-field `StatSpan` literals instead
    of a shared builder helper; `AggregationRegistry`'s documented ref-counting/eviction/dedup semantics are
    untested and related accessors are dead code.

- [ ] **G8 — Datadog encoders/destinations/forwarders test cleanup**
  (`tobz/test-cleanup-encoders-destinations-test-cleanup`, `test(components)`)
  - [medium/medium] Six `run_request_builder` integration tests in `datadog/metrics/mod.rs` share unextracted
    setup/teardown boilerplate, and are themselves oversized, multi-concern tests (KI-12).
  - [high/medium] Datadog encoder payload-building/enrichment logic, and the logs/traces encoders' documented
    enrichment/formatting behavior, lack unit tests beyond config smoke tests.
  - [medium/medium] The `dsd_stats` destination has zero tests despite a non-trivial stateful collection protocol;
    the Prometheus destination's core metric-translation/merge/tag-collection logic is untested, including its
    documented distribution-as-DDSketch-summary limitation.
  - [low/small] Forwarders' pure mapping/normalization functions are untested (otlp `normalize_endpoint`; datadog
    `get_dd_endpoint_name`/`transaction_metadata_from_payload_metadata`).

- [ ] **G9 — Split oversized, multi-concern test functions into focused single-behavior tests**
  (`tobz/test-cleanup-split-oversized-multiconcern-tests`, `test(components)`)
  - [low/small] Test functions bundle multiple unrelated concerns/scenarios in one body instead of splitting or
    looping (KI-12), and sibling tests duplicate identical setup/driver/struct-literal scaffolding instead of
    sharing a helper — concretely, three otlp transform tests duplicate an identical `TestCase`-struct-and-loop
    table-driven harness that should itself be shared.

- [ ] **G10 — Fix vacuous/weak/tautological assertions and assertion-free proptests repo-wide**
  (`tobz/test-cleanup-fix-vacuous-assertions-sweep`, `test(repo)`)
  - [medium/small] The only coverage for `ProbabilisticSampler::sample` is a tautological determinism test that
    asserts nothing about correctness.
  - [medium/small] Assertions are silently skipped via unguarded `if let Some(...) { assert!(...) }` in several
    places (e.g. `apm_stats` peer-tags aggregation) — a regression that makes the `Option` always `None` would pass.
  - [low/trivial] `dogstatsd_mapper`'s `test_cache_eviction` only bounds cache size, never verifies which entry was
    evicted — a weak, bound-only assertion.
  - [medium/small] The stochastic rounding branch in `apm_stats/statsraw.rs`'s `round()` is never exercised.
  - [medium/small] KI-8: the zero-assertion `bucket_print` test and an assertion-free proptest can never fail
    regardless of a real regression.
  - [low/medium] KI-9: ignored, assertion-free perf/diagnostic tests that only `println!` and live in `mod tests`
    instead of `benches/` are duplicated verbatim across the OTTL processors and elsewhere.
  - [high/medium] `TagFilterlist`'s `Transform::run()` type-guard and context-dedup cache are never exercised
    through the real transform loop — the existing test only asserts trivial metric-construction properties.
  - [medium/small] The DogStatsD metric proptest is pure crash-fuzz with zero `prop_assert!`, and never fuzzes the
    `decode_packet`/event/service-check parsers (KI-16).
  - [low/trivial] `with_slop_factor`'s test checks `Ok`/`Err` only, never the documented percentage arithmetic.

- [ ] **G11 — Repo-wide test naming and proptest-authoring-style consistency sweep**
  (`tobz/test-cleanup-test-naming-style-sweep`, `chore(repo)`)
  - [low/small] Inconsistent test function naming convention across the repo: `test_` prefix vs. bare descriptive
    names (KI-15) — see `docs/development/testing-patterns.md` for the now-documented convention.
  - [low/trivial] Two proptest authoring styles coexist with no prior documented convention: `#[test_strategy::proptest]`
    vs. classic `proptest! {}` block macro (KI-16).
  - [low/trivial] `mod test` (singular) is used in one file instead of the repo-wide `mod tests` convention (KI-14).
  - [low/trivial] A typo in a proptest function name (`dispoint` instead of `disjoint`) and one integration test
    file whose name diverges from its test function name (saluki-tls).

- [ ] **G12 — datadog-common config/endpoints/request-pipeline test cleanup** (AU-01/AU-02)
  (`tobz/test-cleanup-datadog-common-config-tests`, `test(components)`)
  - [low/small] Near-identical single-variant config-flag tests, and formulaic input/output variant tests in
    `endpoints.rs`, should collapse into table-driven tests (KI-10, KI-11).
  - [medium/small] `should_receive_payload`'s doc comment enumerates 5 branches; existing tests exercise only 2
    (series/shadow-series) — the sketch and no-payload branches have no test at all.
  - [medium/trivial] `endpoint_concurrency`/multiplier's documented zero-value clamp, and `validate_targets`'s
    documented empty-target-list `Ready` fallback, are both untested.
  - [low/trivial] `RequestBuilderError::is_recoverable`'s documented recoverability rule is never directly asserted.
  - [medium/small] `middleware::for_resolved_endpoint`/`with_version_info` and `protocol::deserialize_v3_series_mode`'s
    documented behavior are entirely untested.
  - [medium/small] `ComponentTelemetry`'s transaction/error tracking methods are almost entirely untested.
  - [low/small] `PendingTransactions::flush`'s shutdown/disk-persistence path, and `transaction.rs`'s `size_bytes`
    and consumed-body panic/error paths, are untested.

- [ ] **G13 — otlp traces & metrics translation test cleanup** (AU-03/AU-06)
  (`tobz/test-cleanup-otlp-translation-test-cleanup`, `test(components)`)
  - [medium/medium] `traces/transform.rs`'s ported Go-agent branching translation logic is largely untested;
    `otlp/traces/translator.rs`'s documented single-fallback hostname resolution and trace-grouping entry point are
    untested.
  - [high/medium] OTLP Histogram/Summary/ExponentialHistogram translation paths (and the cache extrema methods they
    depend on) are entirely untested, despite `translate_metrics`'s doc claiming full Go `MapMetrics` parity and a
    stale `TODO` claiming histogram-extrema porting is still "pending."
  - [high/medium] OTLP metric-remapping logic (`remap.rs`, `runtime_metrics.rs`) has zero tests despite running on
    every production payload.
  - [medium/small] Instrumentation-scope/library tag-generation functions are only exercised via their empty-scope
    fallback.
  - [low/trivial] The OTLP metrics config `validate()`'s documented histogram-mode/aggregation rejection rule is
    untested.

- [ ] **G14 — agent-data-plane filter components & OTTL processors test cleanup** (AU-25/AU-26)
  (`tobz/test-cleanup-adp-filters-ottl-processors-tests`, `test(agent-data-plane)`)
  - [medium/small] `tag_filterlist`'s `count_*` tests duplicate the distribution-metric tests, differing only by
    metric type and one extra assertion — these should be merged, not kept as separate near-duplicates (KI-10).
  - [low/small] `DogStatsDPrefixFilter` tests hand-roll the full struct literal roughly 12 times instead of a
    parameterized builder helper; the fixture-builder-helper pattern proven in one component isn't reused by its
    sibling.
  - [low/small] `TagFilterlistConfiguration`'s documented `context_cache_capacity` default/override is never tested,
    alongside inconsistent config-smoke-test coverage across sibling components.
  - [medium/trivial] The OTTL processors' documented default `error_mode` (`Propagate`) is never exercised by
    omitting the config key.
  - [low/small] OTTL's per-value-type "set" tests repeat near-identical bodies instead of being table-driven
    (KI-11), and documented error/rejection branches in OTTL and DogStatsD parsers are never exercised.
  - [medium/medium] `apm_onboarding` is fully documented (module docs, explicit `# Errors` sections) yet has zero
    tests for its branchy logic.
  - [low/trivial] A module doc comment overstates implemented capability (span-event filtering is documented but
    not implemented, hence untestable) — fix the doc, not the test.

- [ ] **G15 — saluki-io test cleanup: dogstatsd codec/compression/framing + unix-socket credentials/retry** (AU-28/AU-29)
  (`tobz/test-cleanup-saluki-io-test-cleanup`, `test(io)`)
  - [medium/medium] `Compressor`, `CompressionScheme`, and `CountingWriter` are fully documented but have zero
    direct test coverage in saluki-io's compression module.
  - [low/trivial] `NewlineFramer`'s documented carriage-return-retention behavior, and the DogStatsD event parser's
    documented empty title/text rejection, are both untested.
  - [medium/small] `DogStatsDCodec::decode_packet` and `parse_message_type` dispatch are never directly tested.
  - [high/medium] `SCM_CREDENTIALS` ancillary-data parsing (`uds_recvmsg`, `ancillary.rs`) — a parser of untrusted
    external identity data — has zero test coverage exercising the parsing/malformed-input paths.
  - [medium/small] `RollingExponentialBackoffRetryPolicy`'s documented recovery-error-decrease-factor behavior, and
    `StandardHttpRetryLifecycle`'s documented error-categorization/`Display` logic, are both untested.

- [ ] **G16 — saluki-common & saluki-context test cleanup** (AU-30/AU-31)
  (`tobz/test-cleanup-saluki-common-context-test-cleanup`, `test(common)`)
  - [high/medium] Cache time-to-idle eviction (`cache/expiry.rs`) and its underlying timestamp primitives
    (`time.rs`) are both untested, including the documented eviction-counter side effects.
  - [medium/medium] `spawn_traced` task-spawning helpers and poll-duration instrumentation
    (`task/mod.rs`, `task/instrument.rs`) have no tests, including the documented histogram recording and
    caller-location format.
  - [medium/trivial] `PermissiveBool`'s documented floating-point and out-of-range numeric rejection paths are
    untested; `strings.rs`'s `unsigned_integer_to_string` and `StringBuilder::to_meta_string` are untested.
  - [low/small] Near-duplicate boolean-literal test pairs and other table-driven opportunities exist (KI-10, KI-11).
  - [high/small] `RawExternalData::try_from_str`'s malformed-input branches (empty string, short/garbage parts,
    duplicate/unknown keys) have no direct test — the crate's only test on this file checks `Hash`-impl agreement
    and never calls `try_from_str`. (`OriginTagCardinality::try_from`'s matching logic is *not* a gap: it's already
    table-tested via `saluki-io`'s `helpers.rs` and `saluki-components`' `dogstatsd/tags.rs` wrapper call sites —
    don't duplicate that coverage here.) Custom `Display` implementations elsewhere in the crate are also untested.
  - [medium/small] `tags/mod.rs` has zero tests; `BorrowedTag`'s independently re-implemented name/value splitting
    is unverified. `Context::with_tag_sets_mut`'s documented single-rehash guarantee is never exercised, and
    `TagSet::merge_shared`/`SharedTagSet::extend_from_shared`'s documented dedup/no-dedup contracts are unexercised.

- [ ] **G17 — ddsketch, stringtheory, ottl crate test cleanup** (`tobz/test-cleanup-ddsketch-stringtheory-ottl-tests`, `test(ddsketch)`)
  - [medium/large] Sibling `Store`/`IndexMapping` trait implementations in ddsketch hand-duplicate parallel test
    suites instead of using a generic conformance helper (also present, smaller scale, per KI-4 in stringtheory).
  - [high/medium] The agent DDSketch's public API has no direct unit tests, despite the sibling canonical DDSketch
    being thoroughly tested.
  - [medium/trivial] `test_quantile_bounds` is mischaracterized as a KI-9 perf test in prior notes; it's actually a
    stale, unexplained `#[ignore]` on a passing correctness test — un-ignore it, don't move it to `benches/`.
  - [medium/small] `IndexMapping::validate_proto_mapping`'s doc comment implies distinguishable error kinds, but 2
    of 3 `ProtoConversionError` variants are never triggered by any test.
  - [medium/medium] KI-4: `fixed_size.rs` and `map.rs` duplicate an entire test-fixture/proptest suite
    (`arb_alphanum_strings` + `property_test_entry_count_accurate`) instead of sharing a test-support module.
  - [medium/small] `MetaString`'s hand-written `Serialize`/`Deserialize` impl (custom `Visitor`) has no round-trip
    test; cross-variant `Hash` consistency and `PackedLengthCapacity` bit-packing lack any direct test.
  - [medium/small] `MetaString::from_interner`'s documented 3-step fallback (inline -> intern -> owned) has zero
    direct test coverage.
  - [medium/medium] `ottl/src/tests.rs` conflates four unrelated test concerns into one 2623-line file, with
    duplicated empty-map parser setup boilerplate across ~37 tests and 31 near-identical error-handling tests that
    are a table-driven consolidation candidate, not distinct coverage. The `Lexer` struct's `Iterator` impl is
    unreachable dead code with no path to test coverage.

- [ ] **G18 — saluki-app, saluki-config, saluki-env test cleanup** (`tobz/test-cleanup-app-config-env-test-cleanup`, `test(app)`)
  - [high/medium] `metrics/api.rs`'s override/reset worker is untested despite being a structural twin of the
    well-tested `logging/api.rs`.
  - [medium/small] `CgroupMemoryParser` and `MemoryBoundsConfiguration` parsing/validation logic has no unit tests.
  - [low/trivial] `LogLevel::TryFrom<String>`'s empty-string-error and valid-directive-parse branches are untested;
    `AgentLikeFormatter` (the default console/file log format) has no direct rendering test.
  - [high/small] The YAML config-file loading path is entirely untested, including JSON's error/null-drop siblings.
  - [medium/trivial] `diff_config` silently drops keys removed between config snapshots, with no test or doc
    coverage of that behavior.
  - [medium/small] `FieldUpdateWatcher::changed` only tests the happy path — disabled/`Lagged`/`Closed`/bad-deserialize
    branches are untested.
  - [medium/trivial] `space_separated.rs` deserializers have zero tests in their owning crate; one variant has zero
    callers anywhere.
  - [high/medium] Entity-alias resolution logic (`OriginResolver` + `TagStoreQuerier`) is untested because the only
    test module exercising it was commented out in May 2025 and never restored — restore it, don't rewrite it blind.
  - [medium/trivial] `extract_container_id`'s documented systemd `.mount` and `crio-conmon-` exclusion filters are
    untested.
  - [low/small] Multiple `ProtoConfig`-construction tests hand-build near-identical 17-field struct literals.
  - [low/trivial] `copy_test_file` duplicates `copy_test_file_as`'s body instead of delegating to it.

- [ ] **G19 — resource-accounting, process-memory, prometheus-exposition, saluki-metrics, data_model test cleanup**
  (`tobz/test-cleanup-resource-accounting-misc-crate-tests`, `test(resource-accounting)`)
  - [high/small] `resource-accounting`'s `calculate_backoff` has zero tests despite a fully worked numeric example
    in its own doc comment.
  - [high/medium] The core tracking mechanism (`Track` trait, `Tracked` future, group enter/exit, CPU attribution)
    is entirely untested; the `Track` trait's rustdoc examples are non-assertive.
  - [medium/small] `UsageExpr::product`/`sum` and the compound `BoundsBuilder` helpers are never exercised by any
    test.
  - [medium/medium] `process-memory`'s `resident_set_size` byte-conversion arithmetic and smaps/statm branches are
    untested against synthetic data, including the documented data-source fallback order and `None`-return path.
  - [medium/small] `prometheus-exposition`'s multi-label comma-joining and start-of-name character-validity
    branches are never exercised.
  - [low/trivial] A byte-identical "basic" smoke test is copy-pasted across platform-specific `Querier` modules.
  - [medium/small] `saluki-metrics`'s trybuild pass-fixtures only ever compile a bare info-level counter, leaving
    gauge/histogram/`trace_`/`debug_` arms and "no labels" untested — `static_metrics!`'s documented level-prefixed
    and gauge/histogram arms are only compiled via a non-asserting doctest, never a real test.
  - [low/trivial] KI-9: an assertion-free, print-only struct-size test exists in `data_model::event`.
  - [low/small] `EventType`/`PayloadType` bitmask `Display` formatting has zero test coverage.

- [ ] **G20 — config-overlay-model, commons, config-testing, metrics-v3, saluki-error/protos, correctness tooling test cleanup**
  (`tobz/test-cleanup-overlay-commons-correctness-tests`, `test(datadog-agent)`)
  - [high/medium] `SchemaOverlay`'s per-entry validation rules are precisely documented but almost entirely
    untested.
  - [medium/small] `SessionIdHandle`'s documented `wait_for_update()` race-avoidance idiom has zero test coverage.
  - [low/trivial] Duplicate test config-building helpers exist in `commons`' `ipc/config.rs`.
  - [low/small] Real dispatch/conversion logic in otherwise-thin utility crates has zero tests; `saluki-error`'s
    documented anyhow-forwarding contract (`ErrorContext`, `generic_error!`) is unverified by any test.
  - [low/small] `config-testing`'s `run_config_smoke_tests` documents three guarantees with no meta-test verifying
    any of them; its own crate-role rationale is a plain `//` comment, so it won't render as crate documentation.
  - [medium/medium] `stele`'s V3 metric wire-format decoder is untested for RATE/GAUGE types and FLOAT32/FLOAT64
    encodings — `try_from_v3`'s doc describes general columnar decoding, but tests cover only 2 of 16
    type-combination branches.
  - [medium/trivial] `MetricValue`'s documented ratio-tolerance equality contract has no direct boundary test.
  - [low/trivial] `panoramic`'s `parse_duration` documented no-unit-fallback and zero-duration-rejection branches
    are never hit by tests, and its per-variant dynamic-var placeholder resolution is documented per-method but has
    zero unit-level tests.

## Reviewed, no branch planned

A safety-net catch-all caught 42 clusters the backlog-organizing pass didn't explicitly assign to a group above.
Reviewing them by hand: the large majority are "what good looks like" positive-example citations (already folded
into `docs/development/testing-patterns.md` — e.g. `dsd_debug_log`, `ConfigurationLoader::for_tests`, the
`compare_points!` macro family, `cluster_agent.rs`'s DI seam) or near-duplicate restatements of a finding already
tracked in one of G1-G20 above (for example, a second, differently-worded copy of the `LogLevel::TryFrom` gap
already listed under G18). None of the 42 represent a distinct action item beyond what's already captured. If a
specific one looks like it might be a real gap on closer look, flag it and it'll get its own line above.
