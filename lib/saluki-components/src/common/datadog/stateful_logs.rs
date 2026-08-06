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
    MultiStreamCore, PayloadDelivery, PayloadId, PayloadRecovery, ProtoBatchEncoder, SenderId, StatelessLogRecord,
    StreamError, StreamId, ZstdBatchCompressor,
};
use foldspace_patterns::ClusteringPatternExtractor;
#[cfg(test)]
use foldspace_server::ContentEncoding as FoldspaceContentEncoding;
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
    telemetry::{ComponentTelemetry, TransactionRetryCounters},
    transaction::{Metadata, Transaction, TransactionBody},
};

const LOGS_INTAKE_PATH: &str = "/api/v2/logs";
const STATEFUL_SENDERS: usize = 3;
const STATEFUL_BATCH_CAPACITY: usize = usize::MAX;
const STATEFUL_CHANNEL_CAPACITY: usize = 1;
const STATELESS_BATCH_MAX_BYTES: usize = 5 * 1024 * 1024;
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

struct RetainedStatelessRequest<B>
where
    B: Buf + Clone,
{
    metadata: Metadata,
    request: Request<TransactionBody<B>>,
    retry_counters: TransactionRetryCounters,
}

enum RetainedPayload<B>
where
    B: Buf + Clone,
{
    Stateful(RetainedRequest<B>),
    StatelessRetry(RetainedStatelessRequest<B>),
}

impl<B> RetainedPayload<B>
where
    B: Buf + Clone,
{
    fn metadata(&self) -> &Metadata {
        match self {
            Self::Stateful(retained) => &retained.metadata,
            Self::StatelessRetry(retained) => &retained.metadata,
        }
    }

    fn retry_counters(&self) -> Option<&TransactionRetryCounters> {
        match self {
            Self::Stateful(_) => None,
            Self::StatelessRetry(retained) => Some(&retained.retry_counters),
        }
    }
}

impl<B> RetainedRequest<B>
where
    B: Buf + Clone,
{
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

        transaction_from_values(self.metadata, self.request, values)
    }
}

impl<B> RetainedStatelessRequest<B>
where
    B: Buf + Clone,
{
    fn into_transaction(self, logs: Vec<foldspace_server::DecodedLog>) -> Result<Transaction<B>, GenericError> {
        let mut values = Vec::with_capacity(logs.len());
        for log in logs {
            let original_json = log
                .original_json
                .ok_or_else(|| generic_error!("Self-contained Foldspace log is missing its original JSON object."))?;
            let value: JsonValue = serde_json::from_slice(&original_json)
                .error_context("Failed to parse retained self-contained log JSON.")?;
            if !value.is_object() {
                return Err(generic_error!("Self-contained Foldspace log JSON is not an object."));
            }
            values.push(value);
        }

        let mut metadata = self.metadata;
        metadata.event_count = values.len();
        transaction_from_values(metadata, self.request, values)
    }
}

fn transaction_from_values<B>(
    metadata: Metadata, request: Request<TransactionBody<B>>, values: Vec<JsonValue>,
) -> Result<Transaction<B>, GenericError>
where
    B: Buf + Clone,
{
    let uncompressed = serde_json::to_vec(&values).error_context("Failed to encode recovered stateless logs.")?;
    let encoding = content_encoding(request.headers())?;
    let body = compress_body(&uncompressed, encoding)?;
    let mut request = request.map(|_| TransactionBody::from(body));
    if request.headers().contains_key(CONTENT_LENGTH) {
        let content_length = HeaderValue::from_str(&request.body().remaining().to_string())
            .error_context("Failed to update recovered logs content length.")?;
        request.headers_mut().insert(CONTENT_LENGTH, content_length);
    }
    Ok(Transaction::reassemble(metadata, request))
}

struct StatelessRecovery<B>
where
    B: Buf + Clone,
{
    recovery: PayloadRecovery,
    transaction: Transaction<B>,
    retry_counters: Option<TransactionRetryCounters>,
}

enum TransactionEncoding<B>
where
    B: Buf + Clone,
{
    Encoded {
        payload_id: PayloadId,
        effects: Vec<MplexEffect>,
    },
    Passthrough(Box<Transaction<B>>),
}

enum PreparedRecovery<B>
where
    B: Buf + Clone,
{
    Reconstructed(Box<StatelessRecovery<B>>),
    Dropped(PayloadRecovery),
    Resolved,
    Failed,
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
    let objects = parse_request_log_objects(request)?;
    let mut templates = Vec::with_capacity(objects.len());
    let mut records = Vec::with_capacity(objects.len());
    for mut object in objects {
        let record = log_record_from_object(&object, true)?;
        object.remove("message");
        object.remove(DUAL_SEND_UUID_FIELD);
        records.push(record);
        templates.push(LogTemplate { object });
    }
    Ok((templates, records))
}

fn parse_request_log_records<B>(request: &Request<TransactionBody<B>>) -> Result<Vec<StatelessLogRecord>, GenericError>
where
    B: Buf + Clone,
{
    parse_request_log_objects(request)?
        .iter()
        .map(|object| {
            let record = log_record_from_object(object, false)?;
            let original_json = serde_json::to_vec(object).error_context("Failed to preserve complete retry log.")?;
            Ok(StatelessLogRecord::new(record, original_json))
        })
        .collect()
}

fn parse_request_log_objects<B>(
    request: &Request<TransactionBody<B>>,
) -> Result<Vec<JsonMap<String, JsonValue>>, GenericError>
where
    B: Buf + Clone,
{
    let encoding = content_encoding(request.headers())?;
    let encoded = copy_body(request.body());
    let decoded = decompress_body(&encoded, encoding)?;
    let values: Vec<JsonValue> =
        serde_json::from_slice(&decoded).error_context("Failed to parse stateless logs transaction.")?;
    values
        .into_iter()
        .map(|value| match value {
            JsonValue::Object(object) => Ok(object),
            _ => Err(generic_error!("Logs transaction contained a non-object entry.")),
        })
        .collect()
}

fn log_record_from_object(object: &JsonMap<String, JsonValue>, generate_uuid: bool) -> Result<LogRecord, GenericError> {
    let timestamp_millis = log_timestamp_millis(object)?;
    let message = required_string(object, "message")?;
    let status = optional_string(object, "status")?;
    let hostname = optional_string(object, "hostname")?;
    let service = optional_string(object, "service")?;
    let source = optional_string(object, "ddsource")?;
    let tags = optional_string(object, "ddtags")?;
    let mut uuid = optional_string(object, DUAL_SEND_UUID_FIELD)?;
    if uuid.is_none() && generate_uuid {
        uuid = Some(Uuid::now_v7().to_string());
    }

    let mut record = LogRecord::new(message.into_bytes(), timestamp_millis);
    record.status = status;
    record.hostname = hostname;
    record.service = service;
    record.source = source;
    record.tags = tags
        .as_deref()
        .map(|tags| tags.split(',').map(ToOwned::to_owned).collect())
        .unwrap_or_default();
    record.uuid = uuid;
    Ok(record)
}

fn required_string(object: &JsonMap<String, JsonValue>, field: &str) -> Result<String, GenericError> {
    optional_string(object, field)?.ok_or_else(|| generic_error!("Log entry is missing string field '{}'.", field))
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
    max_stateless_batch_bytes: usize,
    streams: FastHashMap<u64, SenderStream>,
    retained: FastHashMap<u64, RetainedPayload<B>>,
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
            max_stateless_batch_bytes: STATELESS_BATCH_MAX_BYTES,
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
        let (payload_id, effects) = match self.encode_transaction(transaction) {
            TransactionEncoding::Encoded { payload_id, effects } => (payload_id, effects),
            TransactionEncoding::Passthrough(transaction) => return Err(*transaction),
        };

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

    pub(super) fn can_attempt_retry(&self) -> bool {
        !self.disabled && self.core.can_dispatch_stateless_logs()
    }

    pub(super) fn owns_retry_transaction(&self, transaction: &Transaction<B>) -> bool {
        transaction.request_uri().path() == LOGS_INTAKE_PATH
    }

    pub(super) async fn try_send_retry_transaction(
        &mut self, transaction: Transaction<B>, retry_counters: TransactionRetryCounters,
        component_telemetry: &ComponentTelemetry, endpoint_domain: &str,
    ) -> Result<(), Transaction<B>> {
        if !self.can_attempt_retry() || transaction.request_uri().path() != LOGS_INTAKE_PATH {
            return Err(transaction);
        }
        let records = match parse_request_log_records(transaction.request()) {
            Ok(records) if !records.is_empty() => records,
            Ok(_) | Err(_) => {
                component_telemetry.track_permanently_failed_transaction(transaction.metadata(), None, endpoint_domain);
                return Ok(());
            }
        };
        let log_count = records.len();
        let dispatch = match self
            .core
            .try_dispatch_stateless_logs(records, self.max_stateless_batch_bytes)
        {
            Ok(dispatched) => dispatched,
            Err(rejected) => {
                drop(rejected.into_logs());
                return Err(transaction);
            }
        };
        let (payload_id, effects, dropped_logs) = dispatch.into_parts();
        if dropped_logs > 0 {
            self.telemetry.oversized.increment(dropped_logs as u64);
            let mut dropped_metadata = transaction.metadata().clone();
            dropped_metadata.event_count = dropped_logs;
            dropped_metadata.data_point_count = 0;
            component_telemetry.track_permanently_failed_transaction(&dropped_metadata, None, endpoint_domain);
        }
        let Some(payload_id) = payload_id else {
            return Ok(());
        };
        let sends_on_existing_stream = !effects.is_empty()
            && effects.iter().all(|effect| match effect {
                MplexEffect::SendBatch { sender_id, batch, .. } => self
                    .streams
                    .get(&sender_id.get())
                    .is_some_and(|stream| stream.stream_id == batch.stream),
                _ => false,
            });
        if !sends_on_existing_stream {
            error!(
                payload_id = payload_id.get(),
                "Foldspace accepted a stateless retry without an existing adapter stream."
            );
            self.disabled = true;
            if let Some(sender_id) = self.core.payload_sender(payload_id) {
                if let Ok(recovery) = self
                    .core
                    .begin_recovery(sender_id, payload_id, PayloadDelivery::NotSent)
                {
                    let effects = self.core.complete_recovery(recovery);
                    self.execute(effects).await;
                }
            }
            return Err(transaction);
        }
        let (mut metadata, request) = transaction.into_parts();
        metadata.event_count = log_count.saturating_sub(dropped_logs);
        self.retained.insert(
            payload_id.get(),
            RetainedPayload::StatelessRetry(RetainedStatelessRequest {
                metadata,
                request: request.map(|_| TransactionBody::from(Vec::new())),
                retry_counters,
            }),
        );
        self.execute(effects).await;
        Ok(())
    }

    fn encode_transaction(&mut self, transaction: Transaction<B>) -> TransactionEncoding<B> {
        let (metadata, request) = transaction.into_parts();
        let (templates, records) = match parse_request_logs(&request) {
            Ok((templates, records)) if !records.is_empty() => (templates, records),
            Ok(_) | Err(_) => {
                return TransactionEncoding::Passthrough(Box::new(Transaction::reassemble(metadata, request)))
            }
        };
        let retained = RetainedRequest {
            metadata,
            request: request.map(|_| TransactionBody::from(Vec::new())),
            templates,
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
        self.retained
            .insert(payload_id.get(), RetainedPayload::Stateful(retained));
        TransactionEncoding::Encoded { payload_id, effects }
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
                if let Some(payload_id) =
                    payload_id.filter(|payload_id| self.core.payload_sender(*payload_id).is_none())
                {
                    if let Some(retained) = self.retained.remove(&payload_id.get()) {
                        component_telemetry.track_successful_transaction(retained.metadata(), endpoint_domain);
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
        let recovered =
            match self.prepare_stateless_recovery(payload_id, delivery, component_telemetry, endpoint_domain) {
                PreparedRecovery::Reconstructed(recovered) => *recovered,
                PreparedRecovery::Dropped(recovery) => {
                    let effects = self.core.complete_recovery(recovery);
                    self.execute(effects).await;
                    return true;
                }
                PreparedRecovery::Resolved => return true,
                PreparedRecovery::Failed => return false,
            };
        let metadata = recovered.transaction.metadata().clone();
        match pending.push_low_priority(recovered.transaction).await {
            Ok(push_result) => {
                if delivery == PayloadDelivery::NotSent {
                    if let Some(counters) = recovered.retry_counters.as_ref() {
                        counters.increment_requeued();
                    }
                }
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
        let effects = self.core.complete_recovery(recovered.recovery);
        self.execute(effects).await;
        true
    }

    fn prepare_stateless_recovery(
        &mut self, payload_id: PayloadId, delivery: PayloadDelivery, component_telemetry: &ComponentTelemetry,
        endpoint_domain: &str,
    ) -> PreparedRecovery<B> {
        let Some(sender_id) = self.core.payload_sender(payload_id) else {
            return PreparedRecovery::Resolved;
        };
        let recovery = match self.core.begin_recovery(sender_id, payload_id, delivery) {
            Ok(recovery) => recovery,
            Err(error) => {
                self.disabled = true;
                self.telemetry.conversion_errors.increment(1);
                error!(payload_id = payload_id.get(), %error, "Failed to begin Foldspace stateless recovery.");
                return PreparedRecovery::Failed;
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
            return PreparedRecovery::Dropped(recovery);
        };
        let metadata = retained.metadata().clone();
        let retry_counters = retained.retry_counters().cloned();
        let transaction = match retained {
            RetainedPayload::Stateful(retained) => StatefulLogsDecoder::new()
                .decode_recovery(&recovery)
                .error_context("Failed to decode retained Foldspace payload.")
                .and_then(|logs| retained.into_transaction(logs)),
            RetainedPayload::StatelessRetry(retained) => StatefulLogsDecoder::new()
                .decode_recovery(&recovery)
                .error_context("Failed to decode remaining self-contained Foldspace retry batches.")
                .and_then(|logs| retained.into_transaction(logs)),
        };
        let transaction = match transaction {
            Ok(transaction) => transaction,
            Err(error) => {
                self.telemetry.conversion_errors.increment(1);
                component_telemetry.track_permanently_failed_transaction(&metadata, None, endpoint_domain);
                error!(payload_id = payload_id.get(), %error, "Failed to reconstruct Foldspace payload; dropping it.");
                return PreparedRecovery::Dropped(recovery);
            }
        };
        PreparedRecovery::Reconstructed(Box::new(StatelessRecovery {
            recovery,
            transaction,
            retry_counters,
        }))
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
                if let Some(counters) = payload_id
                    .and_then(|payload_id| self.retained.get(&payload_id.get()))
                    .and_then(RetainedPayload::retry_counters)
                {
                    counters.increment_retries();
                }
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
    use saluki_metrics::test::TestRecorder;

    use super::*;
    use crate::common::datadog::{
        io::PendingTransaction,
        telemetry::{SharedTransactionQueueTelemetry, TransactionQueueTelemetry, TransactionRetryTelemetry},
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
        let (metadata, request) = transaction.into_parts();
        let (templates, records) = parse_request_logs(&request).unwrap();
        let retained = RetainedRequest {
            metadata,
            request: request.map(|_| TransactionBody::from(Vec::new())),
            templates,
        };
        (retained, records)
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

    fn retry_counters(metrics_builder: &MetricsBuilder) -> TransactionRetryCounters {
        TransactionRetryTelemetry::from_builder(metrics_builder, TEST_DOMAIN).counters_for(LOGS_INTAKE_PATH)
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

    fn multiple_logs(messages: &[&str]) -> Transaction<Bytes> {
        let logs = messages
            .iter()
            .enumerate()
            .map(|(index, message)| {
                serde_json::json!({
                    "message": message,
                    "@timestamp": "2026-08-03T12:00:00.000Z",
                    "dual-send-uuid": format!("multi-{index}"),
                    "custom": "preserved"
                })
            })
            .collect::<Vec<_>>();
        let request = Request::builder()
            .method("POST")
            .uri(LOGS_INTAKE_PATH)
            .header(CONTENT_TYPE, "application/json")
            .header("x-test-routing", "route-a")
            .body(Bytes::from(serde_json::to_vec(&logs).unwrap()))
            .unwrap();
        Transaction::from_original(Metadata::from_event_and_data_point_count(logs.len(), 0), request)
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
            original_json: None,
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
    async fn converted_retry_uses_existing_stream_as_a_self_contained_batch() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let metrics_builder = MetricsBuilder::default();
        let mut sender = sender("http://127.0.0.1:4317", "key-a");
        let mut receivers = open_core_streams(&mut sender);
        let original_streams = sender
            .streams
            .iter()
            .map(|(sender_id, stream)| (*sender_id, stream.stream_id))
            .collect::<FastHashMap<_, _>>();
        let mut pending = pending(0, TEST_RETRY_BYTES);
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);

        assert!(sender
            .try_send_transaction(
                simple_log("retry on grpc 42", "grpc-retry-uuid"),
                &mut pending,
                &telemetry,
                TEST_DOMAIN,
            )
            .await
            .is_ok());
        assert!(sender.retained.is_empty());
        let retry = pending.pop_low_priority().await.unwrap();

        assert!(sender
            .try_send_retry_transaction(retry, retry_counters(&metrics_builder), &telemetry, TEST_DOMAIN)
            .await
            .is_ok());

        assert!(!sender.retained.contains_key(&1));
        assert!(sender.retained.contains_key(&2));
        assert_eq!(sender.streams.len(), original_streams.len());
        assert!(sender
            .streams
            .iter()
            .all(|(sender_id, stream)| original_streams.get(sender_id) == Some(&stream.stream_id)));
        assert!(sender
            .streams
            .values()
            .any(|stream| stream.inflight.as_ref().and_then(|batch| batch.payload_id) == Some(PayloadId(2))));
        let proto = receivers
            .iter_mut()
            .find_map(|receiver| receiver.try_recv().ok())
            .expect("the stateless retry should be written to an existing stream");
        let mut decoder = StatefulLogsDecoder::new();
        let logs = decoder.decode_batch(&proto, FoldspaceContentEncoding::Zstd).unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].message, "retry on grpc 42");
        assert_eq!(logs[0].uuid.as_deref(), Some("grpc-retry-uuid"));
        let original: JsonValue = serde_json::from_slice(logs[0].original_json.as_deref().unwrap()).unwrap();
        assert_eq!(original["custom"], "preserved");
        assert_eq!(decoder.state_bytes(), 0);
        assert_eq!(decoder.stats().state_changes, 0);
        let tags = &[("domain", TEST_DOMAIN), ("endpoint", LOGS_INTAKE_PATH)];
        assert_eq!(recorder.counter(("network_http_requests_retries_total", tags)), Some(1));
        assert_eq!(
            recorder.counter(("network_http_requests_requeued_total", tags)),
            Some(0)
        );
    }

    #[tokio::test]
    async fn stateless_retry_batches_are_sent_sequentially_on_one_existing_stream() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let metrics_builder = MetricsBuilder::default();
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);
        let mut sender = sender("http://127.0.0.1:4317", "key-a");
        sender.max_stateless_batch_bytes = 700;
        let mut receivers = open_core_streams(&mut sender);
        let original_streams = sender
            .streams
            .iter()
            .map(|(sender_id, stream)| (*sender_id, stream.stream_id))
            .collect::<FastHashMap<_, _>>();
        let first_message = "a".repeat(200);
        let second_message = "b".repeat(200);

        assert!(sender
            .try_send_retry_transaction(
                multiple_logs(&[&first_message, &second_message]),
                retry_counters(&metrics_builder),
                &telemetry,
                TEST_DOMAIN,
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
            .expect("the first retry batch should be inflight");
        let receiver = &mut receivers[sender_id.get() as usize];
        let first_batch = receiver.recv().await.unwrap();
        let first_bytes = zstd::stream::decode_all(first_batch.data.as_slice()).unwrap();
        assert!(first_bytes.len() <= sender.max_stateless_batch_bytes);
        let mut decoder = StatefulLogsDecoder::new();
        let first_logs = decoder
            .decode_batch(&first_batch, FoldspaceContentEncoding::Zstd)
            .unwrap();
        assert_eq!(first_logs.len(), 1);
        assert_eq!(first_logs[0].message, first_message);

        let mut pending = pending(16, TEST_RETRY_BYTES);
        sender
            .handle_event(
                StatefulEvent::Ack {
                    sender_id,
                    stream_id,
                    batch_id: u64::from(first_batch.batch_id),
                },
                &mut pending,
                &telemetry,
                TEST_DOMAIN,
            )
            .await;
        assert_eq!(sender.streams.len(), original_streams.len());
        assert!(sender
            .streams
            .iter()
            .all(|(id, stream)| original_streams.get(id) == Some(&stream.stream_id)));
        assert_eq!(sender.retained.len(), 1);

        let second_batch = receiver.recv().await.unwrap();
        let second_bytes = zstd::stream::decode_all(second_batch.data.as_slice()).unwrap();
        assert!(second_bytes.len() <= sender.max_stateless_batch_bytes);
        let second_logs = decoder
            .decode_batch(&second_batch, FoldspaceContentEncoding::Zstd)
            .unwrap();
        assert_eq!(second_logs.len(), 1);
        assert_eq!(second_logs[0].message, second_message);
        sender
            .handle_event(
                StatefulEvent::Ack {
                    sender_id,
                    stream_id,
                    batch_id: u64::from(second_batch.batch_id),
                },
                &mut pending,
                &telemetry,
                TEST_DOMAIN,
            )
            .await;

        assert!(sender.retained.is_empty());
        assert!(pending.is_empty());
        let tags = &[("domain", TEST_DOMAIN), ("endpoint", LOGS_INTAKE_PATH)];
        assert_eq!(recorder.counter(("network_http_requests_retries_total", tags)), Some(2));
    }

    #[tokio::test]
    async fn split_retry_failure_requeues_only_unacknowledged_logs() {
        let metrics_builder = MetricsBuilder::default();
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);
        let mut sender = sender("http://127.0.0.1:4317", "key-a");
        sender.max_stateless_batch_bytes = 700;
        let mut receivers = open_core_streams(&mut sender);
        let first_message = "a".repeat(200);
        let second_message = "b".repeat(200);

        assert!(sender
            .try_send_retry_transaction(
                multiple_logs(&[&first_message, &second_message]),
                retry_counters(&metrics_builder),
                &telemetry,
                TEST_DOMAIN,
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
            .unwrap();
        let receiver = &mut receivers[sender_id.get() as usize];
        let first_batch = receiver.recv().await.unwrap();
        let mut pending = pending(16, TEST_RETRY_BYTES);
        sender
            .handle_event(
                StatefulEvent::Ack {
                    sender_id,
                    stream_id,
                    batch_id: u64::from(first_batch.batch_id),
                },
                &mut pending,
                &telemetry,
                TEST_DOMAIN,
            )
            .await;
        let _second_batch = receiver.recv().await.unwrap();

        sender
            .handle_event(
                StatefulEvent::Failed {
                    sender_id,
                    stream_id,
                    error: MetaString::from_static("split retry transport failure"),
                    failed_payload: None,
                },
                &mut pending,
                &telemetry,
                TEST_DOMAIN,
            )
            .await;

        let retry = pending.pop_low_priority().await.unwrap();
        assert_eq!(retry.metadata().event_count, 1);
        let logs = parsed_body(retry);
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0]["message"], second_message);
    }

    #[tokio::test]
    async fn single_log_larger_than_a_stateless_batch_is_dropped() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let metrics_builder = MetricsBuilder::default();
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);
        let mut sender = sender("http://127.0.0.1:4317", "key-a");
        sender.max_stateless_batch_bytes = 1;
        let receivers = open_core_streams(&mut sender);

        assert!(sender
            .try_send_retry_transaction(
                simple_log("too large", "too-large-uuid"),
                retry_counters(&metrics_builder),
                &telemetry,
                TEST_DOMAIN,
            )
            .await
            .is_ok());

        assert!(sender.retained.is_empty());
        assert!(receivers.iter().all(|receiver| receiver.is_empty()));
        assert_eq!(
            recorder.counter(("stateful_logs_stateless_oversized_total", &[("domain", TEST_DOMAIN)])),
            Some(1)
        );
    }

    #[tokio::test]
    async fn oversized_log_is_dropped_while_fitting_logs_are_sent() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let metrics_builder = MetricsBuilder::default();
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);
        let mut sender = sender("http://127.0.0.1:4317", "key-a");
        sender.max_stateless_batch_bytes = 700;
        let mut receivers = open_core_streams(&mut sender);
        let oversized = "x".repeat(2_000);

        assert!(sender
            .try_send_retry_transaction(
                multiple_logs(&["fits", &oversized]),
                retry_counters(&metrics_builder),
                &telemetry,
                TEST_DOMAIN,
            )
            .await
            .is_ok());

        let sender_id = sender
            .streams
            .iter()
            .find_map(|(sender_id, stream)| stream.inflight.is_some().then_some(SenderId(*sender_id)))
            .expect("the fitting log should be inflight");
        let batch = receivers[sender_id.get() as usize].recv().await.unwrap();
        let decompressed = zstd::stream::decode_all(batch.data.as_slice()).unwrap();
        assert!(decompressed.len() <= sender.max_stateless_batch_bytes);
        let logs = StatefulLogsDecoder::new()
            .decode_batch(&batch, FoldspaceContentEncoding::Zstd)
            .unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].message, "fits");
        assert_eq!(sender.retained.values().next().unwrap().metadata().event_count, 1);
        assert_eq!(
            recorder.counter(("stateful_logs_stateless_oversized_total", &[("domain", TEST_DOMAIN)])),
            Some(1)
        );
    }

    #[tokio::test]
    async fn retry_without_an_open_stream_remains_stateless_without_mutation() {
        let metrics_builder = MetricsBuilder::default();
        let mut sender = sender("http://127.0.0.1:4317", "key-a");
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);
        let transaction = simple_log("queued retry 42", "queued-retry-uuid");

        let transaction = sender
            .try_send_retry_transaction(transaction, retry_counters(&metrics_builder), &telemetry, TEST_DOMAIN)
            .await
            .expect_err("a retry without an open stream should remain queued");

        assert!(sender.streams.is_empty());
        assert!(sender.retained.is_empty());
        assert_eq!(parsed_body(transaction)[0][DUAL_SEND_UUID_FIELD], "queued-retry-uuid");
    }

    #[tokio::test]
    async fn retry_without_immediate_stream_capacity_remains_queued_without_rotating_streams() {
        let metrics_builder = MetricsBuilder::default();
        let mut sender = sender("http://127.0.0.1:4317", "key-a");
        let _receivers = open_core_streams(&mut sender);
        let original_streams = sender
            .streams
            .iter()
            .map(|(sender_id, stream)| (*sender_id, stream.stream_id))
            .collect::<FastHashMap<_, _>>();
        let mut pending = pending(16, TEST_RETRY_BYTES);
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);
        for index in 0..STATEFUL_SENDERS {
            assert!(sender
                .try_send_transaction(
                    simple_log(&format!("busy stream {index}"), &format!("busy-{index}")),
                    &mut pending,
                    &telemetry,
                    TEST_DOMAIN,
                )
                .await
                .is_ok());
        }

        let transaction = sender
            .try_send_retry_transaction(
                simple_log("busy retry 42", "busy-retry-uuid"),
                retry_counters(&metrics_builder),
                &telemetry,
                TEST_DOMAIN,
            )
            .await
            .expect_err("a retry should remain queued when every stream is busy");

        assert_eq!(sender.retained.len(), STATEFUL_SENDERS);
        assert_eq!(sender.streams.len(), original_streams.len());
        assert!(sender
            .streams
            .iter()
            .all(|(sender_id, stream)| original_streams.get(sender_id) == Some(&stream.stream_id)));
        assert_eq!(parsed_body(transaction)[0][DUAL_SEND_UUID_FIELD], "busy-retry-uuid");
    }

    #[tokio::test]
    async fn grpc_retry_failed_before_transmission_requeues_complete_stateless_logs() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let metrics_builder = MetricsBuilder::default();
        let mut sender = sender("http://127.0.0.1:4317", "key-a");
        let mut receivers = open_core_streams(&mut sender);
        drop(receivers.remove(0));
        let mut pending = pending(16, TEST_RETRY_BYTES);
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);

        assert!(sender
            .try_send_retry_transaction(
                simple_log("grpc failed 42", "failed-grpc-retry-uuid"),
                retry_counters(&metrics_builder),
                &telemetry,
                TEST_DOMAIN,
            )
            .await
            .is_ok());
        let event = sender.next_event().await.unwrap();
        sender.handle_event(event, &mut pending, &telemetry, TEST_DOMAIN).await;

        let retry = pending.pop_low_priority().await.unwrap();
        let value = &parsed_body(retry)[0];
        assert_eq!(value["message"], "grpc failed 42");
        assert_eq!(value[DUAL_SEND_UUID_FIELD], "failed-grpc-retry-uuid");
        let tags = &[("domain", TEST_DOMAIN), ("endpoint", LOGS_INTAKE_PATH)];
        assert_eq!(recorder.counter(("network_http_requests_retries_total", tags)), Some(0));
        assert_eq!(
            recorder.counter(("network_http_requests_requeued_total", tags)),
            Some(1)
        );
    }

    #[tokio::test]
    async fn transmitted_grpc_retry_failure_requeues_complete_stateless_logs() {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let metrics_builder = MetricsBuilder::default();
        let mut sender = sender("http://127.0.0.1:4317", "key-a");
        let _receivers = open_core_streams(&mut sender);
        let mut pending = pending(16, TEST_RETRY_BYTES);
        let telemetry = ComponentTelemetry::from_builder(&metrics_builder);

        assert!(sender
            .try_send_retry_transaction(
                simple_log("ambiguous retry 42", "ambiguous-retry-uuid"),
                retry_counters(&metrics_builder),
                &telemetry,
                TEST_DOMAIN,
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
            .unwrap();
        sender
            .handle_event(
                StatefulEvent::Failed {
                    sender_id,
                    stream_id,
                    error: MetaString::from_static("ambiguous retry transport failure"),
                    failed_payload: None,
                },
                &mut pending,
                &telemetry,
                TEST_DOMAIN,
            )
            .await;

        let retry = pending.pop_low_priority().await.unwrap();
        let value = &parsed_body(retry)[0];
        assert_eq!(value["message"], "ambiguous retry 42");
        assert_eq!(value[DUAL_SEND_UUID_FIELD], "ambiguous-retry-uuid");
        let tags = &[("domain", TEST_DOMAIN), ("endpoint", LOGS_INTAKE_PATH)];
        assert_eq!(recorder.counter(("network_http_requests_retries_total", tags)), Some(1));
        assert_eq!(
            recorder.counter(("network_http_requests_requeued_total", tags)),
            Some(0)
        );
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
        let value = &parsed_body(recovered.clone())[0];
        assert_eq!(value["message"], "persisted 42");
        assert_eq!(value[DUAL_SEND_UUID_FIELD], "persisted-uuid");
        assert_eq!(value["custom"], "preserved");

        let metrics_builder = MetricsBuilder::default();
        let mut restarted_sender = sender("http://127.0.0.1:4317", "key-a");
        let _receivers = open_core_streams(&mut restarted_sender);
        assert!(restarted_sender
            .try_send_retry_transaction(
                recovered,
                retry_counters(&metrics_builder),
                &ComponentTelemetry::from_builder(&metrics_builder),
                TEST_DOMAIN,
            )
            .await
            .is_ok());
        assert!(restarted_sender.retained.contains_key(&1));
    }
}
