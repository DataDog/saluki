# DogStatsD source simplification

This file tracks an effort to shrink the DogStatsD source, making it easier to understand and reducing the surface for
regressions. Each step in [Sequencing](#sequencing) lands as its own pull request; check it off here as part of that
pull request. **This file is deleted in the pull request that lands the last step.**

Line references are against `477274b83f`, before any step landed. After step 1, every `mod.rs` reference above line
2350 still holds, and the test references point into `dogstatsd/tests/` instead.

## Context

The DogStatsD source is about 12k lines:

- `lib/saluki-components/src/sources/dogstatsd/`: about 9.9k lines. `mod.rs` alone is 4,941 lines, about 2,350 of them
  production and 2,590 tests.
- `lib/saluki-io/src/deser/codec/dogstatsd/`: about 2.1k lines.

Roughly half of all of it is tests.

This evaluation comes from a read-only audit: 7 slice analyzers, one adversarial verifier per slice, and a completeness
critic. 68 findings survived and 3 were refuted. Line counts below are the verifiers' corrected net figures.

**Headline:** about **850–950 production lines** (~15%) and **~1,600 test lines** (~25%) can go without behavior
changes. Most of that comes from easy, local changes. The biggest understandability win costs no lines: split `mod.rs`
by concern. The cross-codebase "unify everything" wins are smaller than they look, because DogStatsD is the *only*
consumer of raw `Listener`/`Stream::receive`, `ElasticObjectPool` and the framing layer. A generic socket-pipeline
abstraction would have one caller.

## Sequencing

- [x] **1. Test move:** split the `mod.rs` tests into `dogstatsd/tests/{mod,config,decoder,readers,origin_capture,supervision}.rs`,
  with shared fixtures in `tests/mod.rs`. Pure move.
- [x] **2. Tier 1 test cleanups:** tables, counter-key helper, fixtures, `tempfile`, deleting duplicate coverage. Three
  pull requests: source tests, replay/forwarder tests, codec tests.
- [ ] **3. Tier 1 production cleanups:** `worker_with_shutdown`, `EventBufferManager`, `DecoderContext` embed,
  `EnablePayloads` fold, `origin_from_*` collapse, `listener_type()` reuse, dead code, replay `WriterState`,
  `file_header`.
- [ ] **4. Bug fixes 1, 2, 4,** each with a regression test (codec parity first).
- [ ] **5. Inline connected-stream reads** and the context-struct collapse, with an SMP benchmark comparison.
- [ ] **6. Capture/replay:** eager controls, marker-bit removal, typed errors, `EntityId` conversions, then relocation
  out of `sources/dogstatsd`.
- [ ] **7. Codec:** `ClientOrigin` and field helpers; tag-limit removal only if approved.
- [ ] **8. Split `mod.rs` production code** into `config`/`listener`/`decoder`/`packets`.

## Tier 1: easy (local, mechanical, low risk)

### Production (~340 lines)

| Change | Where | Existing primitive | Net prod |
|---|---|---|---|
| Replace hand-written `ListenerWorker` with `runtime::worker_with_shutdown(..).with_shutdown_timeout(Duration::MAX).needed()`. This is the same policy, and the file already uses that builder at 1297. | mod.rs:1177-1226 | `saluki-core/src/runtime/builder.rs:99-107,322-346` | ~45 |
| Replace `buffer_event`/`flush_events` with `EventBufferManager<DEFAULT_EVENTS_BUFFER_CAPACITY>`. Today's code is a line-for-line copy, which arrived with #2079. | mod.rs:1751-1770 | `saluki-core/src/topology/interconnect/event_buffer.rs:190-225` (used by `decoders/otlp`) | ~13 |
| Build `DecoderContext` once in `build()` and embed it in `DogStatsD`/`DogStatsDDecoder`. Today 9 fields are declared 3× and copied 2×. | mod.rs:901-983, 1060-1070, 1574-1601 | already nested in `ListenerContext`/`HandlerContext` | ~40 |
| Fold `EnablePayloadsFilter` into `EnablePayloadsConfiguration` (same 4 bools; `allow_event`/`allow_service_check` ignore their argument) | filters.rs:1-64, mod.rs:129-142, 815-819 | — | ~38 |
| Collapse the three byte-identical `origin_from_{metric,event,service_check}_packet` functions into one | origin.rs:279-328 | — | ~33 |
| `build_metrics`: replace the local `listener_type` match with `ListenAddress::listener_type()`. The rest of `metrics.rs` is deferred. | metrics.rs:302-308 | `saluki-io/src/net/addr.rs:91-99` | ~6 |
| Dead code and wrapper chain: no-op `receive_failed`/`continue`, the `process_datagram_decoder` wrapper, a bare block, `StreamCaptureState` (a newtype over `Option<i32>`), and a `?` in `handle_metric_packet` | mod.rs:1518-1572, 1787-1813, 1972-1987, 2176-2190 | — | ~35 |
| `dispatch_events`: one `send_split` helper for the duplicated events and service-check blocks | mod.rs:2266-2331 | `Dispatcher::buffered_named` | ~18 |
| `handle_frame` becomes a decoder method, dropping its 9 args and `allow(too_many_arguments)`. Share the event/service-check tag+origin prologue. | mod.rs:2049-2264 | — | ~20 |
| Drop the redundant non-Linux SO_REUSEPORT gate; `Listener` already does it | mod.rs:578-597 | `saluki-io/src/net/listener/mod.rs:188-193` | ~5 |
| Replay `WriterState`: `target_path`/`compressed` are write-only, `ongoing` mirrors the atomic, `accepting` == `traffic_tx.is_some()`. Collapse to `Option<SyncSender>`. `CaptureRecord.ancillary` is always empty. | replay/writer.rs:63-201, 322-328 | — | ~40 |
| `file_header.rs`: `write_all` instead of a Go-ported short-write check; slice compares; drop the double `valid_header` call | replay/file_header.rs:20-66, reader.rs:77-83 | std | ~15 |
| Remove the `PacketForwarderTarget` pass-through type | forwarder.rs:21-43, mod.rs:564-576 | — | ~10 |
| Codec: one `ascii_while1(pred)` helper for 4 copies of take_while1 + Antithesis assert + `from_utf8_unchecked`; inline the `decode_*` wrappers; map metric kind directly (drops an `unreachable!`) | codec helpers.rs:87-200, metric.rs:195-246, mod.rs:202-225 | `nom::combinator::value` | ~30 |
| `#[derive(Default)]` on `WellKnownTags` | tags.rs:44-49 | — | ~5 |

### Tests (~1,100 lines)

The project's preferred style is plain array-of-cases loops (`docs/development/testing-patterns.md`). `rstest` isn't in
the workspace, and that's fine.

| Change | Where | Net test |
|---|---|---|
| 14 `build_addresses_*` tests become one `assert_eq!` table. Delete `address_list_eq` and its meta-test: `ListenAddress: PartialEq` and the output order is fixed. | mod.rs:3247-3291, 3941-4188 | ~185 |
| Tables for the eol_required, statsd_forward, interner-size, origin-telemetry-error and named-pipe predicate groups | mod.rs:3293-3386, 3807-3939 | ~80 |
| A `source_counter(name, labels)` key helper replaces 11 hand-written ~11-line `recorder.counter((.., &[("component_id","dogstatsd_test"),..]))` blocks. Generalize the existing `processed_metric_key`. | mod.rs:2471-3023 | ~85 |
| Origin fixtures: `creds_peer(pid)`, `pid_container_resolver()`; the two `origin_telemetry_*` tests become one loop | mod.rs:3026-3106, 4208-4334 | ~70 |
| Three cardinality-precedence tests built from 6 full packet literals become one table against the collapsed origin fn | origin.rs:399-532 | ~100 |
| origin.rs fixtures: an `ENTITY_TAGS` table and an `enabled_resolver()` helper | origin.rs:350-397, 596-836 | ~40 |
| `ContextResolvers::for_tests(heap, origin)` replaces `manual`/`manual_with_origin` and 4 hand-built copies; add a `parse_metric()` helper | resolver.rs:85-105, mod.rs (5 sites) | ~25 |
| Reuse `replay/test_support.rs::wait_until_inactive` (mod.rs re-implements it 3×); add `read_capture_records()` | mod.rs:2537-2556, 2793-2809, 4274-4280 | ~20 |
| Replay tests: use `tempfile` instead of hand-rolled `unique_dir`/`DirGuard`. This also fixes a temp-dir leak in capture_api. Add one capture harness. | replay/test_support.rs, reader.rs:391-448, writer.rs:599-733 | ~95 |
| Writer tests: parse captures with `TrafficCaptureReader` instead of slicing bytes by hand, and drop the round-trip tests that overlap the reader's | writer.rs:620-782 | ~80 |
| Forwarder: merge the identical IPv4/IPv6 tests and add a `start_forwarder()` helper. One test re-implements `wait_for_forwarding`. | forwarder.rs:257-507 | ~70 |
| Codec: delete `check_basic_{metric,eventd,service_check}_eq`. They're `assert_eq!` with worse messages, because all three types derive `PartialEq`. | codec metric.rs:319-349, event.rs:227-239 | ~70 |
| Codec: table-drive basic/multivalue/unit and local_data/external_data/cardinality | codec metric.rs:351-629 | ~80 |
| Codec: collapse the 3-level event and service-check parse wrappers; add an `event_frame()` builder for `_e{len,len}:` | codec event.rs:188-361, service_check.rs:133-167 | ~55 |
| Delete tests that duplicate framework or codec coverage: `non_finite_metric_values_are_silently_dropped` (calls only the codec), 3 `ProcessIdentity` classification tests, 4 framer.rs tests that re-test `NewlineFramer`, plus the framework-guarantee supervision tests (shutdown-withheld halt, registry keeps listener bound, elastic pool growth). Keep one smoke test of each. | mod.rs:3347-3377, 3433-3461, 4190-4206, 4610-4735; framer.rs:60-152 | ~150–200 |
| Decoder test harness (`DecoderFixture` + `decode_once`); socket-reader recv/teardown helpers | mod.rs:2559-3024, 3464-3744 | ~75 |
| `EnablePayloads` / `WellKnownTags` test tables | filters.rs:67-156, tags.rs:90-185 | ~60 |

**Structure (0 net, the largest readability win):** move the 2,590 test lines out of `mod.rs` into
`dogstatsd/tests/`, with shared fixtures in `tests/mod.rs`. The precedent is `saluki-core/src/runtime/scope/tests.rs`
and `components/test_util/tests.rs`. Done in step 1.

## Tier 2: moderate (restructuring within DogStatsD, or a small shared helper)

| Change | Net | Notes |
|---|---|---|
| **Read connected streams inline.** Delete `BufferedStreamReader`, `receive_connected_stream`, the `oneshot` buffer-return and `ReceivedBuffer`'s `Drop`. The reader awaits `returned_buffer` (mod.rs:1496) before each read, so read and decode are already strictly serial; the second task only adds two channel hops per read. Pre-#2079 code was the same single `select!{ stream.receive, tick }` loop. | ~120 prod, ~35–70 test | The largest single production win. Preserve: memory-limiter wait before reads, buffer retain/collapse on partial frames, origin pinning per read, cancel-safety of `Stream::receive` (verified for TCP, UDS and named pipe). Benchmark connected-stream throughput (SMP) before and after. |
| Collapse the remaining context structs: one `Arc<ListenerState>` per listener, always built, which removes `Option<Arc<DatagramSocketContext>>` and its `.expect`. Keep two levels, per-listener and per-connection. | ~30 prod | Pairs with the inline-reader change. |
| `ListenerRequirement` (3 variants) becomes `require_listener: bool`. `All` is never used in production; run.rs only maps a bool to `AtLeastOne`/`Optional`. | ~35 prod, ~15 test | Small public API change in saluki-components. |
| Build capture writer and captured-tagger handle eagerly; delete the late-binding `Arc<Mutex<Option<..>>>` control layers (`DogStatsDCaptureControl`, the `TrafficCapture` forwarding facade, the replay control `Option`) | ~70 prod, ~30 test | The eager pattern already exists in `destinations/dsd_stats/mod.rs:285-300`. Touches run.rs wiring. |
| Remove the replay PID marker-bit round-trip (`mark_replay_process_id` → immediately decoded) and the unreachable `TagsResolver` origin wiring. DogStatsD always resolves via `resolve_origin_tags` with `ProcessOrigin`. | ~30 prod | Confirmed that no production caller passes an origin through `ContextResolvers::resolve`. |
| Codec: shared `ClientOrigin { local_data, external_data, cardinality }` sub-struct in all three packets, plus one `parse_common_field` for `#`/`c:`/`e:`/`card:` | ~50 prod, ~35 test | Packet field access changes (`packet.origin.local_data`). Hot path: keep `#[inline]` and benchmark metric parsing. |
| Codec: `fields()` iterator + `field(chunk, prefix, p)` helper for the triplicated extension loop and 21 `all_consuming(preceded(tag(P), p))` sites | ~25 prod | **Resolve the behavior drift first** (see [Bugs and hazards](#bugs-and-hazards-found-along-the-way)). |
| Typed `ReplayControlError` instead of substring-matching error text to choose 409 vs 412; share wire DTOs and route constants with the CLI client (`bin/agent-data-plane/src/cli/utils.rs:46-62`) | ~25 prod | Correctness improvement. |
| Move `EntityId` ↔ prefixed-string (`FromStr`) and ↔ proto conversions into `saluki-env/src/workload/entity.rs`. writer.rs re-declares all 10 private prefixes, and `datadog-agent/runtime` tagger has a third list. | ~40 prod | Cold path. Adding an `EntityId` variant would then touch one place instead of 3–4. |
| Remove the codec's `maximum_tag_count`/`maximum_tag_length` knobs and the per-tag `limit_str_to_len` in `RawTags`. They're never set outside two codec tests, and every other `RawTags` passes `usize::MAX`. | ~75 prod, ~28 test | Public API change in saluki-io and saluki-core; small hot-path win. Needs a maintainer decision. |

## Deferred: telemetry migration to `#[static_metrics]`

Out of scope for this effort; leave `metrics.rs` as it is for now (no getters-to-`pub`-fields change either). The
investigation is worth keeping for when it is picked up:

- Feasible for DogStatsD with identical names, labels and levels, and no hot-path cost, given four macro extensions: a
  verbatim per-field `name = ".."`, per-field static `labels(k = "v")`, a generated `from_builder(&MetricsBuilder, ..)`
  (component labels plus the conditional `origin=""` go on the builder), and a generated `noop()`. Optionally add
  `<field>_key()` for `delete_counter`.
- The same extensions unblock most of the ~25 hand-written telemetry structs elsewhere (OTLP, common/datadog,
  aggregate, ADP filters, saluki-io HTTP).
- `OriginMetricRecorder`'s bounded FIFO stays hand-written either way.
- Before migrating, add a characterization test that pins all 20 DogStatsD series (current tests cover ~8, and
  `TestRecorder` drops levels).

## Tier 3: high (refactor other code toward a unified approach, or deep moves)

| Change | Benefit | Notes |
|---|---|---|
| **Split `mod.rs` production code by concern:** `config.rs` (config, addresses, build, MemoryBounds), `listener.rs` (run, listener supervision, readers), `decoder.rs`, `packets.rs` (handle_frame, handle_*_packet, dispatch) | ~2,350 lines become 4 files of ~400–700 | A pure move. Do it after Tier 1/2 deletions, as one mechanical commit. |
| **Move capture/replay out of the source module** into `saluki-components/src/common/dogstatsd_capture/` (or its own crate). The source only needs `is_ongoing`/`enqueue`, `CaptureRecord`, `CapturedTaggerHandle` and `REPLAY_CREDENTIALS_GID`. reader.rs, `TimestampResolution` and `DEFAULT_REPLAY_LOOPS` serve only the CLI. | Removes about 2.6k lines from "the DogStatsD source", and the CLI stops importing a file reader from `sources::` | Capture glue that needs the private `ProcessOrigin` stays in the source. |
| A shared route-by-event-type dispatch in `saluki-core` (`dispatch_partitioned`) | ~25 more lines in DogStatsD | Other sources only partly benefit. Per-output buffers were judged a perf hazard, so don't do that variant. |
| Test-util helpers in saluki-core and saluki-metrics: `component_metric_key(ctx, name, labels)` (also used by forwarder, OTLP and the translator tests) and `NodeSnapshot::{find_descendant, running_descendants}` (7 copies of tree walkers across saluki-core and DogStatsD) | ~120 + ~50 test lines repo-wide | Small production API additions in saluki-core. |
| `WorkloadProvider` lookup-with-trace / first-non-empty helper, shared by the DogStatsD and OTLP origin resolvers | ~15 prod | Small; only worth doing alongside other origin work. |

## Bugs and hazards found along the way

Worth fixing regardless of the rest of the effort.

1. **Codec extension-loop drift:**
   - Event and service-check parsers `break` on any field shorter than 2 bytes (event.rs:81, service_check.rs:50).
     Metric breaks only on empty. So `_sc|svc|0|x|#env:prod` silently drops its tags.
   - Service checks have no empty-value guards, so `h:` fails the whole packet.

   Check Agent parity, then fix before unifying the loop.
2. **Replay API picks the HTTP status by `message.contains(..)`** (replay_api.rs:65-73). Rewording an error message
   turns a 409 into a 412.
3. **`OriginMetricRecorder` hand-rebuilds metric keys** for `delete_counter` (metrics.rs:133-158). If they drift from
   the registered labels, eviction silently fails and origin cardinality grows without bound. No test catches this,
   because `TestRecorder` never sees deletions. Deferred with the telemetry migration, which fixes it by construction;
   until then the keys match today.
4. **`write_header` uses `write` and fails on a short write** instead of retrying (file_header.rs:57-66).
5. A duplicate `.with_alert_type(..)` call in `handle_event_packet` (mod.rs:2217 and 2229) and in the codec test
   helper. Harmless.
6. `Listener` warns about SO_REUSEPORT for every UDP listener on non-Linux, even when autoscaling is off (saluki-io
   listener/mod.rs:190).
7. The `stream_capture_state_preserves_last_pid_without_new_creds` test is the **only** coverage of the "keep last
   PID" fallback. If `StreamCaptureState` is removed, port the case into a decoder test; don't just delete it.
8. The `static_metrics` trybuild suite (`lib/saluki-metrics/tests/macros.rs`) never runs under `make test` or `make
   test-all` (`--lib --bins` only). Unrelated to DogStatsD; noted for later.

## Explicitly not worth doing (verified)

- A generic listener → stream → framer → decoder → dispatch pipeline in saluki-io: DogStatsD is the only consumer.
- Sharing listener-requirement or address building with OTLP and checks_ipc: they bind a single endpoint through
  `HttpServer`.
- Merging the connectionless decoder pool into per-socket decoding (#2079/#3008 burst regressions), or moving origin
  pinning to decode time (PID-reuse correctness).
- Replacing `OriginMetricRecorder`'s FIFO with `MappedMetric`, `Cache`, or a bounded mapped variant in the macro.
  There's no cap or eviction callback, it allocates per call, and there would be only one user.
- Embedding the ADP config domain in `DogStatsDConfiguration` (crate layering), replacing `DsdFramer` with a trait
  object, merging the event and service-check parsers wholesale, or dropping nom.
- Proptest for the config helpers (the oracle would re-implement them), or adding rstest.

## Verification (per pull request)

- `cargo check --workspace && cargo check --workspace --tests`; `cargo nextest run -p saluki-components -p saluki-io`;
  `make fmt`; then `make check-all` (Vale lints Rust doc comments) and `make test-all`.
- Windows cross-check (`--target x86_64-pc-windows-gnu --profile test`) for anything touching named-pipe or cfg-gated
  code.
- Test-only pull requests: confirm the test count for each behavior is preserved, and that coverage of deleted tests
  exists at the cited saluki-io or saluki-core location.
- Runtime changes (inline reader, decoder context): `/panoramic` correctness and integration suites (the `dogstatsd-*`
  cases), plus SMP experiments for UDS stream and TCP throughput and datagram burst drops.
- Codec changes: the existing metric proptest, the codec fuzz target, and a parsing microbenchmark (override
  `_RJEM_MALLOC_CONF` for local allocator A/B runs).
