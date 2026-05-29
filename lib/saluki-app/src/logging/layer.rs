use std::{fmt, path::Path, str::FromStr as _, sync::OnceLock};

use chrono::{
    format::{DelayedFormat, Item, StrftimeItems},
    Utc,
};
use chrono_tz::Tz;
use serde_json::{Map as JsonMap, Number as JsonNumber, Value as JsonValue};
use tracing::{field, Event, Level, Subscriber};
use tracing_subscriber::{
    field::VisitOutput,
    fmt::{format::Writer, FmtContext, FormatEvent, FormatFields, Layer, MakeWriter},
    layer,
    registry::LookupSpan,
    Layer as _,
};

use super::LoggingConfiguration;

pub fn build_formatting_layer<S, W>(config: &LoggingConfiguration, writer: W) -> Box<dyn layer::Layer<S> + Send + Sync>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    W: for<'writer> MakeWriter<'writer> + Send + Sync + 'static,
{
    if config.log_format_json {
        Layer::new()
            .json()
            .flatten_event(true)
            .with_target(true)
            .with_file(true)
            .with_line_number(true)
            .with_writer(writer)
            .boxed()
    } else {
        Layer::new()
            .event_format(AgentLikeFormatter::new(config.log_format_rfc3339))
            .with_writer(writer)
            .boxed()
    }
}

pub fn build_syslog_formatting_layer<S, W>(
    config: &LoggingConfiguration, writer: W,
) -> Box<dyn layer::Layer<S> + Send + Sync>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    W: for<'writer> MakeWriter<'writer> + Send + Sync + 'static,
{
    Layer::new()
        .event_format(SyslogFormatter::new(config.log_format_json, config.syslog_rfc))
        .with_writer(writer)
        .boxed()
}

struct AgentLikeFormatter {
    app_name: String,
    rfc3339: bool,
}

impl AgentLikeFormatter {
    fn new(rfc3339: bool) -> Self {
        // Get the configured short name for the current data plane and transform it to a consistent format.
        //
        // This will take something like "data-plane" or "Data Plane" and turn it into "DATAPLANE".
        Self {
            app_name: get_agent_logger_name(),
            rfc3339,
        }
    }
}

impl<S, N> FormatEvent<S, N> for AgentLikeFormatter
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(&self, _ctx: &FmtContext<'_, S, N>, mut writer: Writer<'_>, event: &Event<'_>) -> fmt::Result {
        let metadata = event.metadata();

        // Write the basic log header bits: time, data plane identifier, level, and file/line information:
        write!(
            writer,
            "{} | {} | {} | ",
            get_delayed_format_now(self.rfc3339),
            self.app_name,
            metadata.level()
        )?;

        if let (Some(file), Some(line)) = (metadata.file(), metadata.line()) {
            write!(writer, "({}:{})", file, line)?;
        } else {
            write!(writer, "(unknown:0)")?;
        }

        // Write the span fields, non-message event fields, and the message field itself:
        let mut v = AgentLikeFieldVisitor::new(writer.by_ref());
        event.record(&mut v);
        v.finish()?;

        writeln!(writer)
    }
}

struct SyslogFormatter {
    logger_name: String,
    app_name: String,
    pid: u32,
    json: bool,
    rfc: bool,
}

impl SyslogFormatter {
    fn new(json: bool, rfc: bool) -> Self {
        Self {
            logger_name: get_agent_logger_name(),
            app_name: get_process_app_name(),
            pid: std::process::id(),
            json,
            rfc,
        }
    }

    #[cfg(test)]
    fn for_tests(logger_name: impl Into<String>, app_name: impl Into<String>, pid: u32, json: bool, rfc: bool) -> Self {
        Self {
            logger_name: logger_name.into(),
            app_name: app_name.into(),
            pid,
            json,
            rfc,
        }
    }
}

impl<S, N> FormatEvent<S, N> for SyslogFormatter
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(&self, ctx: &FmtContext<'_, S, N>, writer: Writer<'_>, event: &Event<'_>) -> fmt::Result {
        if self.json {
            self.format_json_event(writer, event)
        } else {
            self.format_text_event(ctx, writer, event)
        }
    }
}

impl SyslogFormatter {
    fn format_text_event<S, N>(
        &self, ctx: &FmtContext<'_, S, N>, mut writer: Writer<'_>, event: &Event<'_>,
    ) -> fmt::Result
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
        N: for<'a> FormatFields<'a> + 'static,
    {
        let metadata = event.metadata();

        write!(
            writer,
            "{} {} | {} | ",
            format_syslog_header(metadata.level(), self.rfc, &self.app_name, self.pid),
            self.logger_name,
            metadata.level()
        )?;

        write_syslog_source_location(&mut writer, event, get_event_source_name(ctx, event))?;

        let mut visitor = AgentLikeFieldVisitor::new(writer.by_ref());
        event.record(&mut visitor);
        visitor.finish()?;

        writeln!(writer)
    }

    fn format_json_event(&self, mut writer: Writer<'_>, event: &Event<'_>) -> fmt::Result {
        let metadata = event.metadata();
        let mut visitor = AgentLikeJsonFieldVisitor::new();
        event.record(&mut visitor);

        let mut object = JsonMap::new();
        object.insert(
            "agent".to_string(),
            JsonValue::String(self.logger_name.to_ascii_lowercase()),
        );
        object.insert("level".to_string(), JsonValue::String(metadata.level().to_string()));
        object.insert(
            "relfile".to_string(),
            JsonValue::String(metadata.file().unwrap_or("unknown").to_string()),
        );
        object.insert(
            "line".to_string(),
            JsonValue::Number(JsonNumber::from(metadata.line().unwrap_or(0))),
        );
        object.insert(
            "msg".to_string(),
            visitor.message.unwrap_or_else(|| JsonValue::String(String::new())),
        );
        object.extend(visitor.fields);

        writeln!(
            writer,
            "{} {}",
            format_syslog_header(metadata.level(), self.rfc, &self.app_name, self.pid),
            JsonValue::Object(object)
        )
    }
}

fn get_agent_logger_name() -> String {
    let app_details = saluki_metadata::get_app_details();
    app_details
        .short_name()
        .to_uppercase()
        .replace("-", "")
        .replace(" ", "")
}

fn get_process_app_name() -> String {
    std::env::args()
        .next()
        .and_then(|arg| {
            Path::new(&arg)
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

// Format the syslog header for the given level, RFC mode, app name, and PID.
fn format_syslog_header(level: &Level, rfc: bool, app_name: &str, pid: u32) -> String {
    let priority = syslog_priority(level);
    if rfc {
        format!("<{}>1 {} {} - -", priority, app_name, pid)
    } else {
        format!("<{}>{}[{}]:", priority, app_name, pid)
    }
}

fn get_event_source_name<'event, S, N>(ctx: &FmtContext<'_, S, N>, event: &'event Event<'_>) -> Option<&'event str>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    // Try to get current tracing span name; if there's no span, fall back to event's Rust module path.
    ctx.event_scope()
        .and_then(|scope| scope.into_iter().next().map(|span| span.metadata().name()))
        .or_else(|| event.metadata().module_path())
}

fn write_syslog_source_location(writer: &mut Writer<'_>, event: &Event<'_>, source_name: Option<&str>) -> fmt::Result {
    let metadata = event.metadata();

    if let (Some(file), Some(line)) = (metadata.file(), metadata.line()) {
        if let Some(source_name) = source_name.filter(|name| !name.is_empty()) {
            write!(writer, "({}:{} in {})", file, line, source_name)
        } else {
            write!(writer, "({}:{})", file, line)
        }
    } else {
        write!(writer, "(unknown:0)")
    }
}

// Match the Go Agent's legacy syslog facility: 20 is `local4`, one of syslog's
// application-defined facilities. Syslog stores severity in the lower three
// bits, so multiplying by 8 shifts the facility before adding severity.
fn syslog_priority(level: &Level) -> u16 {
    20 * 8 + u16::from(syslog_severity(level))
}

fn syslog_severity(level: &Level) -> u8 {
    match *level {
        Level::TRACE | Level::DEBUG => 7,
        Level::INFO => 6,
        Level::WARN => 4,
        Level::ERROR => 3,
    }
}

/// Field visitor that writes event fields similar to the Datadog Agent.
///
/// Structured fields in Agent log messages are written as colon-separated key/value pairs, which are then separated by
/// commas, like so: `key:value,key2:value2,...`. Structured fields are also written before the message, and both are
/// separated by a pipe character, like other sections of the overall log format.
///
/// This means that both structured fields and the log message are written as `| <text>`, leading to something equivalent
/// to the following when both are present:
///
/// ```text
/// | key:value,key2:value2 | message
/// ```
///
/// # Errors
///
/// Errors with writing fields to the given writer are tracked internally. If any operation hits an error during
/// writing, the error is captured and returned when the visitor is finished. All subsequent operations after an error
/// are no-ops
struct AgentLikeFieldVisitor<'writer> {
    writer: Writer<'writer>,
    fields_written: usize,
    message: String,
    last_result: fmt::Result,
}

impl<'writer> AgentLikeFieldVisitor<'writer> {
    fn new(writer: Writer<'writer>) -> Self {
        Self {
            writer,
            fields_written: 0,
            message: String::new(),
            last_result: Ok(()),
        }
    }

    fn needs_prefix(&self) -> bool {
        self.fields_written == 0
    }

    fn needs_separator(&self) -> bool {
        self.fields_written > 0
    }

    /// Writes the given field to the writer.
    ///
    /// `f` is expected to write the field value in whatever the appropriate format is, and should only write the field
    /// value: all other aspects -- field name, separators, etc -- are handled outside of the closure.
    fn try_write(&mut self, field: &field::Field, f: impl FnOnce(&mut dyn fmt::Write) -> fmt::Result) {
        if self.last_result.is_err() {
            return;
        }

        if field.name() == "message" {
            // We store the `message` field until the end when flushing, as it must come last.
            //
            // We still format it here, though.
            self.last_result = f(&mut self.message);
        } else {
            let prefix = if self.needs_prefix() { " | " } else { "" };
            let separator = if self.needs_separator() { "," } else { "" };

            self.last_result = write!(self.writer, "{}{}{}:", prefix, separator, field.name());
            if self.last_result.is_err() {
                return;
            }

            self.last_result = f(&mut self.writer);
            if self.last_result.is_ok() {
                self.fields_written += 1;
            }
        }
    }
}

impl field::Visit for AgentLikeFieldVisitor<'_> {
    fn record_debug(&mut self, field: &field::Field, value: &dyn fmt::Debug) {
        // Small behavior tweak: we don't care about quoting the message field.
        self.try_write(field, |w| {
            if field.name() == "message" {
                write!(w, "{:?}", value)
            } else {
                write!(w, "\"{:?}\"", value)
            }
        });
    }

    fn record_str(&mut self, field: &field::Field, value: &str) {
        // Small behavior tweak: we don't care about quoting the message field.
        self.try_write(field, |w| {
            if field.name() == "message" {
                w.write_str(value)
            } else {
                write!(w, "\"{}\"", value)
            }
        });
    }

    fn record_f64(&mut self, field: &field::Field, value: f64) {
        let mut float_writer = ryu::Buffer::new();
        let float_str = float_writer.format(value);
        self.try_write(field, |w| w.write_str(float_str));
    }

    fn record_i64(&mut self, field: &field::Field, value: i64) {
        let mut int_writer = itoa::Buffer::new();
        let int_str = int_writer.format(value);
        self.try_write(field, |w| w.write_str(int_str));
    }

    fn record_u64(&mut self, field: &field::Field, value: u64) {
        let mut int_writer = itoa::Buffer::new();
        let int_str = int_writer.format(value);
        self.try_write(field, |w| w.write_str(int_str));
    }

    fn record_i128(&mut self, field: &field::Field, value: i128) {
        let mut int_writer = itoa::Buffer::new();
        let int_str = int_writer.format(value);
        self.try_write(field, |w| w.write_str(int_str));
    }

    fn record_bool(&mut self, field: &field::Field, value: bool) {
        self.try_write(field, |w| if value { write!(w, "true") } else { write!(w, "false") });
    }

    fn record_u128(&mut self, field: &field::Field, value: u128) {
        let mut int_writer = itoa::Buffer::new();
        let int_str = int_writer.format(value);
        self.try_write(field, |w| w.write_str(int_str));
    }
}

impl VisitOutput<fmt::Result> for AgentLikeFieldVisitor<'_> {
    fn finish(mut self) -> fmt::Result {
        // Check to see if our last write operation was successful or not before trying to write the `message` field.
        self.last_result?;

        if !self.message.is_empty() {
            write!(self.writer, " | {}", self.message)
        } else {
            Ok(())
        }
    }
}

#[derive(Default)]
struct AgentLikeJsonFieldVisitor {
    fields: JsonMap<String, JsonValue>,
    message: Option<JsonValue>,
}

impl AgentLikeJsonFieldVisitor {
    fn new() -> Self {
        Self::default()
    }

    fn record_value(&mut self, field: &field::Field, value: JsonValue) {
        if field.name() == "message" {
            self.message = Some(value);
        } else {
            self.fields.insert(field.name().to_string(), value);
        }
    }
}

impl field::Visit for AgentLikeJsonFieldVisitor {
    fn record_debug(&mut self, field: &field::Field, value: &dyn fmt::Debug) {
        self.record_value(field, JsonValue::String(format!("{:?}", value)));
    }

    fn record_str(&mut self, field: &field::Field, value: &str) {
        self.record_value(field, JsonValue::String(value.to_string()));
    }

    fn record_f64(&mut self, field: &field::Field, value: f64) {
        let value = JsonNumber::from_f64(value)
            .map(JsonValue::Number)
            .unwrap_or_else(|| JsonValue::String(value.to_string()));
        self.record_value(field, value);
    }

    fn record_i64(&mut self, field: &field::Field, value: i64) {
        self.record_value(field, JsonValue::Number(JsonNumber::from(value)));
    }

    fn record_u64(&mut self, field: &field::Field, value: u64) {
        self.record_value(field, JsonValue::Number(JsonNumber::from(value)));
    }

    fn record_i128(&mut self, field: &field::Field, value: i128) {
        if let Ok(value) = i64::try_from(value) {
            self.record_i64(field, value);
        } else {
            self.record_value(field, JsonValue::String(value.to_string()));
        }
    }

    fn record_bool(&mut self, field: &field::Field, value: bool) {
        self.record_value(field, JsonValue::Bool(value));
    }

    fn record_u128(&mut self, field: &field::Field, value: u128) {
        if let Ok(value) = u64::try_from(value) {
            self.record_u64(field, value);
        } else {
            self.record_value(field, JsonValue::String(value.to_string()));
        }
    }
}

/// Gets a delayed formatter for the current time.
///
/// When `rfc3339` is `true`, returns RFC 3339 format (`2024-12-31T23:59:59Z`, system timezone).
/// When `false`, returns the legacy format (`2024-12-31 23:59:59 UTC`, system timezone).
fn get_delayed_format_now(rfc3339: bool) -> DelayedFormat<impl Iterator<Item = &'static Item<'static>> + Clone> {
    // We fallback to using UTC if something goes wrong during timezone detection.
    static SYSTEM_TZ: OnceLock<Tz> = OnceLock::new();
    let system_tz = SYSTEM_TZ.get_or_init(|| {
        iana_time_zone::get_timezone()
            .map_err(|_| ())
            .and_then(|raw_tz| Tz::from_str(&raw_tz).map_err(|_| ()))
            .unwrap_or(Tz::UTC)
    });

    let now = Utc::now().with_timezone(system_tz);

    if rfc3339 {
        static RFC3339_FORMAT_ITEMS: OnceLock<Vec<Item<'static>>> = OnceLock::new();
        let format_items = RFC3339_FORMAT_ITEMS.get_or_init(|| {
            StrftimeItems::new("%Y-%m-%dT%H:%M:%SZ")
                .parse()
                .expect("should not fail to parse datetime format")
        });
        now.format_with_items(format_items.iter())
    } else {
        // Timestamp format to end up with the equivalent of `2024-12-31 23:59:59 UTC`.
        static FORMAT_ITEMS: OnceLock<Vec<Item<'static>>> = OnceLock::new();
        let format_items = FORMAT_ITEMS.get_or_init(|| {
            StrftimeItems::new("%Y-%m-%d %H:%M:%S %Z")
                .parse()
                .expect("should not fail to parse datetime format")
        });
        now.format_with_items(format_items.iter())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        sync::{Arc, Mutex},
    };

    use serde_json::Value;
    use tracing::{event, info_span, Level};
    use tracing_subscriber::{filter::LevelFilter, fmt::MakeWriter, prelude::*, registry::Registry};

    use super::*;

    #[test]
    fn syslog_priority_maps_tracing_levels_to_agent_values() {
        assert_eq!(syslog_priority(&Level::TRACE), 167);
        assert_eq!(syslog_priority(&Level::DEBUG), 167);
        assert_eq!(syslog_priority(&Level::INFO), 166);
        assert_eq!(syslog_priority(&Level::WARN), 164);
        assert_eq!(syslog_priority(&Level::ERROR), 163);
    }

    #[test]
    fn syslog_header_uses_legacy_shape_by_default() {
        let header = format_syslog_header(&Level::INFO, false, "agent-data-plane", 1234);

        assert_eq!(header, "<166>agent-data-plane[1234]:");
    }

    #[test]
    fn syslog_header_uses_rfc_shape_when_enabled() {
        let header = format_syslog_header(&Level::INFO, true, "agent-data-plane", 1234);

        assert_eq!(header, "<166>1 agent-data-plane 1234 - -");
    }

    #[test]
    fn syslog_text_formatter_includes_header_metadata_fields_message_and_newline() {
        let output = render_syslog_event(
            SyslogFormatter::for_tests("DATAPLANE", "adp", 1234, false, false),
            || {
                event!(Level::WARN, answer = 42_i64, enabled = true, message = "text-message");
            },
        );

        assert!(output.starts_with("<164>adp[1234]: DATAPLANE | WARN | ("));
        assert!(output.contains("layer.rs:"));
        assert!(output.contains(" in saluki_app::logging::layer"));
        assert!(output.contains(" | answer:42,enabled:true | text-message"));
        assert!(output.ends_with('\n'));
    }

    #[test]
    fn syslog_text_formatter_uses_current_span_as_source_context_when_available() {
        let output = render_syslog_event(
            SyslogFormatter::for_tests("DATAPLANE", "adp", 1234, false, false),
            || {
                let span = info_span!("process_packet");
                let _guard = span.enter();
                event!(Level::WARN, message = "span-message");
            },
        );

        assert!(output.contains(" in process_packet)"));
        assert!(output.contains(" | span-message"));
    }

    #[test]
    fn syslog_json_formatter_includes_header_metadata_fields_message_and_newline() {
        let output = render_syslog_event(SyslogFormatter::for_tests("DATAPLANE", "adp", 1234, true, true), || {
            event!(Level::ERROR, answer = 42_i64, enabled = true, message = "json-message");
        });

        let payload = output
            .strip_prefix("<163>1 adp 1234 - - ")
            .expect("output should have RFC syslog prefix");
        assert!(payload.ends_with('\n'));

        let payload: Value = serde_json::from_str(payload.trim_end()).expect("payload should be JSON");
        assert_eq!(payload["agent"], "dataplane");
        assert_eq!(payload["level"], "ERROR");
        assert_eq!(payload["msg"], "json-message");
        assert_eq!(payload["answer"], 42);
        assert_eq!(payload["enabled"], true);
        assert!(payload["relfile"]
            .as_str()
            .expect("relfile should be a string")
            .contains("layer.rs"));
        assert!(payload["line"].as_u64().expect("line should be numeric") > 0);
    }

    fn render_syslog_event(formatter: SyslogFormatter, emit: impl FnOnce()) -> String {
        let writer = SharedWriter::default();
        let layer = Layer::new()
            .event_format(formatter)
            .with_writer(writer.clone())
            .with_filter(LevelFilter::TRACE);
        let subscriber = Registry::default().with(layer);

        tracing::subscriber::with_default(subscriber, emit);

        writer.contents()
    }

    #[derive(Clone, Default)]
    struct SharedWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl SharedWriter {
        fn contents(&self) -> String {
            let buffer = self.buffer.lock().expect("writer buffer lock should not be poisoned");
            String::from_utf8(buffer.clone()).expect("log output should be valid UTF-8")
        }
    }

    struct SharedWriterGuard {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl io::Write for SharedWriterGuard {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.buffer
                .lock()
                .expect("writer buffer lock should not be poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> MakeWriter<'writer> for SharedWriter {
        type Writer = SharedWriterGuard;

        fn make_writer(&'writer self) -> Self::Writer {
            SharedWriterGuard {
                buffer: self.buffer.clone(),
            }
        }
    }
}
