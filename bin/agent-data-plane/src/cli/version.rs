use argh::FromArgs;

/// Prints the agent-data-plane version.
#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "version")]
pub struct VersionCommand {
    /// emits the version information as JSON, with additional detail
    #[argh(switch)]
    pub json: bool,
}

/// Prints the agent-data-plane version.
pub async fn handle_version_command(json: bool) {
    let app_data = saluki_metadata::get_app_details();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(app_data).expect("Unable to serialize version information.")
        )
    } else {
        println!("v{}-{}", app_data.version().raw(), app_data.git_hash())
    }
}
