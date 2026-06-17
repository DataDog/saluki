use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, Mutex},
};

use datadog_protos::stateful::{
    batch_status, datum, dynamic_value, stateful_logs_service_server::StatefulLogsService,
    stateful_logs_service_server::StatefulLogsServiceServer, BatchStatus, Datum, DatumSequence, DeltaEncodingSync,
    DynamicValue, FlatLog, StatefulBatch as ProtoStatefulBatch,
};
use foldspace::{
    BatchStrategy, DefaultBatchEncoder, Endpoint, LogRecord, NoopBatchCompressor, ProtoBatchEncoder, SenderConfig,
    StatefulLogTranslator, StatefulMessage, StreamWorkerRunner, TonicStatefulTransport,
};
use futures::Stream;
use prost::Message as _;
use tokio::net::TcpListener;
use tonic::{Request, Response, Status};

#[tokio::test]
async fn file_backed_logs_round_trip_through_stateful_grpc_protocol() {
    let input = read_fixture_lines("stateful_logs.txt");
    let service = DecodingStatefulLogsService::default();
    let receiver = service.receiver.clone();
    let endpoint = spawn_stateful_receiver(service).await;

    let transport = TonicStatefulTransport::new(ProtoBatchEncoder::new(NoopBatchCompressor));
    let config = SenderConfig {
        stream_lifetime: std::time::Duration::from_secs(60),
        ..SenderConfig::default()
    };
    let (runner, handle) = StreamWorkerRunner::with_channel(Endpoint::new(endpoint), config, transport, 8);

    let mut translator = StatefulLogTranslator::new();
    let mut batcher = BatchStrategy::new(DefaultBatchEncoder, 7);
    for (index, line) in input.iter().enumerate() {
        let mut record = LogRecord::new(line.as_bytes().to_vec(), 1_700_000_000_000 + index as i64);
        record.status = Some("info".to_string());
        record.service = Some("checkout".to_string());
        record.source = Some("foldspace-fixture".to_string());
        record.hostname = Some("test-host".to_string());
        record.tags = vec!["env:test".to_string(), "team:logs".to_string()];
        record.uuid = Some(format!("fixture-{index}"));

        for message in translator.translate(&record) {
            push_or_flush(&mut batcher, &handle, message).await;
        }
    }

    if let Some(payload) = batcher.flush() {
        handle.send(payload).await.unwrap();
    }
    drop(handle);

    runner.run().await.unwrap();

    let receiver = receiver.lock().unwrap();
    assert_eq!(receiver.decoded_messages(), input);
    assert!(
        receiver.batch_ids.len() > 1,
        "small batch capacity should exercise multiple batches"
    );
    assert!(
        receiver.dict_entry_define_count > 0,
        "fixture metadata should exercise dictionary state"
    );
    assert!(receiver.decoded.iter().all(|log| log.status.as_deref() == Some("info")));
    assert!(receiver
        .decoded
        .iter()
        .all(|log| log.service.as_deref() == Some("checkout")));
    assert!(receiver
        .decoded
        .iter()
        .all(|log| log.tags.as_deref() == Some("env:test,team:logs,hostname:test-host,ddsource:foldspace-fixture")));
    assert!(receiver
        .decoded
        .iter()
        .enumerate()
        .all(|(index, log)| log.uuid.as_deref() == Some(format!("fixture-{index}").as_str())));
    let decoded_timestamps = receiver.decoded.iter().map(|log| log.timestamp).collect::<Vec<_>>();
    let expected_timestamps = (0..input.len())
        .map(|index| 1_700_000_000_000 + index as i64)
        .collect::<Vec<_>>();
    assert_eq!(decoded_timestamps, expected_timestamps);
}

fn read_fixture_lines(name: &str) -> Vec<String> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name);
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

async fn push_or_flush(
    batcher: &mut BatchStrategy<DefaultBatchEncoder>, handle: &foldspace::StreamWorkerRunnerHandle,
    message: StatefulMessage,
) {
    if let Err(message) = batcher.push(message) {
        let payload = batcher.flush().expect("full batcher should flush a payload");
        handle.send(payload).await.unwrap();
        batcher.push(message).unwrap();
    }
}

#[derive(Clone, Default)]
struct DecodingStatefulLogsService {
    receiver: Arc<Mutex<MockStatefulReceiver>>,
}

#[tonic::async_trait]
impl StatefulLogsService for DecodingStatefulLogsService {
    type LogsStreamStream = Pin<Box<dyn Stream<Item = Result<BatchStatus, Status>> + Send>>;

    async fn logs_stream(
        &self, request: Request<tonic::Streaming<ProtoStatefulBatch>>,
    ) -> Result<Response<Self::LogsStreamStream>, Status> {
        let receiver = self.receiver.clone();
        let mut inbound = request.into_inner();
        let outbound = async_stream::try_stream! {
            while let Some(batch) = inbound.message().await? {
                let batch_id = batch.batch_id;
                let decode_result = {
                    let mut receiver = receiver.lock().unwrap();
                    receiver.decode_batch(&batch)
                };
                decode_result.map_err(Status::invalid_argument)?;
                yield BatchStatus {
                    batch_id,
                    status: batch_status::Status::Ok as i32,
                };
            }
        };

        Ok(Response::new(Box::pin(outbound)))
    }
}

async fn spawn_stateful_receiver(service: DecodingStatefulLogsService) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = async_stream::stream! {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => yield Ok::<_, std::io::Error>(stream),
                Err(err) => {
                    yield Err(err);
                    break;
                }
            }
        }
    };

    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(StatefulLogsServiceServer::new(service))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });

    format!("http://{addr}")
}

#[derive(Debug, Default)]
struct MockStatefulReceiver {
    patterns: HashMap<u64, Pattern>,
    dictionary: HashMap<u64, String>,
    decoded: Vec<DecodedLog>,
    batch_ids: Vec<u32>,
    pattern_define_count: usize,
    dict_entry_define_count: usize,
}

impl MockStatefulReceiver {
    fn decoded_messages(&self) -> Vec<String> {
        self.decoded.iter().map(|log| log.message.clone()).collect()
    }

    fn decode_batch(&mut self, batch: &ProtoStatefulBatch) -> Result<(), String> {
        self.batch_ids.push(batch.batch_id);
        let sequence = DatumSequence::decode(batch.data.as_slice()).map_err(|err| err.to_string())?;
        // Match logs-backend's StatefulConverter: stream state persists, but delta state is batch-scoped.
        let mut delta = DeltaState::default();

        for datum in sequence.data {
            self.apply_datum(datum, &mut delta)?;
        }

        Ok(())
    }

    fn apply_datum(&mut self, datum: Datum, delta: &mut DeltaState) -> Result<(), String> {
        match datum.data.ok_or_else(|| "datum missing data".to_string())? {
            datum::Data::PatternDefine(pattern) => {
                if pattern.param_count as usize != pattern.pos_list.len() {
                    return Err("pattern define param_count does not match pos_list length".to_string());
                }
                self.pattern_define_count += 1;
                delta.resolve_pattern_id(pattern.pattern_id);
                self.patterns
                    .insert(pattern.pattern_id, Pattern::new(pattern.template, pattern.pos_list));
            }
            datum::Data::PatternDelete(pattern) => {
                self.patterns.remove(&pattern.pattern_id);
            }
            datum::Data::DictEntryDefine(entry) => {
                self.dict_entry_define_count += 1;
                self.dictionary.insert(entry.id, entry.value);
            }
            datum::Data::DictEntryDelete(entry) => {
                self.dictionary.remove(&entry.id);
            }
            datum::Data::DeltaEncodingSync(sync) => {
                delta.sync(sync);
            }
            datum::Data::FlatLog(log) => {
                let decoded = self.decode_flat_log(log, delta)?;
                self.decoded.push(decoded);
            }
            datum::Data::Logs(_) => {
                return Err("Log datum decoding is not implemented in the foldspace mock receiver yet".to_string());
            }
            datum::Data::JsonSchemaDefine(_) | datum::Data::JsonSchemaDelete(_) => {}
        }

        Ok(())
    }

    fn decode_flat_log(&self, log: FlatLog, delta: &mut DeltaState) -> Result<DecodedLog, String> {
        let timestamp = delta.resolve_timestamp(log.timestamp);
        let status_id = delta.resolve_status(log.status);
        let service_id = delta.resolve_service(log.service);
        let tags_id = delta.resolve_tags(log.tags);
        delta.resolve_json_schema_id(log.json_schema_id);

        let message = if log.raw_log.is_empty() {
            let pattern_id = delta.resolve_pattern_id(log.pattern_id);
            let pattern = self
                .patterns
                .get(&pattern_id)
                .ok_or_else(|| format!("flat log referenced unknown pattern id {pattern_id}"))?;
            let values = log
                .dynamic_values
                .iter()
                .map(|value| self.dynamic_value(value))
                .collect::<Result<Vec<_>, _>>()?;
            pattern.build_log_from_values(&values)?
        } else {
            log.raw_log
        };

        Ok(DecodedLog {
            message,
            timestamp,
            status: self.optional_dictionary_value(status_id)?,
            service: self.optional_dictionary_value(service_id)?,
            tags: self.optional_dictionary_value(tags_id)?,
            uuid: log.uuid,
        })
    }

    fn dynamic_value(&self, value: &DynamicValue) -> Result<String, String> {
        match value
            .value
            .as_ref()
            .ok_or_else(|| "dynamic value missing value".to_string())?
        {
            dynamic_value::Value::IntValue(value) => Ok(value.to_string()),
            dynamic_value::Value::FloatValue(value) => Ok(value.to_string()),
            dynamic_value::Value::BoolValue(value) => Ok(value.to_string()),
            dynamic_value::Value::StringValue(value) => Ok(value.clone()),
            dynamic_value::Value::DictIndex(id) => self
                .dictionary
                .get(id)
                .cloned()
                .ok_or_else(|| format!("dynamic value referenced unknown dictionary id {id}")),
            dynamic_value::Value::RawJsonValue(value) => {
                String::from_utf8(value.clone()).map_err(|err| format!("raw JSON dynamic value was not UTF-8: {err}"))
            }
        }
    }

    fn optional_dictionary_value(&self, id: u64) -> Result<Option<String>, String> {
        match id {
            0 | 1 => Ok(None),
            id => self
                .dictionary
                .get(&id)
                .cloned()
                .map(Some)
                .ok_or_else(|| format!("field referenced unknown dictionary id {id}")),
        }
    }
}

#[derive(Debug, Default)]
struct DeltaState {
    last_timestamp: Option<i64>,
    last_pattern_id: u64,
    last_status: u64,
    last_service: u64,
    last_tags: u64,
    last_json_schema_id: u64,
}

impl DeltaState {
    fn sync(&mut self, sync: DeltaEncodingSync) {
        if sync.timestamp != 0 {
            self.last_timestamp = Some(sync.timestamp as i64);
        }
        if sync.pattern_id != 0 {
            self.last_pattern_id = sync.pattern_id;
        }
        if sync.status != 0 {
            self.last_status = sync.status;
        }
        if sync.service != 0 {
            self.last_service = sync.service;
        }
        if sync.flat_log_tags != 0 {
            self.last_tags = sync.flat_log_tags;
        }
        if sync.json_schema_id != 0 {
            self.last_json_schema_id = sync.json_schema_id;
        }
    }

    fn resolve_timestamp(&mut self, value: i64) -> i64 {
        let resolved = match self.last_timestamp {
            Some(last) => last + value,
            None => value,
        };
        self.last_timestamp = Some(resolved);
        resolved
    }

    fn resolve_pattern_id(&mut self, value: u64) -> u64 {
        resolve_delta_value(&mut self.last_pattern_id, value)
    }

    fn resolve_status(&mut self, value: u64) -> u64 {
        resolve_delta_value(&mut self.last_status, value)
    }

    fn resolve_service(&mut self, value: u64) -> u64 {
        resolve_delta_value(&mut self.last_service, value)
    }

    fn resolve_tags(&mut self, value: u64) -> u64 {
        resolve_delta_value(&mut self.last_tags, value)
    }

    fn resolve_json_schema_id(&mut self, value: u64) -> u64 {
        resolve_delta_value(&mut self.last_json_schema_id, value)
    }
}

fn resolve_delta_value(last: &mut u64, value: u64) -> u64 {
    if value == 0 {
        *last
    } else {
        *last = value;
        value
    }
}

#[derive(Debug)]
struct Pattern {
    template: String,
    pos_list: Vec<u32>,
}

impl Pattern {
    fn new(template: String, pos_list: Vec<u32>) -> Self {
        Self { template, pos_list }
    }

    fn build_log_from_values(&self, values: &[String]) -> Result<String, String> {
        if values.len() != self.pos_list.len() {
            return Err(format!(
                "pattern expected {} dynamic values but log had {}",
                self.pos_list.len(),
                values.len()
            ));
        }

        let mut ordered_values = self
            .pos_list
            .iter()
            .copied()
            .zip(values.iter())
            .map(|(position, value)| (position as usize, value))
            .collect::<Vec<_>>();
        ordered_values.sort_by_key(|(position, _)| *position);

        let mut output = String::new();
        let mut last_byte_index = 0;
        for (position, value) in ordered_values {
            let byte_index = char_position_to_byte_index(&self.template, position)?;
            if byte_index < last_byte_index {
                return Err("pattern positions are not monotonic".to_string());
            }
            output.push_str(&self.template[last_byte_index..byte_index]);
            output.push_str(value);
            last_byte_index = byte_index;
        }
        output.push_str(&self.template[last_byte_index..]);
        Ok(output)
    }
}

fn char_position_to_byte_index(value: &str, position: usize) -> Result<usize, String> {
    if position == value.chars().count() {
        return Ok(value.len());
    }

    value
        .char_indices()
        .nth(position)
        .map(|(index, _)| index)
        .ok_or_else(|| format!("pattern position {position} is past template length"))
}

#[derive(Debug)]
struct DecodedLog {
    message: String,
    timestamp: i64,
    status: Option<String>,
    service: Option<String>,
    tags: Option<String>,
    uuid: Option<String>,
}
