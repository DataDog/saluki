//! Logging.

// TODO: `AgentLikeFieldVisitor` currently allocates a `String` to hold the message field when it finds it. This is
// suboptimal because it means we allocate a string literally every time we log a message. Logging is rare, but it's
// just a recipe for small, unnecessary allocations over time... and makes it that much more inefficient to enable
// debug/trace-level logging in production.
//
// We might consider _something_ like a string pool in the future, but we can defer that until we have a better idea of
// what the potential impact is in practice.

use std::{
    fmt,
    future::pending,
    str::FromStr as _,
    sync::{Mutex, OnceLock},
    time::Duration,
};

use bytesize::ByteSize;
use chrono::{
    format::{DelayedFormat, Item, StrftimeItems},
    Utc,
};
use chrono_tz::Tz;
use saluki_api::{
    extract::{Query, State},
    response::IntoResponse,
    routing::{post, Router},
    APIHandler, StatusCode,
};
use saluki_common::{deser::PermissiveBool, task::spawn_traced_named};
use saluki_config::{ConfigurationLoader, GenericConfiguration};
use saluki_error::{generic_error, ErrorContext, GenericError};
use serde::Deserialize;
use serde_with::serde_as;
use tokio::{select, sync::mpsc, time::sleep};
use tracing::{error, field, info, level_filters::LevelFilter, Event, Subscriber};
use tracing_rolling_file::RollingFileAppenderBase;
use tracing_subscriber::{
    field::VisitOutput,
    fmt::{format::Writer, FmtContext, FormatEvent, FormatFields, MakeWriter},
    layer::SubscriberExt as _,
    registry::LookupSpan,
    reload::{Handle, Layer as ReloadLayer},
    util::SubscriberInitExt as _,
    EnvFilter, Layer, Registry,
};

static API_HANDLER: Mutex<Option<LoggingAPIHandler>> = Mutex::new(None);

fn default_log_level() -> LogLevel {
    LevelFilter::INFO.into()
}

const fn default_true() -> bool {
    true
}

const fn default_false() -> bool {
    false
}

const fn default_log_file_max_size() -> ByteSize {
    ByteSize::mib(10)
}

const fn default_log_file_max_rolls() -> usize {
    1
}

#[serde_as]
#[derive(Deserialize)]
struct LoggingConfiguration {
    #[serde(default = "default_log_level")]
    log_level: LogLevel,
    #[serde_as(as = "PermissiveBool")]
    #[serde(default = "default_false")]
    log_format_json: bool,

    #[serde_as(as = "PermissiveBool")]
    #[serde(default = "default_true")]
    log_to_console: bool,

    #[serde(default = "String::new")]
    log_file: String,
    #[serde(default = "default_log_file_max_size")]
    log_file_max_size: ByteSize,
    #[serde(default = "default_log_file_max_rolls")]
    log_file_max_rolls: usize,
}

impl LoggingConfiguration {
    pub(crate) fn from_config(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let logging_config = config.as_typed()?;
        Ok(logging_config)
    }

    async fn from_environment() -> Result<Self, GenericError> {
        // TODO: For the sake of transitioning to this new bootstrapping pattern, we're just creating a configuration
        // manually here so that we can drive everything through use of `LoggingConfiguration` instead of two different
        // code paths. That means we want to use `GenericConfiguration` to source our environment variables instead of
        // querying them manually... mostly to ensure that doing it that way (the way we want to do it overall) is
        // consistent with how we're doing it by hand at the moment.
        let config = ConfigurationLoader::default()
            .from_environment("DD")
            .expect("Environment variable prefix is not empty.")
            .into_generic()
            .await?;
        Self::from_config(&config)
    }
}

#[derive(Deserialize)]
#[serde(try_from = "String")]
struct LogLevel(EnvFilter);

impl LogLevel {
    fn as_env_filter(&self) -> EnvFilter {
        self.0.clone()
    }
}

impl From<LevelFilter> for LogLevel {
    fn from(level: LevelFilter) -> Self {
        Self(EnvFilter::default().add_directive(level.into()))
    }
}

impl TryFrom<String> for LogLevel {
    type Error = GenericError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty() {
            return Err(generic_error!("Log level cannot be empty."));
        }

        EnvFilter::builder()
            .parse(value)
            .map(Self)
            .error_context("Failed to parse valid log level.")
    }
}

/// Logs a message to standard error and exits the process with a non-zero exit code.
pub fn fatal_and_exit(message: String) {
    eprintln!("FATAL: {}", message);
    std::process::exit(1);
}

/// Initializes the logging subsystem for `tracing` with the ability to dynamically update the log filtering directives
/// at runtime.
///
/// This function reads the `DD_LOG_LEVEL` environment variable to determine the log level to use. If the environment
/// variable is not set, the default log level is `INFO`. Additionally, it reads the `DD_LOG_FORMAT_JSON` environment
/// variable to determine which output format to use. If it is set to `json` (case insensitive), the logs will be
/// formatted as JSON. If it is set to any other value, or not set at all, the logs will default to a rich, colored,
/// human-readable format.
///
/// An API handler can be acquired (via [`acquire_logging_api_handler`]) to install the API routes which allow for
/// dynamically controlling the logging level filtering. See [`LoggingAPIHandler`] for more information.
///
/// # Errors
///
/// If the logging subsystem was already initialized, an error will be returned.
pub async fn initialize_logging() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // TODO: Support for logging to file.
    // TODO: Support for logging to syslog.

    let logging_config = LoggingConfiguration::from_environment()
        .await
        .map_err(|e| format!("Failed to initialize logging configuration: {}", e))?;

    // Set up our log level filtering and dynamic filter layer.
    let level_filter = logging_config.log_level.as_env_filter();
    let (filter_layer, reload_handle) = ReloadLayer::new(level_filter.clone());
    API_HANDLER
        .lock()
        .unwrap()
        .replace(LoggingAPIHandler::new(level_filter, reload_handle));

    let mut configured_layers = Vec::new();

    if logging_config.log_to_console {
        // Whatever formatter we're using, it will write to stdout by default, so we just create a new formatter layer
        // and use it as-is.
        configured_layers.push(build_formatter_layer(&logging_config, std::io::stdout));
    }

    if !logging_config.log_file.is_empty() {
        let appender_builder = RollingFileAppenderBase::builder();
        let appender = appender_builder
            .filename(logging_config.log_file.clone())
            .max_filecount(logging_config.log_file_max_rolls)
            .condition_max_file_size(logging_config.log_file_max_size.as_u64())
            .build()?;

        let (non_blocking_appender, worker_guard) = appender.get_non_blocking_appender();

        // TODO: Right now, we're just shoving this worker guard into an asynchronous task to keep it alive,
        // but we really should return it so that it can be held on to and only dropped at process exit... we
        // lack a mechanism to return it cleanly, though, so we'll tackle this once we have a proper bootstrap
        // procedure.
        tokio::spawn(async move {
            let _worker_guard = worker_guard;
            pending::<()>().await;
        });

        configured_layers.push(build_formatter_layer(&logging_config, non_blocking_appender));
    }

    tracing_subscriber::registry()
        .with(configured_layers.with_filter(filter_layer))
        .try_init()?;

    Ok(())
}

fn build_formatter_layer<S, W>(config: &LoggingConfiguration, writer: W) -> Box<dyn Layer<S> + Send + Sync>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    W: for<'writer> MakeWriter<'writer> + Send + Sync + 'static,
{
    if config.log_format_json {
        tracing_subscriber::fmt::Layer::new()
            .json()
            .flatten_event(true)
            .with_target(true)
            .with_file(true)
            .with_line_number(true)
            .with_writer(writer)
            .boxed()
    } else {
        tracing_subscriber::fmt::Layer::new()
            .event_format(AgentLikeFormatter::new())
            .with_writer(writer)
            .boxed()
    }
}

/// Acquires the logging API handler.
///
/// This function is mutable, and consumes the handler if it's present. This means it should only be called once, and
/// only after logging has been initialized via `initialize_dynamic_logging`.
///
/// The logging API handler can be used to install API routes which allow dynamically controlling the logging level
/// filtering. See [`LoggingAPIHandler`] for more information.
pub fn acquire_logging_api_handler() -> Option<LoggingAPIHandler> {
    API_HANDLER.lock().unwrap().take()
}

#[derive(Deserialize)]
struct OverrideQueryParams {
    time_secs: u64,
}

/// State used for the logging API handler.
#[derive(Clone)]
pub struct LoggingHandlerState {
    override_tx: mpsc::Sender<Option<(Duration, EnvFilter)>>,
}

/// An API handler for updating log filtering directives at runtime.
///
/// This handler exposes two main routes -- `/logging/override` and `/logging/reset` -- which allow for overriding the
/// default log filtering directives (configured at startup) at runtime, and then resetting them once the override is no
/// longer needed.
///
/// As this has the potential for incredibly verbose logging at runtime, the override is set with a specific duration in
/// which it will apply. Once an override has been active for the configured duration, it will automatically be reset
/// unless the override is refreshed before the duration elapses.
///
/// The maximum duration for an override is 10 minutes.
pub struct LoggingAPIHandler {
    state: LoggingHandlerState,
}

impl LoggingAPIHandler {
    fn new(original_filter: EnvFilter, reload_handle: Handle<EnvFilter, Registry>) -> Self {
        // Spawn our background task that will handle override requests.
        let (override_tx, override_rx) = mpsc::channel(1);
        spawn_traced_named(
            "dynamic-logging-override-processor",
            process_override_requests(original_filter, reload_handle, override_rx),
        );

        Self {
            state: LoggingHandlerState { override_tx },
        }
    }

    async fn override_handler(
        State(state): State<LoggingHandlerState>, params: Query<OverrideQueryParams>, body: String,
    ) -> impl IntoResponse {
        // Make sure the override length is within the acceptable range.
        const MAXIMUM_OVERRIDE_LENGTH_SECS: u64 = 600;
        if params.time_secs > MAXIMUM_OVERRIDE_LENGTH_SECS {
            return (
                StatusCode::BAD_REQUEST,
                format!(
                    "override time cannot be greater than {} seconds",
                    MAXIMUM_OVERRIDE_LENGTH_SECS
                ),
            );
        }

        // Parse the override duration and create a new filter from the body.
        let duration = Duration::from_secs(params.time_secs);
        let new_filter = match EnvFilter::try_new(body) {
            Ok(filter) => filter,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    format!("failed to parse override filter: {}", e),
                )
            }
        };

        // Instruct the override processor to apply the new log filtering directives for the given duration.
        let _ = state.override_tx.send(Some((duration, new_filter))).await;

        (StatusCode::OK, "acknowledged".to_string())
    }

    async fn reset_handler(State(state): State<LoggingHandlerState>) {
        // Instruct the override processor to immediately reset back to the original log filtering directives.
        let _ = state.override_tx.send(None).await;
    }
}

impl APIHandler for LoggingAPIHandler {
    type State = LoggingHandlerState;

    fn generate_initial_state(&self) -> Self::State {
        self.state.clone()
    }

    fn generate_routes(&self) -> Router<Self::State> {
        Router::new()
            .route("/logging/override", post(Self::override_handler))
            .route("/logging/reset", post(Self::reset_handler))
    }
}

async fn process_override_requests(
    original_filter: EnvFilter, reload_handle: Handle<EnvFilter, Registry>,
    mut rx: mpsc::Receiver<Option<(Duration, EnvFilter)>>,
) {
    let mut override_active = false;
    let override_timeout = sleep(Duration::from_secs(3600));

    tokio::pin!(override_timeout);

    loop {
        select! {
            maybe_override = rx.recv() => match maybe_override {
                Some(Some((duration, new_filter))) => {
                    info!(directives = %new_filter, "Overriding existing log filtering directives for {} seconds...", duration.as_secs());

                    match reload_handle.reload(new_filter) {
                        Ok(()) => {
                            // We were able to successfully reload the filter, so mark ourselves as having an active
                            // override and update the override timeout.
                            override_active = true;
                            override_timeout.as_mut().reset(tokio::time::Instant::now() + duration);
                        },
                        Err(e) => error!(error = %e, "Failed to override log filtering directives."),
                    }
                },

                Some(None) => {
                    // We've been instructed to immediately reset the filter back to the original one, so simply update
                    // the override timeout to fire as soon as possible.
                    override_timeout.as_mut().reset(tokio::time::Instant::now());
                },

                // Our sender has dropped, so there's no more override requests for us to handle.
                None => break,
            },
            _ = &mut override_timeout => {
                // Our override timeout has fired. If we have an active override, reset it back to the original filter.
                //
                // Otherwise, this just means that we've been running for a while without any overrides, so we can just
                // reset the timeout with a long duration.
                if override_active {
                    override_active = false;

                    if let Err(e) = reload_handle.reload(original_filter.clone()) {
                        error!(error = %e, "Failed to reset log filtering directives.");
                    }

                    info!(directives = %original_filter, "Restored original log filtering directives.");
                }

                override_timeout.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(3600));
            }
        }
    }

    // Reset our filter to the original one before we exit.
    if let Err(e) = reload_handle.reload(original_filter) {
        error!(error = %e, "Failed to reset log filtering directives before override handler shutdown.");
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
