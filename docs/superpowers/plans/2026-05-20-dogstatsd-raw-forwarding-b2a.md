# DogStatsD raw forwarding B2a implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Split DogStatsD packet listening into a raw-payload relay, fork raw packets to a UDP forwarder, and decode the same raw packets into the existing DogStatsD metric/event/service-check pipeline.

**Architecture:** Add a `Payload::Raw` DogStatsD packet relay that performs network listening/framing and emits one raw packet per payload. Add a raw UDP forwarder controlled by `statsd_forward_host`/`statsd_forward_port`. Add a raw-payload DogStatsD decoder plus event-type router so the existing DogStatsD branches remain mostly unchanged.

**Tech Stack:** Rust, Saluki topology relays/decoders/forwarders/transforms, Tokio UDP, existing DogStatsD codec/framer/listener abstractions.

---

### Task 1: Forwarder tests and implementation

**Files:**
- Modify: `lib/saluki-components/src/sources/dogstatsd/mod.rs`

- [ ] Write a failing async unit test that builds a DogStatsD raw packet forwarder with a local UDP listener and verifies an exact byte-for-byte packet is received.
- [ ] Run the focused test and verify it fails because the forwarder type does not exist.
- [ ] Implement `DogStatsDPacketForwarderConfiguration` and `DogStatsDPacketForwarder` as a `ForwarderBuilder`/`Forwarder` consuming `PayloadType::Raw`.
- [ ] Run the focused test and verify it passes.

### Task 2: Relay tests and implementation

**Files:**
- Modify: `lib/saluki-components/src/sources/dogstatsd/mod.rs`

- [ ] Write a failing async unit test that starts the relay on an ephemeral UDP port, sends a datagram, and verifies a `Payload::Raw` contains the exact datagram bytes.
- [ ] Run the focused test and verify it fails because the relay type does not exist.
- [ ] Implement `DogStatsDPacketRelayConfiguration` and `DogStatsDPacketRelay` as a `RelayBuilder`/`Relay`, reusing existing DogStatsD listener/framer logic.
- [ ] Run the focused test and verify it passes.

### Task 3: Decoder/router tests and implementation

**Files:**
- Modify: `lib/saluki-components/src/sources/dogstatsd/mod.rs`

- [ ] Write a failing async unit test that feeds a `Payload::Raw` DogStatsD metric into the decoder and verifies a metric event is emitted.
- [ ] Write a failing unit test that routes mixed metric/event/service-check events to named outputs by event type.
- [ ] Run focused tests and verify they fail because decoder/router types do not exist.
- [ ] Implement `DogStatsDDecoderConfiguration`/`DogStatsDDecoder` as a `DecoderBuilder`/`Decoder` consuming `PayloadType::Raw` and emitting all DogStatsD event types.
- [ ] Implement `DogStatsDEventRouterConfiguration`/`DogStatsDEventRouter` as a `TransformBuilder`/`Transform` with `metrics`, `events`, and `service_checks` outputs.
- [ ] Run focused tests and verify they pass.

### Task 4: Topology rewiring

**Files:**
- Modify: `lib/saluki-components/src/sources/mod.rs`
- Modify: `lib/saluki-components/src/relays/mod.rs`
- Modify: `lib/saluki-components/src/forwarders/mod.rs`
- Modify: `lib/saluki-components/src/decoders/mod.rs`
- Modify: `lib/saluki-components/src/transforms/mod.rs`
- Modify: `bin/agent-data-plane/src/cli/run.rs`

- [ ] Export new DogStatsD relay/decoder/forwarder/router configs.
- [ ] Rewire `add_dsd_pipeline_to_blueprint` to use `dsd_packets_in -> dsd_decode -> dsd_event_router -> existing branches`.
- [ ] Conditionally add `dsd_packet_forward_out` only when both forwarding config keys are set.
- [ ] Run `cargo check -p agent-data-plane -p saluki-components -p saluki-core` and fix compile errors.

### Task 5: Compatibility docs/config update

**Files:**
- Modify: `lib/saluki-components/src/config_registry/datadog/unsupported.rs`
- Modify: `docs/agent-data-plane/configuration/dogstatsd.md`
- Modify: `docs/agent-data-plane/configuration/dogstatsd/known-configs.json`
- Modify: `test/integration/cases/adp-config-check-exit/config.yaml`

- [ ] Mark `statsd_forward_host` and `statsd_forward_port` as supported/full in the config registry.
- [ ] Move docs/inventory entries to parity.
- [ ] Replace the config-check-exit test sentinel with a different high-severity incompatible key.
- [ ] Run formatting and focused checks.
