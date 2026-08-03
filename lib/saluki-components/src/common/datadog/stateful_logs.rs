//! Stateful Foldspace logs transport and stateless retry recovery.
//!
//! # Missing
//!
//! - Preserve stateful payloads across a process crash before stateless conversion.
//! - Remove the persisted retry queue's existing dequeue crash window.

use std::{
    collections::VecDeque,
    io::{Read as _, Write as _},
    time::Duration,
};

use bytes::Buf;
use chrono::{DateTime, Utc};
use flate2::{
    read::{GzDecoder, ZlibDecoder},
    write::{GzEncoder, ZlibEncoder},
    Compression,
};
use foldspace_core::{
    proto::stateful::{
        batch_status, stateful_intake_client::StatefulIntakeClient, BatchStatus, StatefulBatch as ProtoStatefulBatch,
    },
    CoreError, DefaultBatchEncoder, DispatchPolicy, LogRecord, MplexEffect, MplexTimerKind, MultiStreamConfig,
    MultiStreamCore, PayloadDelivery, PayloadId, ProtoBatchEncoder, SenderId, StreamError, StreamId,
    ZstdBatchCompressor,
};
use foldspace_patterns::ClusteringPatternExtractor;
use foldspace_server::StatefulLogsDecoder;
use http::{
    header::{CONTENT_ENCODING, CONTENT_LENGTH},
    HeaderValue, Request,
};
use saluki_common::collections::FastHashMap;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_metrics::MetricsBuilder;
use serde_json::{Map as JsonMap, Value as JsonValue};
use stringtheory::MetaString;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{metadata::MetadataValue, transport::Channel, Request as TonicRequest};
use tracing::{debug, error, warn};
use uuid::Uuid;

use super::{
    endpoints::ResolvedEndpoint,
    io::{track_queue_drops, PendingTransactions},
    telemetry::ComponentTelemetry,
    transaction::{Metadata, Transaction, TransactionBody},
};

const LOGS_INTAKE_PATH: &str = "/api/v2/logs";
const STATEFUL_SENDERS: usize = 3;
const STATEFUL_BATCH_CAPACITY: usize = usize::MAX;
const STATEFUL_CHANNEL_CAPACITY: usize = 1;
const STATE_REQUEST_BYTES: u64 = 5 * 1024 * 1024;
const DUAL_SEND_UUID_FIELD: &str = "dual-send-uuid";

type StatefulCore = MultiStreamCore<DefaultBatchEncoder, ClusteringPatternExtractor>;

#[derive(Debug)]
struct LogTemplate {
    object: JsonMap<String, JsonValue>,
}

struct RetainedRequest<B>
where
    B: Buf + Clone,
{
    metadata: Metadata,
    request: Request<TransactionBody<B>>,
    templates: Vec<LogTemplate>,
}

enum StatefulRequest<B>
where
    B: Buf + Clone,
{
    Retained {
        request: RetainedRequest<B>,
        records: Vec<LogRecord>,
    },
    Passthrough(Transaction<B>),
}

impl<B> RetainedRequest<B>
where
    B: Buf + Clone,
{
    fn try_from_transaction(transaction: Transaction<B>) -> StatefulRequest<B> {
        let (metadata, request) = transaction.into_parts();
        match parse_request_logs(&request) {
            Ok((templates, records)) if !records.is_empty() => StatefulRequest::Retained {
                request: Self {
                    metadata,
                    request: request.map(|_| TransactionBody::from(Vec::new())),
                    templates,
                },
                records,
            },
            Ok(_) | Err(_) => StatefulRequest::Passthrough(Transaction::reassemble(metadata, request)),
        }
    }

    fn into_transaction(self, logs: Vec<foldspace_server::DecodedLog>) -> Result<Transaction<B>, GenericError> {
        if logs.len() != self.templates.len() {
            return Err(generic_error!(
                "Foldspace recovery produced {} logs for {} retained templates.",
                logs.len(),
                self.templates.len()
            ));
        }

        let mut values = Vec::with_capacity(logs.len());
        for (mut template, log) in self.templates.into_iter().zip(logs) {
            template
                .object
                .insert("message".to_owned(), JsonValue::String(log.message));
            if let Some(uuid) = log.uuid {
                template
                    .object
                    .insert(DUAL_SEND_UUID_FIELD.to_owned(), JsonValue::String(uuid));
            }
            values.push(JsonValue::Object(template.object));
        }

        let uncompressed = serde_json::to_vec(&values).error_context("Failed to encode recovered stateless logs.")?;
        let encoding = content_encoding(self.request.headers())?;
        let body = compress_body(&uncompressed, encoding)?;
        let mut request = self.request.map(|_| TransactionBody::from(body));
        if request.headers().contains_key(CONTENT_LENGTH) {
            let content_length = HeaderValue::from_str(&request.body().remaining().to_string())
                .error_context("Failed to update recovered logs content length.")?;
            request.headers_mut().insert(CONTENT_LENGTH, content_length);
        }
        Ok(Transaction::reassemble(self.metadata, request))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BodyEncoding {
    Identity,
    Gzip,
    Deflate,
    Zstd,
}

fn content_encoding(headers: &http::HeaderMap) -> Result<BodyEncoding, GenericError> {
    let Some(value) = headers.get(CONTENT_ENCODING) else {
        return Ok(BodyEncoding::Identity);
    };
    match value.to_str().unwrap_or_default().trim().to_ascii_lowercase().as_str() {
        "" | "identity" => Ok(BodyEncoding::Identity),
        "gzip" => Ok(BodyEncoding::Gzip),
        "deflate" => Ok(BodyEncoding::Deflate),
        "zstd" => Ok(BodyEncoding::Zstd),
        other => Err(generic_error!(
            "Stateful logs do not support HTTP content encoding '{}'.",
            other
        )),
    }
}

fn copy_body<B>(body: &TransactionBody<B>) -> Vec<u8>
where
    B: Buf + Clone,
{
    let mut body = body.clone();
    let mut bytes = Vec::with_capacity(body.remaining());
    while body.has_remaining() {
        let chunk = body.chunk();
        bytes.extend_from_slice(chunk);
        let chunk_len = chunk.len();
        body.advance(chunk_len);
    }
    bytes
}

fn decompress_body(bytes: &[u8], encoding: BodyEncoding) -> Result<Vec<u8>, GenericError> {
    match encoding {
        BodyEncoding::Identity => Ok(bytes.to_vec()),
        BodyEncoding::Gzip => {
            let mut decoder = GzDecoder::new(bytes);
            let mut decoded = Vec::new();
            decoder
                .read_to_end(&mut decoded)
                .error_context("Failed to decompress gzip logs transaction.")?;
            Ok(decoded)
        }
        BodyEncoding::Deflate => {
            let mut decoder = ZlibDecoder::new(bytes);
            let mut decoded = Vec::new();
            decoder
                .read_to_end(&mut decoded)
                .error_context("Failed to decompress deflate logs transaction.")?;
            Ok(decoded)
        }
        BodyEncoding::Zstd => {
            zstd::stream::decode_all(bytes).error_context("Failed to decompress zstd logs transaction.")
        }
    }
}

fn compress_body(bytes: &[u8], encoding: BodyEncoding) -> Result<Vec<u8>, GenericError> {
    match encoding {
        BodyEncoding::Identity => Ok(bytes.to_vec()),
        BodyEncoding::Gzip => {
            let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
            encoder
                .write_all(bytes)
                .error_context("Failed to gzip recovered logs transaction.")?;
            encoder
                .finish()
                .error_context("Failed to finish recovered gzip logs transaction.")
        }
        BodyEncoding::Deflate => {
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
            encoder
                .write_all(bytes)
                .error_context("Failed to deflate recovered logs transaction.")?;
            encoder
                .finish()
                .error_context("Failed to finish recovered deflate logs transaction.")
        }
        BodyEncoding::Zstd => {
            zstd::stream::encode_all(bytes, 0).error_context("Failed to zstd-compress recovered logs transaction.")
        }
    }
}

fn parse_request_logs<B>(
    request: &Request<TransactionBody<B>>,
) -> Result<(Vec<LogTemplate>, Vec<LogRecord>), GenericError>
where
    B: Buf + Clone,
{
    let encoding = content_encoding(request.headers())?;
    let encoded = copy_body(request.body());
    let decoded = decompress_body(&encoded, encoding)?;
    let values: Vec<JsonValue> =
        serde_json::from_slice(&decoded).error_context("Failed to parse stateless logs transaction.")?;
    let mut templates = Vec::with_capacity(values.len());
    let mut records = Vec::with_capacity(values.len());
    for value in values {
        let JsonValue::Object(mut object) = value else {
            return Err(generic_error!("Logs transaction contained a non-object entry."));
        };
        let timestamp_millis = log_timestamp_millis(&object)?;
        let message = take_required_string(&mut object, "message")?;
        let status = optional_string(&object, "status")?;
        let hostname = optional_string(&object, "hostname")?;
        let service = optional_string(&object, "service")?;
        let source = optional_string(&object, "ddsource")?;
        let tags = optional_string(&object, "ddtags")?;
        let uuid =
            take_optional_string(&mut object, DUAL_SEND_UUID_FIELD)?.unwrap_or_else(|| Uuid::now_v7().to_string());

        let mut record = LogRecord::new(message.into_bytes(), timestamp_millis);
        record.status = status;
        record.hostname = hostname;
        record.service = service;
        record.source = source;
        record.tags = tags
            .as_deref()
            .map(|tags| tags.split(',').map(ToOwned::to_owned).collect())
            .unwrap_or_default();
        record.uuid = Some(uuid);
        records.push(record);
        templates.push(LogTemplate { object });
    }
    Ok((templates, records))
}

fn take_required_string(object: &mut JsonMap<String, JsonValue>, field: &str) -> Result<String, GenericError> {
    take_optional_string(object, field)?.ok_or_else(|| generic_error!("Log entry is missing string field '{}'.", field))
}

fn take_optional_string(object: &mut JsonMap<String, JsonValue>, field: &str) -> Result<Option<String>, GenericError> {
    match object.remove(field) {
        Some(JsonValue::String(value)) => Ok(Some(value)),
        Some(_) => Err(generic_error!("Log field '{}' is not a string.", field)),
        None => Ok(None),
    }
}

fn optional_string(object: &JsonMap<String, JsonValue>, field: &str) -> Result<Option<String>, GenericError> {
    match object.get(field) {
        Some(JsonValue::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(generic_error!("Log field '{}' is not a string.", field)),
        None => Ok(None),
    }
}

fn log_timestamp_millis(object: &JsonMap<String, JsonValue>) -> Result<i64, GenericError> {
    if let Some(value) = object.get("timestamp") {
        return value
            .as_i64()
            .ok_or_else(|| generic_error!("Log timestamp is not an integer."));
    }
    if let Some(value) = object.get("@timestamp") {
        let value = value
            .as_str()
            .ok_or_else(|| generic_error!("Log @timestamp is not a string."))?;
        return DateTime::parse_from_rfc3339(value)
            .map(|timestamp| timestamp.timestamp_millis())
            .error_context("Failed to parse log @timestamp.");
    }
    Ok(Utc::now().timestamp_millis())
}

#[derive(Debug)]
struct OutboundBatch {
    proto: ProtoStatefulBatch,
    payload_id: Option<PayloadId>,
}

#[derive(Debug)]
struct InflightBatch {
    batch_id: u64,
    payload_id: Option<PayloadId>,
}

#[derive(Debug)]
struct SenderStream {
    stream_id: StreamId,
    outbound: mpsc::Sender<ProtoStatefulBatch>,
    inflight: Option<InflightBatch>,
    pending: VecDeque<OutboundBatch>,
}

impl SenderStream {
    fn payload_delivery(&self) -> Option<(PayloadId, PayloadDelivery)> {
        if let Some(payload_id) = self.inflight.as_ref().and_then(|batch| batch.payload_id) {
            return Some((payload_id, PayloadDelivery::Unknown));
        }
        self.pending
            .iter()
            .find_map(|batch| batch.payload_id)
            .map(|payload_id| (payload_id, PayloadDelivery::NotSent))
    }

    fn remove_payload(&mut self, payload_id: PayloadId) {
        if self
            .inflight
            .as_ref()
            .is_some_and(|batch| batch.payload_id == Some(payload_id))
        {
            self.inflight = None;
        }
        self.pending.retain(|batch| batch.payload_id != Some(payload_id));
    }
}

#[derive(Debug)]
pub(super) enum StatefulEvent {
    Opened {
        sender_id: SenderId,
        stream_id: StreamId,
        outbound: mpsc::Sender<ProtoStatefulBatch>,
    },
    OpenFailed {
        sender_id: SenderId,
        stream_id: StreamId,
        error: MetaString,
    },
    Ack {
        sender_id: SenderId,
        stream_id: StreamId,
        batch_id: u64,
    },
    Failed {
        sender_id: SenderId,
        stream_id: StreamId,
        error: MetaString,
        failed_payload: Option<(PayloadId, PayloadDelivery)>,
    },
    Timer {
        sender_id: SenderId,
        kind: MplexTimerKind,
    },
}

#[derive(Clone)]
struct StatefulTelemetry {
    converted: metrics::Counter,
    conversion_errors: metrics::Counter,
    oversized: metrics::Counter,
}

impl StatefulTelemetry {
    fn new(builder: &MetricsBuilder, domain: &str) -> Self {
        let domain_tag = ("domain", domain.to_owned());
        Self {
            converted: builder
                .register_counter_with_tags("stateful_logs_payloads_converted_total", [domain_tag.clone()]),
            conversion_errors: builder
                .register_counter_with_tags("stateful_logs_conversion_errors_total", [domain_tag.clone()]),
            oversized: builder.register_counter_with_tags("stateful_logs_stateless_oversized_total", [domain_tag]),
        }
    }
}

pub(super) struct StatefulLogsSender<B>
where
    B: Buf + Clone,
{
    core: StatefulCore,
    client: StatefulIntakeClient<Channel>,
    encoder: ProtoBatchEncoder<ZstdBatchCompressor>,
    endpoint: ResolvedEndpoint,
    open_timeout: Duration,
    streams: FastHashMap<u64, SenderStream>,
    retained: FastHashMap<u64, RetainedRequest<B>>,
    events_tx: mpsc::UnboundedSender<StatefulEvent>,
    events_rx: mpsc::UnboundedReceiver<StatefulEvent>,
    telemetry: StatefulTelemetry,
    disabled: bool,
}

impl<B> StatefulLogsSender<B>
where
    B: Buf + Clone,
{
    pub(super) fn new(
        endpoint: ResolvedEndpoint, metrics_builder: &MetricsBuilder, endpoint_domain: &str, request_timeout: Duration,
    ) -> Result<Self, GenericError> {
        let authority = endpoint
            .logs_authority()
            .map(ToString::to_string)
            .unwrap_or_else(|| endpoint.endpoint().authority().to_owned());
        let grpc_endpoint = format!("{}://{}", endpoint.endpoint().scheme(), authority);
        let channel = Channel::from_shared(grpc_endpoint.clone())
            .error_context("Failed to build Foldspace endpoint.")?
            .connect_timeout(request_timeout)
            .connect_lazy();
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let core = MultiStreamCore::with_encoder_and_extractor(
            MultiStreamConfig {
                senders: STATEFUL_SENDERS,
                dispatch: DispatchPolicy::Reliable,
                batch_capacity: STATEFUL_BATCH_CAPACITY,
                max_outstanding_payloads: 1,
                fold_catch_up: true,
                ..MultiStreamConfig::default()
            },
            DefaultBatchEncoder,
            ClusteringPatternExtractor::new(),
        );

        debug!(endpoint = %grpc_endpoint, "Configured endpoint-scoped Foldspace sender.");
        Ok(Self {
            core,
            client: StatefulIntakeClient::new(channel),
            encoder: ProtoBatchEncoder::new(ZstdBatchCompressor::default()),
            endpoint,
            open_timeout: request_timeout,
            streams: FastHashMap::default(),
            retained: FastHashMap::default(),
            events_tx,
            events_rx,
            telemetry: StatefulTelemetry::new(metrics_builder, endpoint_domain),
            disabled: false,
        })
    }

    pub(super) async fn start(&mut self) {
        let effects = self.core.start();
        self.execute(effects).await;
    }

    pub(super) async fn next_event(&mut self) -> Option<StatefulEvent> {
        self.events_rx.recv().await
    }

    pub(super) async fn try_send_transaction(
        &mut self, transaction: Transaction<B>, pending: &mut PendingTransactions<Transaction<B>>,
        component_telemetry: &ComponentTelemetry, endpoint_domain: &str,
    ) -> Result<(), Transaction<B>> {
        if self.disabled || transaction.request_uri().path() != LOGS_INTAKE_PATH {
            return Err(transaction);
        }
        let (retained, records) = match RetainedRequest::try_from_transaction(transaction) {
            StatefulRequest::Retained { request, records } => (request, records),
            StatefulRequest::Passthrough(transaction) => return Err(transaction),
        };
        for record in &records {
            let effects = self.core.push_log(record);
            debug_assert!(
                effects.is_empty(),
                "one HTTP transaction must map to one Foldspace payload"
            );
        }
        let (payload_id, effects) = self.core.flush_with_payload_id();
        let payload_id = payload_id.expect("a non-empty logs transaction must flush one payload");
        self.retained.insert(payload_id.get(), retained);

        let high_priority_full = pending.high_priority_is_full_with(self.retained.len());
        if high_priority_full {
            let _ = self
                .recover_to_retry(
                    payload_id,
                    PayloadDelivery::NotSent,
                    pending,
                    component_telemetry,
                    endpoint_domain,
                )
                .await;
        } else {
            self.execute(effects).await;
        }
        Ok(())
    }

    pub(super) async fn handle_event(
        &mut self, event: StatefulEvent, pending: &mut PendingTransactions<Transaction<B>>,
        component_telemetry: &ComponentTelemetry, endpoint_domain: &str,
    ) {
        match event {
            StatefulEvent::Opened {
                sender_id,
                stream_id,
                outbound,
            } => {
                self.streams.insert(
                    sender_id.get(),
                    SenderStream {
                        stream_id,
                        outbound,
                        inflight: None,
                        pending: VecDeque::new(),
                    },
                );
                let effects = self.core.handle_stream_opened(sender_id, stream_id);
                self.execute(effects).await;
            }
            StatefulEvent::OpenFailed {
                sender_id,
                stream_id,
                error,
            } => {
                let effects = self
                    .core
                    .handle_stream_error(sender_id, stream_id, StreamError::new(error.as_ref()));
                self.execute(effects).await;
            }
            StatefulEvent::Ack {
                sender_id,
                stream_id,
                batch_id,
            } => {
                let Some(stream) = self.streams.get_mut(&sender_id.get()) else {
                    return;
                };
                if stream.stream_id != stream_id {
                    return;
                }
                let payload_id = if stream.inflight.as_ref().is_some_and(|batch| batch.batch_id == batch_id) {
                    stream.inflight.take().and_then(|batch| batch.payload_id)
                } else {
                    None
                };
                let effects = self.core.handle_ack(sender_id, stream_id, batch_id);
                if let Some(payload_id) = payload_id {
                    if let Some(retained) = self.retained.remove(&payload_id.get()) {
                        component_telemetry.track_successful_transaction(&retained.metadata, endpoint_domain);
                    }
                }
                self.execute(effects).await;
                self.try_send(sender_id).await;
            }
            StatefulEvent::Failed {
                sender_id,
                stream_id,
                error,
                failed_payload,
            } => {
                let Some(stream) = self.streams.get(&sender_id.get()) else {
                    return;
                };
                if stream.stream_id != stream_id {
                    return;
                }
                let stream = self
                    .streams
                    .remove(&sender_id.get())
                    .expect("Foldspace stream was checked above");
                let recovery = failed_payload.or_else(|| stream.payload_delivery());
                let effects = self
                    .core
                    .handle_stream_error(sender_id, stream_id, StreamError::new(error.as_ref()));
                if let Some((payload_id, delivery)) = recovery {
                    if !self
                        .recover_to_retry(payload_id, delivery, pending, component_telemetry, endpoint_domain)
                        .await
                    {
                        return;
                    }
                }
                self.execute(effects).await;
            }
            StatefulEvent::Timer { sender_id, kind } => {
                let effects = self.core.handle_timer(sender_id, kind);
                self.execute(effects).await;
            }
        }
    }

    pub(super) async fn shutdown(
        &mut self, pending: &mut PendingTransactions<Transaction<B>>, component_telemetry: &ComponentTelemetry,
        endpoint_domain: &str,
    ) {
        let payloads = self.retained.keys().copied().map(PayloadId).collect::<Vec<_>>();
        for payload_id in payloads {
            let Some(sender_id) = self.core.payload_sender(payload_id) else {
                continue;
            };
            let delivery = self
                .streams
                .get(&sender_id.get())
                .and_then(SenderStream::payload_delivery)
                .filter(|(candidate, _)| *candidate == payload_id)
                .map_or(PayloadDelivery::NotSent, |(_, delivery)| delivery);
            if delivery == PayloadDelivery::Unknown {
                if let Some(stream) = self.streams.remove(&sender_id.get()) {
                    let _ = self.core.handle_stream_error(
                        sender_id,
                        stream.stream_id,
                        StreamError::new("graceful shutdown"),
                    );
                }
            }
            let _ = self
                .recover_to_retry(payload_id, delivery, pending, component_telemetry, endpoint_domain)
                .await;
        }
        self.streams.clear();
    }

    async fn recover_to_retry(
        &mut self, payload_id: PayloadId, delivery: PayloadDelivery, pending: &mut PendingTransactions<Transaction<B>>,
        component_telemetry: &ComponentTelemetry, endpoint_domain: &str,
    ) -> bool {
        let Some(sender_id) = self.core.payload_sender(payload_id) else {
            return true;
        };
        let recovery = match self.core.begin_recovery(sender_id, payload_id, delivery) {
            Ok(recovery) => recovery,
            Err(error) => {
                self.disabled = true;
                self.telemetry.conversion_errors.increment(1);
                error!(payload_id = payload_id.get(), %error, "Failed to begin Foldspace stateless recovery.");
                return false;
            }
        };
        for stream in self.streams.values_mut() {
            stream.remove_payload(payload_id);
        }
        let Some(retained) = self.retained.remove(&payload_id.get()) else {
            self.telemetry.conversion_errors.increment(1);
            error!(
                payload_id = payload_id.get(),
                "Foldspace recovery lost its request metadata."
            );
            self.core.complete_recovery(recovery);
            return true;
        };
        let metadata = retained.metadata.clone();
        let transaction = StatefulLogsDecoder::new()
            .decode_recovery(&recovery)
            .error_context("Failed to decode retained Foldspace payload.")
            .and_then(|logs| retained.into_transaction(logs));
        let transaction = match transaction {
            Ok(transaction) => transaction,
            Err(error) => {
                self.telemetry.conversion_errors.increment(1);
                component_telemetry.track_permanently_failed_transaction(&metadata, None, endpoint_domain);
                error!(payload_id = payload_id.get(), %error, "Failed to reconstruct Foldspace payload; dropping it.");
                self.core.complete_recovery(recovery);
                return true;
            }
        };

        match pending.push_low_priority(transaction).await {
            Ok(push_result) => {
                self.telemetry.converted.increment(1);
                track_queue_drops(component_telemetry, endpoint_domain, push_result);
            }
            Err(error) => {
                self.telemetry.oversized.increment(1);
                component_telemetry.track_permanently_failed_transaction(&metadata, None, endpoint_domain);
                error!(
                    payload_id = payload_id.get(),
                    %error,
                    "Recovered stateless payload exceeds retry queue limits; dropping it without stateful fallback."
                );
            }
        }
        let effects = self.core.complete_recovery(recovery);
        self.execute(effects).await;
        true
    }

    async fn execute(&mut self, effects: Vec<MplexEffect>) {
        let mut effects = VecDeque::from(effects);
        while let Some(effect) = effects.pop_front() {
            match effect {
                MplexEffect::OpenStream { sender_id, stream_id } => {
                    if let Err(error) = self.open_stream(sender_id, stream_id) {
                        effects.extend(self.core.handle_stream_error(
                            sender_id,
                            stream_id,
                            StreamError::new(error.to_string()),
                        ));
                    }
                }
                MplexEffect::SendBatch {
                    sender_id,
                    payload_id,
                    batch,
                } => match self.encoder.encode(&batch) {
                    Ok(proto) => {
                        if let Some(stream) = self.streams.get_mut(&sender_id.get()) {
                            stream.pending.push_back(OutboundBatch { proto, payload_id });
                            self.try_send(sender_id).await;
                        }
                    }
                    Err(error) => {
                        let _ = self.events_tx.send(StatefulEvent::Failed {
                            sender_id,
                            stream_id: batch.stream,
                            error: MetaString::from(format!("failed to encode Foldspace batch: {error:?}")),
                            failed_payload: payload_id.map(|payload_id| (payload_id, PayloadDelivery::NotSent)),
                        });
                    }
                },
                MplexEffect::CloseStream { sender_id, .. } => {
                    self.streams.remove(&sender_id.get());
                }
                MplexEffect::ScheduleTimer { sender_id, timer } => {
                    let events_tx = self.events_tx.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(timer.after).await;
                        let _ = events_tx.send(StatefulEvent::Timer {
                            sender_id,
                            kind: timer.kind,
                        });
                    });
                }
                MplexEffect::ReportError { sender_id, error } => match error {
                    CoreError::StreamFailed(error) => {
                        warn!(sender_id = sender_id.get(), error = ?error, "Foldspace stream failed.");
                    }
                    error => {
                        error!(
                            sender_id = sender_id.get(),
                            ?error,
                            "Foldspace sender reported a protocol error."
                        );
                        if let Some(stream) = self.streams.get(&sender_id.get()) {
                            let _ = self.events_tx.send(StatefulEvent::Failed {
                                sender_id,
                                stream_id: stream.stream_id,
                                error: MetaString::from_static("unrecoverable Foldspace protocol error"),
                                failed_payload: None,
                            });
                        }
                    }
                },
            }
        }
    }

    fn open_stream(&mut self, sender_id: SenderId, stream_id: StreamId) -> Result<(), GenericError> {
        let (outbound, receiver) = mpsc::channel(STATEFUL_CHANNEL_CAPACITY);
        let mut request = TonicRequest::new(ReceiverStream::new(receiver));
        let api_key = MetadataValue::try_from(self.endpoint.api_key())
            .error_context("Foldspace API key is not valid gRPC metadata.")?;
        request.metadata_mut().insert("dd-api-key", api_key);
        request
            .metadata_mut()
            .insert("dd-content-encoding", MetadataValue::from_static("zstd"));
        let state_request_bytes = MetadataValue::try_from(STATE_REQUEST_BYTES.to_string())
            .error_context("Foldspace state request limit is not valid gRPC metadata.")?;
        request
            .metadata_mut()
            .insert("dd-state-request-bytes", state_request_bytes);
        let mut client = self.client.clone();
        let events_tx = self.events_tx.clone();
        let open_timeout = self.open_timeout;
        tokio::spawn(async move {
            match tokio::time::timeout(open_timeout, client.stateful_stream(request)).await {
                Ok(Ok(response)) => {
                    if events_tx
                        .send(StatefulEvent::Opened {
                            sender_id,
                            stream_id,
                            outbound,
                        })
                        .is_ok()
                    {
                        spawn_response_reader(sender_id, stream_id, response.into_inner(), events_tx);
                    }
                }
                Ok(Err(error)) => {
                    let _ = events_tx.send(StatefulEvent::OpenFailed {
                        sender_id,
                        stream_id,
                        error: MetaString::from(format!("failed to open Foldspace stream: {error}")),
                    });
                }
                Err(_) => {
                    let _ = events_tx.send(StatefulEvent::OpenFailed {
                        sender_id,
                        stream_id,
                        error: MetaString::from_static("timed out opening Foldspace stream"),
                    });
                }
            }
        });
        Ok(())
    }

    async fn try_send(&mut self, sender_id: SenderId) {
        let Some(stream) = self.streams.get_mut(&sender_id.get()) else {
            return;
        };
        if stream.inflight.is_some() {
            return;
        }
        let Some(batch) = stream.pending.pop_front() else {
            return;
        };
        let batch_id = u64::from(batch.proto.batch_id);
        let payload_id = batch.payload_id;
        match stream.outbound.send(batch.proto).await {
            Ok(()) => {
                stream.inflight = Some(InflightBatch { batch_id, payload_id });
            }
            Err(error) => {
                let error_message = error.to_string();
                stream.pending.push_front(OutboundBatch {
                    proto: error.0,
                    payload_id,
                });
                let _ = self.events_tx.send(StatefulEvent::Failed {
                    sender_id,
                    stream_id: stream.stream_id,
                    error: MetaString::from(format!("failed to transmit Foldspace batch: {error_message}")),
                    failed_payload: None,
                });
            }
        }
    }
}

fn spawn_response_reader(
    sender_id: SenderId, stream_id: StreamId, mut responses: tonic::Streaming<BatchStatus>,
    events_tx: mpsc::UnboundedSender<StatefulEvent>,
) {
    tokio::spawn(async move {
        loop {
            match responses.message().await {
                Ok(Some(status)) if status.status == i32::from(batch_status::Status::Ok) => {
                    if events_tx
                        .send(StatefulEvent::Ack {
                            sender_id,
                            stream_id,
                            batch_id: u64::from(status.batch_id),
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(Some(status)) => {
                    let _ = events_tx.send(StatefulEvent::Failed {
                        sender_id,
                        stream_id,
                        error: MetaString::from(format!(
                            "Foldspace intake rejected batch {} with status {}",
                            status.batch_id, status.status
                        )),
                        failed_payload: None,
                    });
                    return;
                }
                Ok(None) => {
                    let _ = events_tx.send(StatefulEvent::Failed {
                        sender_id,
                        stream_id,
                        error: MetaString::from_static("Foldspace response stream closed"),
                        failed_payload: None,
                    });
                    return;
                }
                Err(error) => {
                    let _ = events_tx.send(StatefulEvent::Failed {
                        sender_id,
                        stream_id,
                        error: MetaString::from(format!("Foldspace acknowledgement failed: {error}")),
                        failed_payload: None,
                    });
                    return;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc};

    use bytes::Bytes;
    use http::header::{CONTENT_ENCODING, CONTENT_TYPE};
    use saluki_io::net::util::retry::{DiskUsageRetrieverImpl, PersistedQueueArgs, RetryQueue};

    use super::*;
    use crate::common::datadog::{
        io::PendingTransaction,
        telemetry::{SharedTransactionQueueTelemetry, TransactionQueueTelemetry},
    };

    const TEST_DOMAIN: &str = "http://127.0.0.1";
    const TEST_RETRY_BYTES: u64 = 64 * 1024;

    fn transaction(body: Vec<u8>, encoding: Option<&'static str>) -> Transaction<Bytes> {
        let mut builder = Request::builder()
            .method("POST")
            .uri(LOGS_INTAKE_PATH)
            .header(CONTENT_TYPE, "application/json")
            .header("x-test-routing", "route-a");
        if let Some(encoding) = encoding {
            builder = builder.header(CONTENT_ENCODING, encoding);
        }
        Transaction::from_original(
            Metadata::from_event_and_data_point_count(1, 0),
            builder.body(Bytes::from(body)).unwrap(),
        )
    }

    fn parsed_body(transaction: Transaction<Bytes>) -> Vec<JsonValue> {
        let (_, request) = transaction.into_parts();
        let encoding = content_encoding(request.headers()).unwrap();
        let body = decompress_body(&copy_body(request.body()), encoding).unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    fn retain(transaction: Transaction<Bytes>) -> (RetainedRequest<Bytes>, Vec<LogRecord>) {
        match RetainedRequest::try_from_transaction(transaction) {
            StatefulRequest::Retained { request, records } => (request, records),
            StatefulRequest::Passthrough(_) => {
                panic!("test logs transaction should be accepted for stateful encoding")
            }
        }
    }

    fn pending(max_high_priority: usize, max_retry_bytes: u64) -> PendingTransactions<Transaction<Bytes>> {
        let metrics_builder = MetricsBuilder::default();
        let shared = SharedTransactionQueueTelemetry::from_builder(&metrics_builder);
        let telemetry = TransactionQueueTelemetry::from_builder(&metrics_builder, TEST_DOMAIN, shared);
        PendingTransactions::new(
            max_high_priority,
            RetryQueue::new("stateful-logs-test".to_owned(), max_retry_bytes),
            telemetry,
            MetaString::from_static(TEST_DOMAIN),
            900,
        )
    }

    fn sender(endpoint: &str, api_key: &str) -> StatefulLogsSender<Bytes> {
        let endpoint = ResolvedEndpoint::from_raw_endpoint(endpoint, api_key).unwrap();
        StatefulLogsSender::new(
            endpoint,
            &MetricsBuilder::default(),
            TEST_DOMAIN,
            Duration::from_secs(1),
        )
        .unwrap()
    }

    fn open_core_streams(sender: &mut StatefulLogsSender<Bytes>) -> Vec<mpsc::Receiver<ProtoStatefulBatch>> {
        let mut receivers = Vec::new();
        for effect in sender.core.start() {
            let MplexEffect::OpenStream { sender_id, stream_id } = effect else {
                panic!("starting Foldspace should only open streams");
            };
            let (outbound, receiver) = mpsc::channel(STATEFUL_CHANNEL_CAPACITY);
            sender.streams.insert(
                sender_id.get(),
                SenderStream {
                    stream_id,
                    outbound,
                    inflight: None,
                    pending: VecDeque::new(),
                },
            );
            assert!(sender.core.handle_stream_opened(sender_id, stream_id).is_empty());
            receivers.push(receiver);
        }
        receivers
    }

    fn simple_log(message: &str, uuid: &str) -> Transaction<Bytes> {
        transaction(
            serde_json::to_vec(&serde_json::json!([{
                "message": message,
                "@timestamp": "2026-08-03T12:00:00.000Z",
                "dual-send-uuid": uuid,
                "custom": "preserved"
            }]))
            .unwrap(),
            None,
        )
    }

    #[test]
    fn recovered_transaction_preserves_fields_uuid_and_compression() {
        let input = serde_json::json!([{
            "message": "user alice logged in from 10.0.0.2",
            "status": "info",
            "hostname": "host-a",
            "service": "auth",
            "ddsource": "rust",
            "ddtags": "env:test,team:logs",
            "timestamp": 1700000000000_i64,
            "custom": {"nested": true},
            "dual-send-uuid": "uuid-p2"
        }]);
        let compressed = compress_body(&serde_json::to_vec(&input).unwrap(), BodyEncoding::Zstd).unwrap();
        let (retained, records) = retain(transaction(compressed, Some("zstd")));
        assert_eq!(records[0].uuid.as_deref(), Some("uuid-p2"));

        let decoded = foldspace_server::DecodedLog {
            message: "user alice logged in from 10.0.0.2".to_owned(),
            status: Some("info".to_owned()),
            hostname: Some("host-a".to_owned()),
            service: Some("auth".to_owned()),
            ddsource: Some("rust".to_owned()),
            tags: Some("env:test,team:logs".to_owned()),
            timestamp_millis: 1_700_000_000_000,
            uuid: Some("uuid-p2".to_owned()),
        };
        let output = retained.into_transaction(vec![decoded]).unwrap();
        assert_eq!(output.metadata().event_count, 1);
        assert_eq!(output.request_uri().path(), LOGS_INTAKE_PATH);
        let (_, output_request) = output.clone().into_parts();
        assert_eq!(output_request.method(), "POST");
        assert_eq!(output_request.headers()["x-test-routing"], "route-a");
        assert_eq!(output_request.headers()[CONTENT_ENCODING], "zstd");
        let values = parsed_body(output);
        assert_eq!(values, input.as_array().unwrap().clone());
    }

    #[test]
    fn missing_uuid_is_added_to_both_stateful_and_recovered_forms() {
        let input = serde_json::to_vec(&serde_json::json!([{
            "message": "hello 42",
            "@timestamp": "2026-08-03T12:00:00.000Z"
        }]))
        .unwrap();
        let (retained, records) = retain(transaction(input, None));
        let uuid = records[0].uuid.clone().unwrap();
        let output = retained
            .into_transaction(vec![foldspace_server::DecodedLog {
                message: "hello 42".to_owned(),
                timestamp_millis: 1_775_390_400_000,
                uuid: Some(uuid.clone()),
                ..foldspace_server::DecodedLog::default()
            }])
            .unwrap();
        assert_eq!(parsed_body(output)[0][DUAL_SEND_UUID_FIELD], uuid);
    }

    #[tokio::test]
    async fn high_priority_overflow_converts_before_retry_and_keeps_healthy_streams() {
        let mut sender = sender("http://127.0.0.1:4317", "key-a");
        let _receivers = open_core_streams(&mut sender);
        let mut pending = pending(0, TEST_RETRY_BYTES);
        let telemetry = ComponentTelemetry::from_builder(&MetricsBuilder::default());

        assert!(sender
            .try_send_transaction(
                simple_log("hello 42", "overflow-uuid"),
                &mut pending,
                &telemetry,
                TEST_DOMAIN
            )
            .await
            .is_ok());

        assert!(sender.retained.is_empty());
        assert_eq!(sender.streams.len(), STATEFUL_SENDERS);
        assert!(sender.streams.values().all(|stream| stream.inflight.is_none()));
        let Some(PendingTransaction::LowPriority(retry)) = pending.pop().await else {
            panic!("overflow should enqueue one stateless retry");
        };
        assert_eq!(parsed_body(retry)[0][DUAL_SEND_UUID_FIELD], "overflow-uuid");

        let stream_count = sender.streams.len();
        let push_result = pending
            .push_low_priority(simple_log("ordinary retry", "ordinary-uuid"))
            .await
            .unwrap();
        assert!(!push_result.had_drops());
        assert_eq!(sender.streams.len(), stream_count);
    }

    #[tokio::test]
    async fn ambiguous_stream_failure_converts_exactly_one_payload() {
        let mut sender = sender("http://127.0.0.1:4317", "key-a");
        let _receivers = open_core_streams(&mut sender);
        let mut pending = pending(16, TEST_RETRY_BYTES);
        let telemetry = ComponentTelemetry::from_builder(&MetricsBuilder::default());

        assert!(sender
            .try_send_transaction(
                simple_log("ambiguous 42", "ambiguous-uuid"),
                &mut pending,
                &telemetry,
                TEST_DOMAIN
            )
            .await
            .is_ok());
        let (sender_id, stream_id) = sender
            .streams
            .iter()
            .find_map(|(sender_id, stream)| {
                stream
                    .inflight
                    .is_some()
                    .then_some((SenderId(*sender_id), stream.stream_id))
            })
            .expect("one sender should have an in-flight payload");

        sender
            .handle_event(
                StatefulEvent::Failed {
                    sender_id,
                    stream_id,
                    error: MetaString::from_static("test stream failure"),
                    failed_payload: None,
                },
                &mut pending,
                &telemetry,
                TEST_DOMAIN,
            )
            .await;

        assert!(sender.retained.is_empty());
        assert_eq!(sender.streams.len(), STATEFUL_SENDERS - 1);
        let Some(PendingTransaction::LowPriority(retry)) = pending.pop().await else {
            panic!("ambiguous failure should enqueue one stateless retry");
        };
        assert_eq!(parsed_body(retry)[0][DUAL_SEND_UUID_FIELD], "ambiguous-uuid");
        assert!(pending.pop().await.is_none());
    }

    #[tokio::test]
    async fn ack_and_failure_races_resolve_once() {
        let telemetry = ComponentTelemetry::from_builder(&MetricsBuilder::default());

        let mut ack_first = sender("http://127.0.0.1:4317", "key-a");
        let _ack_first_receivers = open_core_streams(&mut ack_first);
        let mut ack_first_pending = pending(16, TEST_RETRY_BYTES);
        assert!(ack_first
            .try_send_transaction(
                simple_log("ack first", "ack-first-uuid"),
                &mut ack_first_pending,
                &telemetry,
                TEST_DOMAIN,
            )
            .await
            .is_ok());
        let (sender_id, stream_id, batch_id) = ack_first
            .streams
            .iter()
            .find_map(|(sender_id, stream)| {
                stream
                    .inflight
                    .as_ref()
                    .map(|batch| (SenderId(*sender_id), stream.stream_id, batch.batch_id))
            })
            .unwrap();
        ack_first
            .handle_event(
                StatefulEvent::Ack {
                    sender_id,
                    stream_id,
                    batch_id,
                },
                &mut ack_first_pending,
                &telemetry,
                TEST_DOMAIN,
            )
            .await;
        ack_first
            .handle_event(
                StatefulEvent::Failed {
                    sender_id,
                    stream_id,
                    error: MetaString::from_static("failure after acknowledgement"),
                    failed_payload: None,
                },
                &mut ack_first_pending,
                &telemetry,
                TEST_DOMAIN,
            )
            .await;
        assert!(ack_first_pending.is_empty());

        let mut failure_first = sender("http://127.0.0.1:4318", "key-b");
        let _failure_first_receivers = open_core_streams(&mut failure_first);
        let mut failure_first_pending = pending(16, TEST_RETRY_BYTES);
        assert!(failure_first
            .try_send_transaction(
                simple_log("failure first", "failure-first-uuid"),
                &mut failure_first_pending,
                &telemetry,
                TEST_DOMAIN,
            )
            .await
            .is_ok());
        let (sender_id, stream_id, batch_id) = failure_first
            .streams
            .iter()
            .find_map(|(sender_id, stream)| {
                stream
                    .inflight
                    .as_ref()
                    .map(|batch| (SenderId(*sender_id), stream.stream_id, batch.batch_id))
            })
            .unwrap();
        failure_first
            .handle_event(
                StatefulEvent::Failed {
                    sender_id,
                    stream_id,
                    error: MetaString::from_static("failure before acknowledgement"),
                    failed_payload: None,
                },
                &mut failure_first_pending,
                &telemetry,
                TEST_DOMAIN,
            )
            .await;
        failure_first
            .handle_event(
                StatefulEvent::Ack {
                    sender_id,
                    stream_id,
                    batch_id,
                },
                &mut failure_first_pending,
                &telemetry,
                TEST_DOMAIN,
            )
            .await;
        assert!(failure_first_pending.pop().await.is_some());
        assert!(failure_first_pending.pop().await.is_none());
    }

    #[tokio::test]
    async fn shutdown_converts_retained_payload_before_flushing() {
        let mut sender = sender("http://127.0.0.1:4317", "key-a");
        let mut pending = pending(16, TEST_RETRY_BYTES);
        let telemetry = ComponentTelemetry::from_builder(&MetricsBuilder::default());
        assert!(sender
            .try_send_transaction(
                simple_log("shutdown 42", "shutdown-uuid"),
                &mut pending,
                &telemetry,
                TEST_DOMAIN
            )
            .await
            .is_ok());
        assert!(pending.is_empty());

        sender.shutdown(&mut pending, &telemetry, TEST_DOMAIN).await;

        assert!(sender.retained.is_empty());
        let Some(PendingTransaction::LowPriority(retry)) = pending.pop().await else {
            panic!("shutdown should enqueue one stateless retry");
        };
        assert_eq!(parsed_body(retry)[0][DUAL_SEND_UUID_FIELD], "shutdown-uuid");
    }

    #[tokio::test]
    async fn oversized_stateless_conversion_is_dropped_explicitly() {
        let mut sender = sender("http://127.0.0.1:4317", "key-a");
        let mut pending = pending(0, 1);
        let telemetry = ComponentTelemetry::from_builder(&MetricsBuilder::default());

        assert!(sender
            .try_send_transaction(
                simple_log("too large", "oversized-uuid"),
                &mut pending,
                &telemetry,
                TEST_DOMAIN
            )
            .await
            .is_ok());

        assert!(sender.retained.is_empty());
        assert!(sender.core.payload_sender(PayloadId(1)).is_none());
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn endpoint_and_api_key_state_are_independent() {
        let mut first = sender("http://127.0.0.1:4317", "key-a");
        let mut second = sender("http://127.0.0.1:4318", "key-b");
        let _first_receivers = open_core_streams(&mut first);
        let _second_receivers = open_core_streams(&mut second);
        let mut first_pending = pending(16, TEST_RETRY_BYTES);
        let mut second_pending = pending(16, TEST_RETRY_BYTES);
        let telemetry = ComponentTelemetry::from_builder(&MetricsBuilder::default());
        assert_ne!(first.endpoint.cached_api_key(), second.endpoint.cached_api_key());

        assert!(first
            .try_send_transaction(
                simple_log("first 42", "first-uuid"),
                &mut first_pending,
                &telemetry,
                TEST_DOMAIN
            )
            .await
            .is_ok());
        assert!(second
            .try_send_transaction(
                simple_log("second 42", "second-uuid"),
                &mut second_pending,
                &telemetry,
                TEST_DOMAIN
            )
            .await
            .is_ok());
        assert!(first.retained.contains_key(&1));
        assert!(second.retained.contains_key(&1));

        let (sender_id, stream_id, batch_id) = first
            .streams
            .iter()
            .find_map(|(sender_id, stream)| {
                stream
                    .inflight
                    .as_ref()
                    .map(|batch| (SenderId(*sender_id), stream.stream_id, batch.batch_id))
            })
            .unwrap();
        first
            .handle_event(
                StatefulEvent::Ack {
                    sender_id,
                    stream_id,
                    batch_id,
                },
                &mut first_pending,
                &telemetry,
                TEST_DOMAIN,
            )
            .await;
        assert!(first.retained.is_empty());
        assert!(second.retained.contains_key(&1));
        assert!(second_pending.is_empty());
    }

    #[tokio::test]
    async fn persisted_recovery_is_a_complete_stateless_transaction() {
        let original = simple_log("persisted 42", "persisted-uuid");
        let (retained, records) = retain(original);
        let recovered = retained
            .into_transaction(vec![foldspace_server::DecodedLog {
                message: String::from_utf8(records[0].body.clone()).unwrap(),
                timestamp_millis: records[0].timestamp_millis,
                uuid: records[0].uuid.clone(),
                ..foldspace_server::DecodedLog::default()
            }])
            .unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let root_path = temp_dir.path().to_path_buf();
        let queue_name = "stateful-disk-recovery";
        let persisted_args = |root_path: PathBuf| PersistedQueueArgs {
            root_path: root_path.clone(),
            max_on_disk_bytes: TEST_RETRY_BYTES,
            storage_max_disk_ratio: 0.8,
            disk_usage_retriever: Arc::new(DiskUsageRetrieverImpl::new(root_path)),
            max_age_days: 10,
        };
        let queue = RetryQueue::new(queue_name.to_owned(), TEST_RETRY_BYTES)
            .with_disk_persistence(persisted_args(root_path.clone()))
            .await
            .unwrap();
        let metrics_builder = MetricsBuilder::default();
        let shared = SharedTransactionQueueTelemetry::from_builder(&metrics_builder);
        let telemetry = TransactionQueueTelemetry::from_builder(&metrics_builder, TEST_DOMAIN, shared);
        let mut pending = PendingTransactions::new(0, queue, telemetry, MetaString::from_static(TEST_DOMAIN), 900);
        let push_result = pending.push_low_priority(recovered).await.unwrap();
        assert!(!push_result.had_drops());
        let flush_result = pending.flush().await.unwrap();
        assert!(!flush_result.had_drops());

        let mut restarted = RetryQueue::<Transaction<Bytes>>::new(queue_name.to_owned(), TEST_RETRY_BYTES)
            .with_disk_persistence(persisted_args(root_path))
            .await
            .unwrap();
        let recovered = restarted.pop().await.unwrap().expect("persisted retry should recover");
        let value = &parsed_body(recovered)[0];
        assert_eq!(value["message"], "persisted 42");
        assert_eq!(value[DUAL_SEND_UUID_FIELD], "persisted-uuid");
        assert_eq!(value["custom"], "preserved");
    }
}
