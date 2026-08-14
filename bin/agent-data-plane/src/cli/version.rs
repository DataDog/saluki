use argh::FromArgs;
use saluki_metadata::AppDetails;

/// Prints the agent-data-plane version.
#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "version")]
pub struct VersionCommand {
    /// emits the version information as JSON, with additional detail
    #[argh(switch)]
    pub json: bool,
}

/// Prints the agent-data-plane version.
///
/// Takes the details directly rather than reading the registered ones, since this runs before bootstrap has had a
/// chance to register them.
pub async fn handle_version_command(app_data: &AppDetails, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(app_data).expect("Unable to serialize version information.")
        )
    } else {
        println!("v{}-{}", app_data.version().raw(), app_data.git_hash())
    }
}
