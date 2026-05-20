use argh::FromArgs;

/// Prints the agent-data-plane version.
#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "version")]
pub struct VersionCommand {}

/// Prints the agent-data-plane version.
pub async fn handle_version_command() {
    println!(env!("CARGO_PKG_VERSION"));
}
