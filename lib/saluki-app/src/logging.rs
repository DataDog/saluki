//! Logging.

// TODO: `AgentLikeFieldVisitor` currently allocates a `String` to hold the message field when it finds it. This is
// suboptimal because it means we allocate a string literally every time we log a message. Logging is rare, but it's
// just a recipe for small, unnecessary allocations over time... and makes it that much more inefficient to enable
// debug/trace-level logging in production.
//
// We might consider _something_ like a string pool in the future, but we can defer that until we have a better idea of
// what the potential impact is in practice.

use std::{fmt, str::FromStr as _, sync::OnceLock};

use chrono::{
    format::{DelayedFormat, Item, StrftimeItems},
    Utc,
};
use chrono_tz::Tz;
use tracing::{field, level_filters::LevelFilter, Event, Subscriber};
use tracing_subscriber::{
    field::VisitOutput,
    fmt::{format::Writer, FmtContext, FormatEvent, FormatFields},
    registry::LookupSpan,
    EnvFilter,
};

/// Logs a message to standard error and exits the process with a non-zero exit code.
pub fn fatal_and_exit(message: String) {
    eprintln!("FATAL: {}", message);
    std::process::exit(1);
}

/// Initializes the logging subsystem for `tracing`.
///
/// This function reads the `DD_LOG_LEVEL` environment variable to determine the log level to use. If the environment
/// variable is not set, the default log level is `INFO`. Additionally, it reads the `DD_LOG_FORMAT_JSON` environment
/// variable to determine which output format to use. If it is set to `json` (case insensitive), the logs will be
/// formatted as JSON. If it is set to any other value, or not set at all, the logs will default to a rich, colored,
/// human-readable format.
///
/// ## Errors
///
/// If the logging subsystem was already initialized, an error will be returned.
pub fn initialize_logging(default_level: Option<LevelFilter>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let is_json = std::env::var("DD_LOG_FORMAT_JSON")
        .map(|s| s.trim().to_lowercase())
        .map(|s| s == "true" || s == "1")
        .unwrap_or(false);

    let level_filter = EnvFilter::builder()
        .with_default_directive(default_level.unwrap_or(LevelFilter::INFO).into())
        .with_env_var("DD_LOG_LEVEL")
        .from_env_lossy();

    if is_json {
        initialize_tracing_json(level_filter)
    } else {
        initialize_tracing_pretty(level_filter)
    }
}

fn initialize_tracing_json(level_filter: EnvFilter) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(level_filter)
        .with_target(true)
        .with_file(true)
        .with_line_number(true)
        .json()
        .flatten_event(true)
        .try_init()
}

fn initialize_tracing_pretty(level_filter: EnvFilter) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(level_filter)
        .event_format(AgentLikeFormatter::new())
        .try_init()
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

impl<'writer> field::Visit for AgentLikeFieldVisitor<'writer> {
    fn record_debug(&mut self, field: &field::Field, value: &dyn fmt::Debug) {
        self.try_write(field, |w| write!(w, "{:?}", value));
    }

    fn record_str(&mut self, field: &field::Field, value: &str) {
        self.try_write(field, |w| write!(w, "{}", value));
    }
}

impl<'writer> VisitOutput<fmt::Result> for AgentLikeFieldVisitor<'writer> {
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
