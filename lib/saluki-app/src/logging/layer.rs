use std::{fmt, str::FromStr as _, sync::OnceLock};

use chrono::{
    format::{DelayedFormat, Item, StrftimeItems},
    Utc,
};
use chrono_tz::Tz;
use tracing::{field, Event, Subscriber};
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
            .event_format(AgentLikeFormatter::new())
            .with_writer(writer)
            .boxed()
    }
}

struct AgentLikeFormatter {
    app_name: String,
}

impl AgentLikeFormatter {
    fn new() -> Self {
        // Get the configured short name for the current data plane and transform it to a consistent format.
        //
        // This will take something like "data-plane" or "Data Plane" and turn it into "DATAPLANE".
        let app_details = saluki_metadata::get_app_details();
        let app_name = app_details
            .short_name()
            .to_uppercase()
            .replace("-", "")
            .replace(" ", "");

        Self { app_name }
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
            get_delayed_format_now(),
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

/// Gets a delayed formatter for the current time.
fn get_delayed_format_now() -> DelayedFormat<impl Iterator<Item = &'static Item<'static>> + Clone> {
    // Determine the system's timezone.
    //
    // We fallback to using UTC if something goes wrong during timezone detection.
    static SYSTEM_TZ: OnceLock<Tz> = OnceLock::new();
    let system_tz = SYSTEM_TZ.get_or_init(|| {
        iana_time_zone::get_timezone()
            .map_err(|_| ())
            .and_then(|raw_tz| Tz::from_str(&raw_tz).map_err(|_| ()))
            .unwrap_or(Tz::UTC)
    });

    // Timestamp format to end up with the equivalent of `2024-12-31 23:59:59 UTC`.
    static FORMAT_ITEMS: OnceLock<Vec<Item<'static>>> = OnceLock::new();
    let format_items = FORMAT_ITEMS.get_or_init(|| {
        let parser = StrftimeItems::new("%Y-%m-%d %H:%M:%S %Z");
        parser.parse().expect("should not fail to parse datetime format")
    });

    let now = Utc::now().with_timezone(system_tz);
    now.format_with_items(format_items.iter())
}
