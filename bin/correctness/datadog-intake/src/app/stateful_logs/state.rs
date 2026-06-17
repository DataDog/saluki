use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use datadog_protos::stateful::{
    datum, dynamic_value, Datum, DatumSequence, DeltaEncodingSync, DynamicValue, FlatLog,
    StatefulBatch as ProtoStatefulBatch,
};
use prost::Message as _;
use stele::StatefulLog;

/// Shared decoded stateful logs captured by datadog-intake.
#[derive(Clone, Default)]
pub struct StatefulLogsState {
    receiver: Arc<Mutex<StatefulLogsReceiver>>,
}

impl StatefulLogsState {
    /// Creates an empty stateful logs capture state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns all decoded logs captured so far.
    pub fn dump_logs(&self) -> Vec<StatefulLog> {
        self.receiver.lock().unwrap().decoded.clone()
    }

    /// Decodes one stateful batch and appends any logs it contains.
    pub fn decode_batch(&self, batch: &ProtoStatefulBatch) -> Result<(), String> {
        self.receiver.lock().unwrap().decode_batch(batch)
    }
}

#[derive(Debug, Default)]
struct StatefulLogsReceiver {
    patterns: HashMap<u64, Pattern>,
    dictionary: HashMap<u64, String>,
    decoded: Vec<StatefulLog>,
}

impl StatefulLogsReceiver {
    fn decode_batch(&mut self, batch: &ProtoStatefulBatch) -> Result<(), String> {
        let sequence = DatumSequence::decode(batch.data.as_slice()).map_err(|err| err.to_string())?;
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
                delta.resolve_pattern_id(pattern.pattern_id);
                self.patterns
                    .insert(pattern.pattern_id, Pattern::new(pattern.template, pattern.pos_list));
            }
            datum::Data::PatternDelete(pattern) => {
                self.patterns.remove(&pattern.pattern_id);
            }
            datum::Data::DictEntryDefine(entry) => {
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
                return Err("log datum decoding is not implemented for stateful logs correctness intake".to_string());
            }
            datum::Data::JsonSchemaDefine(_) | datum::Data::JsonSchemaDelete(_) => {}
        }

        Ok(())
    }

    fn decode_flat_log(&self, log: FlatLog, delta: &mut DeltaState) -> Result<StatefulLog, String> {
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

        Ok(StatefulLog::from_parts(
            message,
            self.optional_dictionary_value(status_id)?,
            self.optional_dictionary_value(service_id)?,
            self.optional_dictionary_value(tags_id)?,
            timestamp,
            log.uuid,
        ))
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
