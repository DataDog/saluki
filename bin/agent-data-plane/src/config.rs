use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(about)]
pub struct Cli {
    /// Subcommand to run.
    #[command(subcommand)]
    pub action: Option<Action>,
}

#[derive(Subcommand)]
pub enum Action {
    /// Runs the data plane.
    #[command(name = "run")]
    Run(RunConfig),

    /// Various debugging commands.
    #[command(subcommand)]
    Debug(DebugConfig),

    /// Various dogstatsd commands.
    #[command(subcommand)]
    Dogstatsd(DogstatsdConfig),
}

/// Run subcommand configuration.
#[derive(Args, Debug)]
pub struct RunConfig {
    /// Path to the configuration file.
    #[arg(short = 'c', long = "config", default_value = "/etc/datadog-agent/datadog.yaml")]
    pub config: PathBuf,
}

/// Debug subcommand configuration.
#[derive(Subcommand, Debug)]
pub enum DebugConfig {
    /// Resets log level.
    #[command(name = "reset-log-level")]
    ResetLogLevel,

    /// Overrides the current log level.
    #[command(name = "set-log-level")]
    SetLogLevel(SetLogLevelConfig),
}

/// Set log level configuration.
#[derive(Args, Debug)]
pub struct SetLogLevelConfig {
    /// Filter directives to apply (e.g. `INFO`, `DEBUG`, `TRACE`, `WARN`, `ERROR`).
    #[arg(required = true)]
    pub filter_directives: String,

    /// Amount of time to apply the log level override, in seconds.
    #[arg(required = true)]
    pub duration_secs: u64,
}

/// Dogstatsd subcommand configuration.
#[derive(Subcommand, Debug)]
pub enum DogstatsdConfig {
    /// Prints basic statistics about the metrics received by the data plane.
    #[command(name = "stats")]
    Stats(DogstatsdStatsConfig),
}

/// Dogstatsd stats subcommand configuration.
#[derive(Args, Debug)]
pub struct DogstatsdStatsConfig {
    /// Amount of time to collect statistics for, in seconds.
    #[arg(required = true, short = 'd', long = "duration-secs")]
    pub collection_duration_secs: u64,

    /// Analysis mode to use.
    #[arg(value_enum, required = false, short = 'm', long = "mode", default_value_t = AnalysisMode::Summary)]
    pub analysis_mode: AnalysisMode,

    /// Sort direction to use.
    #[arg(value_enum, required = false, short = 's', long = "sort-dir")]
    pub sort_direction: Option<SortDirection>,

    /// Filter to apply to metric names. Any metrics which don't match the filter will be excluded.
    #[arg(required = false, short = 'f', long = "filter")]
    pub filter: Option<String>,
}

/// Sort direction.
#[derive(Clone, Copy, Debug, Default, ValueEnum)]
pub enum SortDirection {
    /// Sorts in ascending order.
    #[default]
    #[value(name = "asc")]
    Ascending,

    /// Sorts in descending order.
    #[value(name = "desc")]
    Descending,
}

/// Analysis mode.
#[derive(Clone, Copy, Debug, Default, ValueEnum)]
pub enum AnalysisMode {
    /// Displays a high-level summary of all collected metrics, sorted by metric name.
    #[default]
    #[value(name = "summary")]
    Summary,

    /// Displays the cardinality of all collected metrics, sorted by cardinality.
    #[value(name = "cardinality")]
    Cardinality,
}
