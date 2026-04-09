# Trace Agent Migration Plan: Datadog Agent to Saluki/Agent Data Plane

## Executive Summary

This document outlines a phased approach to migrating the Datadog Agent's Trace Agent component from Go to Rust, leveraging the Saluki framework and Agent Data Plane (ADP) architecture. The migration will be broken into incremental PRs, starting with simpler endpoints and progressively building towards full feature parity with the existing trace agent.

**Primary Goals:**
- Maintain 100% backward compatibility with existing trace ingestion APIs
- Achieve performance improvements through Rust's memory safety and efficiency
- Leverage existing Saluki components where possible
- Enable gradual rollout with minimal risk
- Maintain comprehensive test coverage throughout

**Key Finding:**
After analyzing the codebase, proxy endpoints (EVP, telemetry, DogStatsD) are primarily pass-through with header manipulation and validation - significantly simpler than initially assessed. These will be prioritized in Phase 2 to validate infrastructure early with lower-complexity endpoints.

## Current State Analysis

### Datadog Agent Trace Component

**Architecture:**
- **Language:** Go with goroutine-based concurrency
- **Entry Point:** `cmd/trace-agent/` with Uber FX dependency injection
- **Core Components:**
  - HTTP API receiver (`pkg/trace/api/`) with 20+ endpoint versions
  - 5 sampling strategies (Priority, Errors, Rare, NoPriority, Probabilistic)
  - Stats concentrator with time-bucketed aggregation
  - Obfuscation, filtering, and transformation pipeline
  - Writer components for traces and stats

**API Endpoints (Complexity Assessment - Revised):**
| Category | Endpoints | Complexity | Priority |
|----------|-----------|------------|----------|
| **Simple (Pass-through)** | `/tracer_flare/v1`, `/evp_proxy/v1-v4`, `/telemetry/proxy/`, `/dogstatsd/v1-v2/proxy` | Low | Phase 2 |
| **Medium (Trace Formats)** | `/v0.3-v0.5/traces`, `/v0.7/traces`, legacy `/spans`, `/services` | Medium | Phase 3 |
| **High (Core Processing)** | `/v1.0/traces`, `/v0.6/stats` | High | Phase 4-5 |
| **Medium (Specialized)** | `/profiling/v1/input`, `/openlineage/`, `/debugger/*`, `/symdb/v1/input` | Medium | Phase 6 |

**Note:** EVP proxy and other proxy endpoints (telemetry, dogstatsd) are primarily pass-through with header manipulation and validation - simpler than initially assessed.

### Saluki/Agent Data Plane Capabilities

**Ready to Use:**
- ✅ OTLP source & trace decoder (gRPC + HTTP)
- ✅ `apm_stats` transform (time-bucketed aggregation)
- ✅ `trace_sampler` (probabilistic, basic priority, basic errors)
- ✅ `trace_obfuscation` (sensitive data masking)
- ✅ Datadog encoder/forwarder (HTTP API client)
- ✅ Configuration system with environment overrides
- ✅ Health & telemetry framework
- ✅ Memory accounting per component
- ✅ Integration test infrastructure (Panoramic)

**Available from libdatadog (requires integration):**
- 🔄 **libdd-trace-obfuscation** - SQL, HTTP, Redis, Memcached, credit card detection, regex tag replacement
- 🔄 **libdd-trace-normalization** - Service/operation/resource name normalization, tag validation, sampling priority extraction
- 🔄 **libdd-trace-stats** - SpanConcentrator with time-bucketed aggregation, peer tag support, configurable span kinds
- 🔄 **libdd-trace-utils** - v0.4/v0.5 MessagePack decoders, HTTP transport with retry logic
- 🔄 **libdd-data-pipeline** - TraceExporter/StatsExporter with compression and endpoint configuration

**Gaps Requiring Implementation:**
- ❌ Proprietary trace format decoders (v0.1-v0.3, v0.7, v1.0) - v0.4/v0.5 exist in libdatadog
- ❌ Multi-version API endpoint routing (HTTP server infrastructure)
- ❌ Advanced sampling (Rare, NoPriority, signature-based catalog)
- ❌ Full tag enrichment pipeline (container auto-discovery - libdatadog has header-based tags only)
- ❌ Event processing for APM events
- ❌ Legacy endpoints support (proxy endpoints, tracer flare, etc.)
- ❌ Stats endpoint ingestion (`/v0.6/stats` receiver)

## Migration Goals

### Functional Requirements
1. **API Compatibility:** Support all existing trace API endpoints
2. **Sampling Parity:** Implement all 5 sampling strategies
3. **Stats Accuracy:** Match existing APM stats aggregation
4. **Format Support:** Handle all trace format versions (v0.1-v1.0)
5. **Configuration:** Maintain existing configuration interface

### Non-Functional Requirements
1. **Performance:** Improve throughput and reduce latency
2. **Memory:** Predictable memory usage without GC pauses
3. **Observability:** Comprehensive telemetry and health checks
4. **Testing:** Integration and regression test coverage
5. **Safety:** Leverage Rust's memory safety guarantees

## Phased Implementation Plan

### Phase 0: Foundation (PR #1-2)

**Goal:** Establish core infrastructure for trace ingestion

#### PR #1: Trace API Infrastructure
**Description:** Create the HTTP API layer for trace ingestion with version routing

**Scope:**
- Create `lib/saluki-components/src/sources/trace_api/` module
- Implement Axum-based HTTP server with version routing
- Add endpoint registration system similar to `AttachEndpoint()`
- Implement request buffering and size limiting
- Add semaphore-based concurrency control
- Create basic health check endpoint

**Files to Create:**
```
lib/saluki-components/src/sources/trace_api/
├── mod.rs                  # Main source component
├── server.rs               # Axum HTTP server setup
├── endpoints.rs            # Endpoint registry
├── middleware.rs           # Request validation, sizing, buffering
└── config.rs               # TraceAPIConfiguration
```

**Testing:**
- Unit tests for endpoint registration
- Integration test: HTTP server starts and responds to health checks
- Load test: Concurrent request handling with backpressure

**Success Criteria:**
- HTTP server accepts connections on configurable port
- Version routing works (parses `/vX.Y/endpoint`)
- Request size limiting enforced
- Concurrent request limit respected

---

#### PR #2: Tracer Flare Endpoint (Simplest Endpoint)
**Description:** Implement the simplest endpoint as a proof-of-concept

**Scope:**
- Implement `/tracer_flare/v1` endpoint handler
- Create decoder for flare data format
- Forward to existing Datadog forwarder
- Add integration test

**Rationale:** This endpoint has:
- Simple format (compressed JSON payload)
- No sampling or processing required
- Straightforward pass-through to backend
- Low risk, validates infrastructure

**Files to Modify:**
```
lib/saluki-components/src/sources/trace_api/
└── handlers/
    ├── mod.rs
    └── tracer_flare.rs
```

**Testing:**
- Integration test: Send flare data, verify accepted
- Round-trip test: Decode → encode → verify

**Success Criteria:**
- `/tracer_flare/v1` accepts POST requests
- Flare data forwarded to Datadog API
- Returns appropriate HTTP status codes

---

### Phase 1: OTLP Foundation (PR #3-4)

**Goal:** Leverage existing OTLP support as primary ingestion path

#### PR #3: OTLP Trace Ingestion Integration
**Description:** Integrate existing OTLP components into trace topology

**Scope:**
- Configure OTLP source for trace ingestion
- Wire OTLP decoder → trace_sampler → apm_stats → encoder → forwarder
- Add container tag enrichment
- Enable basic obfuscation

**libdatadog Integration Note:**
- Consider using `libdd-trace-obfuscation` for obfuscation (requires Saluki Span ↔ pb::Span conversion)
- Consider using `libdd-trace-normalization` for span normalization (requires conversion)
- See Open Questions section for integration strategy decisions

**Topology:**
```
OtlpSource
    ↓
OtlpTraceDecoder
    ↓
[Transforms]
├── TraceSampler (probabilistic)
├── TraceObfuscation (Saluki native OR libdd-trace-obfuscation*)
├── HostTags
├── ApmStatsTransform (Saluki native OR libdd-trace-stats*)
    ↓
[Encoders]
├── DatadogTraceEncoder
└── DatadogApmStatsEncoder
    ↓
[Forwarders]
├── DatadogTracesForwarder (/v1.0/traces)
└── DatadogStatsForwarder (/v0.6/stats)

* libdatadog integration decision pending (see Open Questions)
```

**Configuration:**
```yaml
data_plane:
  otlp:
    grpc_port: 4317
    http_port: 4318
    probabilistic_sampling: 100.0  # Start with 100% sampling
```

**Testing:**
- Panoramic integration test: Send OTLP traces → verify received
- Stats accuracy test: Compare stats output with Go agent
- Sampling test: Verify probabilistic sampling rate
- SMP regression: Baseline throughput/memory

**Success Criteria:**
- OTLP gRPC + HTTP endpoints functional
- Traces forwarded to `/v1.0/traces`
- Stats forwarded to `/v0.6/stats`
- No memory leaks under sustained load

---

#### PR #4: Enhanced Trace Sampling
**Description:** Extend `trace_sampler` component with priority and error sampling

**Scope:**
- Enhance `PrioritySampler` to respect `_dd.p.sm` user priority
- Improve `ErrorsSampler` with configurable rates
- Add sampling metrics export
- Implement sampling coordinator for TPS targeting

**Files to Modify:**
```
lib/saluki-components/src/transforms/trace_sampler/
├── mod.rs                  # Component entry
├── priority.rs             # Enhanced priority sampling
├── errors.rs               # Enhanced error sampling
├── coordinator.rs          # TPS coordination (NEW)
└── metrics.rs              # Sampling metrics (NEW)
```

**Configuration:**
```yaml
transforms:
  trace_sampler:
    default_sample_rate: 1.0
    max_tps: 10.0
    errors_sample_rate: 1.0
    priority_sampling: true
```

**Testing:**
- Unit tests: Priority sampling respects user decisions
- Unit tests: Error sampling increases error trace retention
- Integration test: TPS targeting converges to configured limit
- Correctness test: Compare sampling decisions with Go agent

**Success Criteria:**
- Priority sampling matches Go agent behavior
- Error traces sampled at higher rate
- Sampling metrics exported for telemetry

---

### Phase 2: Simple Proxy Endpoints (PR #5-8)

**Goal:** Implement pass-through proxy endpoints with minimal processing

**Rationale:** These endpoints are primarily reverse proxies with header manipulation and validation - simpler than trace processing endpoints. Implementing them early validates the HTTP API infrastructure with lower complexity.

#### PR #5: EVP Proxy Infrastructure
**Description:** Create reusable proxy infrastructure for pass-through endpoints

**Scope:**
- Create `lib/saluki-components/src/sources/trace_api/proxy/` module
- Implement generic reverse proxy handler
- Add header filtering and enrichment framework
- Implement subdomain/path/query validation
- Add container ID extraction and tag enrichment
- Multi-endpoint fanout support

**Files to Create:**
```
lib/saluki-components/src/sources/trace_api/proxy/
├── mod.rs                  # Proxy infrastructure
├── reverse_proxy.rs        # Generic reverse proxy handler
├── headers.rs              # Header filtering/enrichment
├── validation.rs           # Input validation (security)
├── container_tags.rs       # Container metadata
└── config.rs               # Proxy configuration
```

**Key Features:**
- Header allowlist filtering
- Datadog header injection (hostname, container ID, API key)
- Multi-endpoint fanout (return first response, discard others)
- Request size limiting
- Custom timeout support per endpoint

**Testing:**
- Unit test: Header filtering works correctly
- Unit test: Subdomain validation rejects invalid input
- Integration test: Request forwarded to backend
- Multi-endpoint test: Fanout to multiple backends

**Success Criteria:**
- Generic proxy infrastructure reusable for all proxy endpoints
- Security validation prevents abuse
- Multi-endpoint fanout functional

---

#### PR #6: EVP Proxy Endpoints (`/evp_proxy/v1-v4`)
**Description:** Implement Event Platform proxy endpoints

**Scope:**
- Register `/evp_proxy/v1/`, `/v2/`, `/v3/`, `/v4/` endpoints
- Wire to proxy infrastructure
- Add EVP-specific header handling (`X-Datadog-EVP-Subdomain`, `X-Datadog-NeedsAppKey`)
- Configure custom timeouts per version

**Handler Pattern:**
```rust
async fn handle_evp_proxy(
    State(state): State<TraceAPIState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ProxyError> {
    // Validate subdomain/path
    // Extract container ID
    // Forward to backend with enriched headers
    // Return response
}
```

**Files to Create:**
```
lib/saluki-components/src/sources/trace_api/handlers/
└── evp_proxy.rs            # EVP proxy handler
```

**Configuration:**
```yaml
trace_api:
  evp_proxy:
    enabled: true
    dd_url: "datadoghq.com"
    api_key: "${DD_API_KEY}"
    application_key: "${DD_APP_KEY}"  # Optional
    max_payload_size: 5242880          # 5MB
    timeout: 30s
    additional_endpoints: {}           # Map of host -> api_keys
```

**Testing:**
- Integration test: EVP request forwarded to backend
- Security test: Invalid subdomain rejected
- Header test: Correct headers added
- Multi-version test: All v1-v4 work
- AppKey test: ApplicationKey added when requested

**Success Criteria:**
- All EVP versions (v1-v4) functional
- Security validation prevents malicious use
- Headers correctly enriched
- Multi-endpoint fanout works

---

#### PR #7: Telemetry & DogStatsD Proxy Endpoints
**Description:** Implement telemetry and DogStatsD proxy endpoints

**Scope:**
- Implement `/telemetry/proxy/` endpoint (telemetry forwarding)
- Implement `/dogstatsd/v1/proxy` and `/v2/proxy` endpoints
- Reuse proxy infrastructure from PR #5
- Add endpoint-specific validation

**Endpoints:**
- `/telemetry/proxy/` - Forward telemetry data to backend
- `/dogstatsd/v1/proxy` - Forward DogStatsD metrics (v1)
- `/dogstatsd/v2/proxy` - Forward DogStatsD metrics (v2)

**Files to Create:**
```
lib/saluki-components/src/sources/trace_api/handlers/
├── telemetry_proxy.rs      # Telemetry proxy
└── dogstatsd_proxy.rs      # DogStatsD proxy
```

**Testing:**
- Integration test: Telemetry data forwarded
- Integration test: DogStatsD metrics forwarded
- Version test: v1 and v2 both work

**Success Criteria:**
- Telemetry proxy functional
- DogStatsD v1/v2 proxy functional
- Reuses generic proxy infrastructure

---

#### PR #8: Pipeline Stats Endpoint (`/v0.1/pipeline_stats`)
**Description:** Implement pipeline statistics endpoint

**Scope:**
- Implement `/v0.1/pipeline_stats` endpoint
- Forward pipeline stats to backend
- Add basic validation

**Rationale:** Relatively simple pass-through endpoint

**Testing:**
- Integration test: Pipeline stats forwarded

**Success Criteria:**
- Pipeline stats endpoint functional

---

### Phase 3: Legacy Trace Formats (PR #9-11)

**Goal:** Support v0.3-v0.5 trace formats and legacy endpoints

#### PR #9: Trace Format Decoders (v0.3-v0.5)
**Description:** Implement decoders for v0.3, v0.4, and v0.5 trace formats

**Scope:**
- Create `lib/saluki-components/src/decoders/datadog_traces/` module
- ✅ **Leverage libdd-trace-utils for v0.4/v0.5** (decoders already exist!)
- Implement custom v0.3 decoder (not in libdatadog)
- Add format validation and error handling
- Convert to Saluki's internal `Trace` representation

**libdatadog Integration:**
- `libdd-trace-utils::msgpack_decoder::v04` - v0.4 decoder (production-ready)
- `libdd-trace-utils::msgpack_decoder::v05` - v0.5 decoder with string dictionary (production-ready)
- Both decode to `Vec<pb::Span>` which needs conversion to Saluki `Span` structs
- See Open Questions for conversion strategy

**Format Differences:**
```
v0.3: Basic MsgPack spans array (custom decoder needed)
v0.4: Added metrics, improved performance (USE libdatadog)
v0.5: Optimized for throughput, smaller payload, string dictionary (USE libdatadog)
```

**Files to Create:**
```
lib/saluki-components/src/decoders/datadog_traces/
├── mod.rs                  # Decoder registry
├── v0_3.rs                 # v0.3 decoder (custom)
├── libdatadog_adapter.rs   # pb::Span → Saluki Span conversion (NEW)
├── common.rs               # Shared utilities
└── tests.rs                # Round-trip tests
```

**Testing:**
- Unit tests: Decode sample payloads from each version
- Round-trip tests: Decode → encode → verify
- Fuzz tests: Random payload fuzzing
- Correctness test: Compare decoded traces with Go agent

**Success Criteria:**
- All three formats decode without errors
- Decoded traces match Go agent output
- Invalid payloads rejected gracefully

---

#### PR #10: v0.3-v0.5 API Endpoints
**Description:** Wire up `/v0.3/traces`, `/v0.4/traces`, `/v0.5/traces` endpoints

**Scope:**
- Register endpoints in trace API server
- Route to appropriate decoder based on version
- Add version-specific response headers
- Implement `/services` companion endpoints
- Wire decoded traces to processing topology

**Handler Pattern:**
```rust
async fn handle_v0_3_traces(
    State(state): State<TraceAPIState>,
    body: Bytes,
) -> Result<Response, TraceAPIError> {
    // Decode v0.3 format
    // Send to topology input
    // Return appropriate response
}
```

**Files to Create:**
```
lib/saluki-components/src/sources/trace_api/handlers/
├── v0_3.rs                 # v0.3 handler
├── v0_4.rs                 # v0.4 handler
└── v0_5.rs                 # v0.5 handler
```

**Testing:**
- Integration test: Send v0.3 traces → verify processed
- Integration test: Send v0.4 traces → verify processed
- Integration test: Send v0.5 traces → verify processed
- Multi-version test: Interleave requests from all versions
- Sampling test: Verify sampling applies to legacy formats

**Success Criteria:**
- All three endpoints accept traces
- Version-specific headers returned
- No cross-version interference
- Traces flow through sampling and stats pipeline

---

#### PR #11: Legacy Endpoints (v0.1, v0.2)
**Description:** Implement oldest trace formats (marked as hidden in Go agent)

**Scope:**
- Implement v0.1 and v0.2 decoders
- Add endpoints with deprecation warnings
- Log usage metrics for observability

**Rationale:** These are legacy but may still be used by old tracers

**Testing:**
- Integration test: v0.1/v0.2 traces accepted
- Deprecation warning logged
- Usage metrics recorded

**Success Criteria:**
- Legacy endpoints functional
- Clear deprecation signaling

---

### Phase 4: Advanced Trace Formats (PR #12-14)

**Goal:** Implement v0.7 and v1.0 trace formats (current production formats)

#### PR #12: v0.7 Trace Format
**Description:** Implement latest proprietary MsgPack format (v0.7)

**Scope:**
- Implement v0.7 MsgPack decoder
- Add `/v0.7/traces` endpoint
- Handle v0.7-specific features (if any)

**Files to Create:**
```
lib/saluki-components/src/decoders/datadog_traces/
└── v0_7.rs                 # v0.7 decoder
```

**Testing:**
- Unit tests: Decode v0.7 payloads
- Correctness test: Compare with Go agent v0.7 handling
- Performance test: Benchmark v0.7 decoding speed

**Success Criteria:**
- v0.7 format fully supported
- No regression in earlier versions
- Performance acceptable

---

#### PR #13: v1.0 Trace Format & Endpoint
**Description:** Implement current production format (highest priority)

**Scope:**
- Implement v1.0 decoder with Protobuf support
- Add `/v1.0/traces` endpoint
- Implement v1-specific processing (payload metadata)
- Handle agent payload metadata (version, APM mode flags)

**Files to Create:**
```
lib/saluki-components/src/decoders/datadog_traces/
└── v1_0.rs                 # v1.0 decoder with Protobuf
```

**Testing:**
- Integration test: v1.0 traces end-to-end
- Metadata test: Verify agent metadata attached
- Correctness test: Compare with Go agent v1.0
- SMP regression: v1.0 performance benchmarks

**Success Criteria:**
- v1.0 format production-ready
- Metadata correctly attached
- Performance meets SMP targets

---

#### PR #14: OpenLineage Endpoint (`/openlineage/api/v1/lineage`)
**Description:** Implement OpenLineage metadata endpoint

**Scope:**
- Implement `/openlineage/api/v1/lineage` endpoint
- Decode OpenLineage format
- Forward to backend or process as metadata

**Rationale:** Relatively simple, can be tackled after main trace formats

**Testing:**
- Integration test: OpenLineage data accepted and forwarded

**Success Criteria:**
- OpenLineage endpoint functional

---

### Phase 5: Stats Endpoint (PR #15)

**Goal:** Implement client-side stats ingestion (high complexity)

#### PR #15: Stats Endpoint (`/v0.6/stats`)
**Description:** Implement client-side stats ingestion

**Scope:**
- Implement v0.6 stats payload decoder
- Add `/v0.6/stats` endpoint
- Create `ClientStatsAggregator` component
- Wire stats to aggregation (consider libdd-trace-stats::SpanConcentrator)
- Forward to `/api/v0.2/stats` backend

**libdatadog Integration:**
- `libdd-trace-stats::SpanConcentrator` provides production-tested aggregation logic
- Time-bucketed aggregation with configurable bucket size
- Multi-level aggregation keys (service/operation/resource/type/http_status/peer_tags)
- Top-level/measured span eligibility built-in
- See Open Questions for whether to use libdatadog or Saluki's apm_stats

**Rationale:** High complexity due to stats aggregation logic, weight calculation, and buffering requirements

**Component:**
```rust
ClientStatsAggregator {
    // Accepts client-provided stats
    // Adjusts weights
    // Buffers for concentration (potentially using libdd-trace-stats)
    // Forwards to backend (potentially using libdd-data-pipeline::StatsExporter)
}
```

**Files to Create:**
```
lib/saluki-components/src/transforms/client_stats_aggregator/
├── mod.rs                  # Component
├── decoder.rs              # v0.6 stats format
├── weight.rs               # Weight adjustment
└── tests.rs
```

**Testing:**
- Integration test: Send client stats → verify aggregated
- Correctness test: Compare stats accuracy with Go agent
- Weight test: Verify weight calculation matches

**Success Criteria:**
- Stats endpoint accepts v0.6 format
- Client stats correctly aggregated
- Forwarded to backend API

---

### Phase 6: Advanced Sampling (PR #16-17)

**Goal:** Implement rare and nopriority sampling strategies

#### PR #16: Rare Sampler (Outlier Detection)
**Description:** Implement rare trace sampling based on signature catalog

**Scope:**
- Create signature-based catalog (hash of service/name/resource)
- Implement outlier detection algorithm
- Add rare sampling configuration
- Integrate into `trace_sampler` component

**Algorithm:**
```
1. Compute trace signature hash
2. Track signature frequency over time window
3. Sample traces with rare signatures at higher rate
4. Age out old signatures periodically
```

**Files to Create:**
```
lib/saluki-components/src/transforms/trace_sampler/
├── rare.rs                 # RareSampler
├── catalog.rs              # SignatureCatalog
└── signature.rs            # Hash computation
```

**Configuration:**
```yaml
transforms:
  trace_sampler:
    rare_sampler:
      enabled: true
      rare_sample_rate: 1.0
      catalog_size: 10000
```

**Testing:**
- Unit test: Signature computation deterministic
- Unit test: Rare traces sampled at higher rate
- Integration test: Catalog ages out old entries
- Correctness test: Compare with Go agent rare sampling

**Success Criteria:**
- Rare traces identified and sampled
- Catalog size bounded
- No memory leaks from catalog

---

#### PR #17: NoPriority Sampler (Fallback)
**Description:** Implement fallback sampling for traces without priority

**Scope:**
- Add `NoPrioritySampler` for traces without `_dd.p.sm`
- Integrate into sampling coordinator
- Add configuration

**Testing:**
- Unit test: Traces without priority use fallback sampler
- Integration test: Mix of priority/nopriority traces

**Success Criteria:**
- All traces get sampling decision
- No traces dropped due to missing priority

---

### Phase 7: Tag Enrichment (PR #18-19)

**Goal:** Full tag enrichment pipeline

#### PR #18: Container Tag Enrichment
**Description:** Add container metadata to traces

**Scope:**
- Implement container tags buffer (query Docker/K8s API)
- Add `ContainerTagEnricher` transform
- Cache container metadata
- Enrich spans with container ID, pod UID, etc.

**Files to Create:**
```
lib/saluki-components/src/transforms/container_tag_enricher/
├── mod.rs                  # Component
├── buffer.rs               # Metadata buffer/cache
├── docker.rs               # Docker API client
└── kubernetes.rs           # K8s API client
```

**Testing:**
- Integration test (Docker): Verify container tags added
- Integration test (K8s): Verify pod tags added
- Cache test: Verify caching reduces API calls

**Success Criteria:**
- Container tags correctly added
- Minimal performance impact
- Cache hit rate > 90%

---

#### PR #19: Peer Tag & Service Mapping
**Description:** Add peer service tagging and service mapping

**Scope:**
- Implement peer tag detection (span relationships)
- Add service name mapping
- Add environment tag injection

**Testing:**
- Unit test: Peer tags correctly identified
- Integration test: Service mapping applied

**Success Criteria:**
- Peer tags match Go agent
- Service mapping functional

---

### Phase 8: Specialized Endpoints (PR #20-21)

**Goal:** Implement remaining specialized endpoints

#### PR #20: Profiling & Debugger Endpoints
**Description:** Add profiling and debugger data ingestion endpoints

**Scope:**
- Implement `/profiling/v1/input` endpoint (profiling data)
- Implement `/debugger/v1/input` and `/v2/input` endpoints
- Implement `/debugger/v1/diagnostics` endpoint
- Implement `/symdb/v1/input` endpoint (symbol database)
- Add custom timeout handling where needed
- Forward to respective backends

**Endpoints:**
- `/profiling/v1/input` - Profiling data (may be out of scope)
- `/debugger/v1/input`, `/v2/input` - Debugger data ingestion
- `/debugger/v1/diagnostics` - Debugger diagnostics
- `/symdb/v1/input` - Symbol database

**Note:** Profiling may be handled separately from trace agent

**Testing:**
- Integration test per endpoint
- Timeout handling test

**Success Criteria:**
- All endpoints functional
- Data correctly forwarded

---

#### PR #21: Event Processing (Optional)
**Description:** Implement APM event processing if required

**Scope:**
- Create event extraction transform
- Process trace events for alerting
- Forward events to backend

**Note:** May not be required if events handled differently

**Testing:**
- Event extraction test
- Integration test: Events forwarded

**Success Criteria:**
- Event processing functional (if needed)

---

### Phase 9: Production Readiness (PR #22-24)

**Goal:** Production-ready with full observability

#### PR #22: Comprehensive Observability
**Description:** Add full telemetry, metrics, and logging

**Scope:**
- Component-level metrics (throughput, latency, errors)
- Trace ingestion metrics
- Sampling decision metrics
- Stats aggregation metrics
- Memory accounting dashboards
- Error logging with structured context

**Testing:**
- Metrics test: All metrics exported
- Dashboard test: Grafana dashboard renders

**Success Criteria:**
- Full observability parity with Go agent
- Prometheus metrics exported
- Structured logs

---

#### PR #23: Configuration Migration
**Description:** Ensure configuration compatibility

**Scope:**
- Document configuration mapping (Go → Rust)
- Add configuration validation
- Support existing trace agent config format
- Migration guide

**Testing:**
- Config test: All Go agent configs map to Rust
- Validation test: Invalid configs rejected

**Success Criteria:**
- Drop-in configuration compatibility
- Clear migration documentation

---

#### PR #24: Performance Optimization & Tuning
**Description:** Final performance tuning

**Scope:**
- Profile hot paths
- Optimize allocations
- Tune buffer sizes
- Optimize decoder performance
- Memory pool tuning

**Testing:**
- SMP regression: Compare with Go agent
- Load test: Sustained high throughput
- Memory test: No leaks under load

**Success Criteria:**
- Performance >= Go agent
- Memory usage < Go agent
- No performance regressions

---

## Testing Strategy

### Unit Testing
**Scope:** Individual component logic
- Decoder round-trip tests
- Sampling decision tests
- Stats aggregation tests
- Configuration parsing tests

**Tools:** Rust `cargo test`, property-based testing with `proptest`

---

### Integration Testing
**Scope:** Component interaction and end-to-end flows
- Panoramic test framework (existing in Saluki)
- Docker-based test scenarios
- Multi-version endpoint tests
- Concurrent load tests

**Test Scenarios:**
```yaml
# Example Panoramic test
test_trace_ingestion_v1_0:
  services:
    - agent-data-plane:
        config: trace_agent.yaml
  steps:
    - send_traces:
        endpoint: http://localhost:8126/v1.0/traces
        payload: fixtures/trace_v1_0.msgpack
    - assert:
        - port_listening: 8126
        - log_contains: "trace received"
        - http_endpoint:
            url: http://localhost:5100/health
            status: 200
```

---

### Correctness Testing
**Scope:** Validate output matches Go agent exactly

**Approach:**
1. Run same trace payloads through Go agent and Rust agent
2. Compare decoded traces (field-by-field)
3. Compare sampling decisions
4. Compare aggregated stats
5. Compare forwarded payloads

**Files:**
```
test/correctness/trace_agent/
├── traces/                 # Sample trace payloads
├── expected_outputs/       # Go agent outputs
├── run_go_agent.sh         # Run Go agent
├── run_rust_agent.sh       # Run Rust agent
└── compare.py              # Comparison script
```

---

### SMP Regression Testing
**Scope:** Performance benchmarks and memory profiling

**Metrics:**
- Throughput (traces/sec)
- Latency (p50, p90, p99)
- Memory usage (RSS, heap)
- CPU usage

**Scenarios:**
- Baseline: Idle agent
- Low load: 100 traces/sec
- Medium load: 1000 traces/sec
- High load: 10000 traces/sec
- Burst load: Spike to 50000 traces/sec

**Comparison:** Go agent vs Rust agent side-by-side

---

### Load Testing
**Scope:** Sustained high throughput

**Tools:** `k6` or custom trace generator

**Scenarios:**
- Sustained 10k traces/sec for 1 hour
- Gradual ramp: 0 → 20k traces/sec over 10 minutes
- Spike: 0 → 50k → 0 in 1 minute
- Mixed versions: All endpoint versions concurrently

---

## Dependencies and Risks

### External Dependencies
- **OTLP Protocol:** Depends on `opentelemetry-proto` crate
- **MsgPack:** Depends on `rmp-serde` for decoding
- **Protobuf:** Depends on `prost` for v1.0 format
- **HTTP Server:** Axum framework
- **TLS:** `rustls` for HTTPS

### Internal Dependencies
- **Saluki Core:** Event model (custom Rust structs, NOT protobuf-based), component traits
- **Saluki Components:** Existing transforms (apm_stats, trace_sampler, etc.)
- **Agent Data Plane:** Configuration, control plane, APIs

### libdatadog Dependencies (Optional Integration)
- **libdd-trace-obfuscation** (v1.0.0) - SQL/HTTP/Redis obfuscation, credit card detection
- **libdd-trace-normalization** (v1.0.0) - Span/tag normalization, sampling priority
- **libdd-trace-stats** (v1.0.0) - SpanConcentrator for stats aggregation
- **libdd-trace-utils** (v1.0.0) - v0.4/v0.5 decoders, HTTP transport with retry
- **libdd-trace-protobuf** (v1.0.0) - Protobuf message definitions
- **libdd-data-pipeline** (v1.0.0) - TraceExporter/StatsExporter

**Note:** libdatadog operates on `pb::Span` (protobuf), while Saluki uses custom Rust structs for performance. Integration requires conversion at boundaries (see Open Questions).

### Technical Risks

| Risk | Impact | Mitigation |
|------|--------|-----------|
| **Format Compatibility** | High - Trace decoding errors break ingestion | Comprehensive round-trip tests, fuzzing, correctness tests |
| **Sampling Accuracy** | High - Incorrect sampling affects APM data | Side-by-side comparison with Go agent, extensive testing |
| **Stats Accuracy** | High - Incorrect stats affect APM metrics | Correctness tests comparing aggregated stats |
| **Performance Regression** | Medium - Slower than Go agent | SMP benchmarks, profiling, optimization |
| **Memory Leaks** | High - Long-running process | Memory accounting, leak tests, Valgrind |
| **Configuration Breaking** | High - Existing configs stop working | Configuration compatibility tests, migration guide |
| **API Breaking** | Critical - Tracers can't send data | Multi-version integration tests, backward compatibility |

### Organizational Risks

| Risk | Impact | Mitigation |
|------|--------|-----------|
| **Scope Creep** | Medium - Migration takes too long | Strict PR scope, phased approach |
| **Resource Availability** | Medium - Engineers pulled to other work | Clear prioritization, dedicated team |
| **Go Agent Evolution** | Medium - Go agent adds features during migration | Regular syncs, feature parity tracking |
| **Rollback Complexity** | High - Hard to revert if issues | Gradual rollout, feature flags, kill switch |

---

## Success Metrics

### Functional Metrics
- ✅ All API endpoints functional (20+ endpoints)
- ✅ All trace format versions supported (v0.1-v1.0)
- ✅ Sampling parity with Go agent (all 5 strategies)
- ✅ Stats accuracy >= 99.9% match with Go agent
- ✅ Zero data loss under normal load

### Performance Metrics
- ✅ Throughput >= Go agent (target: 10k traces/sec)
- ✅ P99 latency <= Go agent (target: < 100ms)
- ✅ Memory usage < Go agent (target: 30% reduction)
- ✅ CPU usage <= Go agent
- ✅ No memory leaks (24hr soak test)

### Quality Metrics
- ✅ Test coverage >= 80%
- ✅ All correctness tests pass
- ✅ All SMP regression tests pass
- ✅ Zero critical bugs in production

### Operational Metrics
- ✅ Drop-in configuration replacement
- ✅ Comprehensive observability (metrics, logs, traces)
- ✅ Runbook and troubleshooting docs
- ✅ Migration guide for operators

---

## Rollout Strategy

### Phase A: Development (Weeks 1-12)
- Implement PRs #1-20 in sequence
- Continuous integration testing
- SMP regression testing per PR

### Phase B: Internal Dogfooding (Weeks 13-14)
- Deploy to internal Datadog staging environment
- Monitor metrics, logs, errors
- Identify and fix issues

### Phase C: Beta Release (Weeks 15-16)
- Deploy to opt-in beta customers
- Gather feedback
- Performance validation

### Phase D: Gradual Rollout (Weeks 17-20)
- 1% → 5% → 10% → 25% → 50% → 100%
- Monitor error rates, latency, throughput
- Rollback capability at each stage

### Phase E: Full Production (Week 21+)
- 100% traffic on Rust trace agent
- Deprecate Go trace agent
- Ongoing monitoring and optimization

---

## Open Questions

### Architecture & Integration

1. **libdatadog Integration Strategy:**
   - **Q:** Should we adopt libdatadog's `pb::Span` (protobuf) model throughout Saluki's pipeline?
   - **Context:** Saluki currently uses custom Rust structs (`saluki_core::Span`) for performance. libdatadog uses protobuf spans.
   - **Options:**
     - A) **Maintain dual models** - Convert Saluki Span ↔ pb::Span at libdatadog integration points
     - B) **Adopt pb::Span throughout** - Change Saluki to use protobuf spans internally
     - C) **Avoid libdatadog** - Implement all functionality natively in Saluki
   - **Tradeoffs:**
     - Option A: Conversion overhead at boundaries, but maintains Saluki's performance optimizations (MetaString interning, zero-copy)
     - Option B: Eliminates conversion, but adds protobuf overhead throughout pipeline (allocations, serialization)
     - Option C: More implementation work, but optimal performance and full control
   - **Recommendation:** Option A (convert at boundaries) - Saluki's current architecture is already optimal

2. **Obfuscation: Saluki vs libdatadog:**
   - **Q:** Should we use `libdd-trace-obfuscation` or Saluki's native `trace_obfuscation` transform?
   - **Context:** libdatadog has production-tested obfuscation (SQL, HTTP, Redis, credit cards), but requires pb::Span conversion
   - **Performance Impact:** One-time conversion per span for obfuscation call
   - **Evaluation Needed:** Benchmark conversion overhead vs native implementation complexity

3. **Stats Aggregation: Saluki vs libdatadog:**
   - **Q:** Should we use `libdd-trace-stats::SpanConcentrator` or Saluki's `apm_stats` transform?
   - **Context:** libdatadog's SpanConcentrator is more mature with peer tags, configurable span kinds, production-tested
   - **Performance Impact:** Stats aggregation is read-heavy, conversion overhead may be acceptable
   - **Evaluation Needed:**
     - Compare aggregation accuracy (libdatadog vs Saluki)
     - Benchmark conversion overhead for stats computation
     - Assess peer tag requirements (libdatadog has this, Saluki may not)

4. **v0.4/v0.5 Decoder Reuse:**
   - **Q:** How to efficiently integrate libdatadog's v0.4/v0.5 decoders?
   - **Context:** libdatadog decoders output `Vec<pb::Span>`, Saluki needs `Vec<Span>` (custom struct)
   - **Options:**
     - A) Use libdatadog decoders + write efficient pb::Span → Saluki Span converter
     - B) Reimplement v0.4/v0.5 decoders natively in Saluki
   - **Evaluation Needed:** Profile conversion overhead for decode path

5. **Normalization Integration:**
   - **Q:** Should we use `libdd-trace-normalization` or extend Saluki's existing normalization?
   - **Context:** libdatadog has comprehensive normalization (service names, tag validation, length limits)
   - **Performance Impact:** Normalization runs on every span, conversion overhead matters
   - **Evaluation Needed:** Determine if Saluki already has normalization or needs this functionality

### Functional Requirements

6. **Event Processing:** Is full APM event processing in scope, or handled separately?
7. **Profiling:** Should profiling ingestion be part of trace agent or separate?
8. **Remote Config:** Full remote config support required, or optional?
9. **Backward Compatibility:** How long must we support legacy formats (v0.1-v0.2)?
10. **Feature Flags:** Should we implement feature flags for gradual rollout?
11. **Monitoring:** What are the key SLIs/SLOs for trace ingestion?

### Performance & Optimization

12. **Conversion Optimization:**
    - **Q:** If using libdatadog, how to minimize Saluki Span ↔ pb::Span conversion overhead?
    - **Approaches:**
      - Zero-copy where possible (share byte buffers)
      - Batch conversions to amortize overhead
      - Lazy conversion (convert only fields needed)
      - Pool pb::Span allocations

13. **Hybrid Approach Viability:**
    - **Q:** Can we use libdatadog selectively for complex components (obfuscation) while keeping Saluki native for hot paths (sampling, encoding)?
    - **Evaluation Needed:** Identify hot vs cold paths in trace processing pipeline

---

## Appendix: Key File References

### Datadog Agent (Go)
- `/Users/andrew.glaude/dd/agent-main/pkg/trace/api/endpoints.go` - API endpoint registry
- `/Users/andrew.glaude/dd/agent-main/pkg/trace/api/api.go` - HTTPReceiver implementation
- `/Users/andrew.glaude/dd/agent-main/pkg/trace/agent/agent.go` - Agent orchestration
- `/Users/andrew.glaude/dd/agent-main/pkg/trace/sampler/*.go` - Sampling strategies
- `/Users/andrew.glaude/dd/agent-main/pkg/trace/stats/concentrator.go` - Stats aggregation

### Saluki/Agent Data Plane (Rust)
- `/Users/andrew.glaude/go/src/github.com/DataDog/saluki/bin/agent-data-plane/src/main.rs` - Entry point
- `/Users/andrew.glaude/go/src/github.com/DataDog/saluki/lib/saluki-core/src/data_model/event/trace/mod.rs` - Trace event model (custom Rust structs)
- `/Users/andrew.glaude/go/src/github.com/DataDog/saluki/lib/saluki-components/src/transforms/apm_stats/mod.rs` - Stats transform
- `/Users/andrew.glaude/go/src/github.com/DataDog/saluki/lib/saluki-components/src/transforms/trace_sampler/mod.rs` - Sampling
- `/Users/andrew.glaude/go/src/github.com/DataDog/saluki/lib/saluki-components/src/common/otlp/traces/translator.rs` - OTLP → Saluki conversion example
- `/Users/andrew.glaude/go/src/github.com/DataDog/saluki/test/integration/README.md` - Test framework

### libdatadog (Rust)
- `/Users/andrew.glaude/dd/libdatadog/libdd-trace-obfuscation/src/` - Obfuscation (SQL, HTTP, Redis, credit cards)
- `/Users/andrew.glaude/dd/libdatadog/libdd-trace-normalization/src/` - Span normalization, validation
- `/Users/andrew.glaude/dd/libdatadog/libdd-trace-stats/src/span_concentrator/` - Stats aggregation (SpanConcentrator)
- `/Users/andrew.glaude/dd/libdatadog/libdd-trace-utils/src/msgpack_decoder/` - v0.4/v0.5 MessagePack decoders
- `/Users/andrew.glaude/dd/libdatadog/libdd-data-pipeline/src/trace_exporter/` - TraceExporter with retry logic

---

## Timeline Estimate

**Total Duration:** ~24 weeks (assuming 1 PR per week)

| Phase | PRs | Duration | Cumulative |
|-------|-----|----------|-----------|
| Phase 0: Foundation | #1-2 | 2 weeks | Week 2 |
| Phase 1: OTLP | #3-4 | 2 weeks | Week 4 |
| Phase 2: Simple Proxies | #5-8 | 4 weeks | Week 8 |
| Phase 3: Legacy Trace Formats | #9-11 | 3 weeks | Week 11 |
| Phase 4: Advanced Trace Formats | #12-14 | 3 weeks | Week 14 |
| Phase 5: Stats Endpoint | #15 | 1 week | Week 15 |
| Phase 6: Advanced Sampling | #16-17 | 2 weeks | Week 17 |
| Phase 7: Tag Enrichment | #18-19 | 2 weeks | Week 19 |
| Phase 8: Specialized Endpoints | #20-21 | 2 weeks | Week 21 |
| Phase 9: Production Readiness | #22-24 | 3 weeks | Week 24 |

**Note:** Timeline assumes:
- 1 engineer full-time
- No major blockers
- Parallel work on testing infrastructure
- Proxy endpoints (Phase 2) can progress quickly as they're mostly pass-through

---

## Conclusion

This migration plan provides a structured, incremental approach to rewriting the Datadog Agent's Trace Agent in Rust using Saluki. By breaking the work into ~24 reviewable PRs across 9 phases, we can maintain code quality, ensure comprehensive testing, and enable gradual rollout with minimal risk.

**Key Strategic Decisions:**
1. **Early proxy implementation** (Phase 2): Tackle simpler pass-through endpoints first to validate infrastructure
2. **Progressive complexity**: Build from simple (proxies) → medium (legacy formats) → high (v1.0, stats)
3. **Leverage existing components**: Reuse OTLP, apm_stats, trace_sampler from Saluki
4. **Comprehensive testing**: Unit, integration, correctness, and SMP regression at every phase

The plan focuses on building missing pieces (format decoders, advanced sampling, API endpoints) in a logical sequence that minimizes risk and enables early validation.

**Success depends on:**
1. Rigorous testing at every phase (unit, integration, correctness, SMP)
2. Continuous comparison with Go agent behavior
3. Performance validation against SMP benchmarks
4. Gradual rollout with monitoring and rollback capability

By following this plan, we can deliver a production-ready Rust trace agent that matches or exceeds the Go agent's functionality and performance.
