//! Log filtering helpers.

use saluki_error::{generic_error, GenericError};
use tracing_subscriber::filter::Targets;

/// Environment variable conventionally used to configure log filtering for Rust applications.
const RUST_LOG_ENV_VAR: &str = "RUST_LOG";

/// Parses a comma-separated list of log filter directives.
///
/// Each directive is a bare level (`info`), which sets the default level for all targets, or a target with a level
/// (`saluki=debug`), which applies to that target and any target nested under it (for example, `saluki::io`). A bare
/// target (`saluki`) enables all levels for that target. Empty directives are ignored, so an input with no directives
/// disables all logging.
///
/// # Errors
///
/// If any directive is malformed, or uses span or field filters (`target[span{field=value}]=level`), an error is
/// returned.
pub fn parse_filter_directives(directives: &str) -> Result<Targets, GenericError> {
    // `Targets` would parse an empty directive as the `error` level, and a span filter as a literal target name that
    // never matches, so we drop the former and reject the latter before handing the rest over.
    let directives = directives.split(',').filter(|d| !d.is_empty()).collect::<Vec<_>>();
    if let Some(directive) = directives.iter().find(|d| d.contains('[')) {
        return Err(generic_error!(
            "Invalid log filter directive `{}`: span and field filters are not supported.",
            directive
        ));
    }

    if directives.is_empty() {
        return Ok(Targets::new());
    }

    directives
        .join(",")
        .parse()
        .map_err(|e| generic_error!("Invalid log filter directives: {}", e))
}

/// Builds a log filter from the `RUST_LOG` environment variable, falling back to `default`.
///
/// `RUST_LOG` is parsed with [`parse_filter_directives`]. If it's unset or empty, `default` is returned. If it can't be
/// parsed, a warning is written to standard error -- logging isn't initialized yet at this point -- and `default` is
/// returned.
pub fn filter_from_env(default: Targets) -> Targets {
    filter_from_env_value(std::env::var(RUST_LOG_ENV_VAR).ok(), default)
}

fn filter_from_env_value(value: Option<String>, default: Targets) -> Targets {
    match value {
        Some(directives) if !directives.is_empty() => match parse_filter_directives(&directives) {
            Ok(filter) => filter,
            Err(e) => {
                eprintln!(
                    "warning: ignoring {}=`{}` ({}); using default log filter `{}`",
                    RUST_LOG_ENV_VAR, directives, e, default
                );
                default
            }
        },
        _ => default,
    }
}

#[cfg(test)]
mod tests {
    use tracing_subscriber::filter::LevelFilter;

    use super::*;

    #[test]
    fn filter_directives_combine_default_and_per_target_levels() {
        let targets = parse_filter_directives("info,saluki=debug,saluki::io=trace").expect("valid directives");

        assert_eq!(targets.default_level(), Some(LevelFilter::INFO));
        assert_eq!(targets.to_string(), "saluki::io=trace,saluki=debug,info");
    }

    #[test]
    fn filter_directives_ignore_empty_directives() {
        // A trailing or doubled comma must not introduce a default level for every other target.
        let targets = parse_filter_directives("saluki=debug,,").expect("valid directives");
        assert_eq!(targets.default_level(), None);
        assert_eq!(targets.to_string(), "saluki=debug");

        let targets = parse_filter_directives(",").expect("no directives");
        assert_eq!(targets.default_level(), None);
        assert_eq!(targets.to_string(), "");
    }

    #[test]
    fn filter_directives_reject_span_and_field_filters() {
        for directives in [
            "[span]=debug",
            "saluki[span]=debug",
            "info,saluki[{field}]=debug",
            "saluki[span{field=value}]=debug",
        ] {
            let error = match parse_filter_directives(directives) {
                Ok(targets) => panic!("`{directives}` should be rejected, got `{targets}`"),
                Err(error) => error,
            };
            assert!(
                error.to_string().contains("span and field filters are not supported"),
                "unexpected error message for `{directives}`: {error}"
            );
        }
    }

    #[test]
    fn filter_directives_reject_invalid_levels() {
        assert!(parse_filter_directives("saluki=verbose").is_err());
        assert!(parse_filter_directives("saluki=debug=trace").is_err());
    }

    #[test]
    fn filter_from_env_uses_default_when_unset_or_empty() {
        let default = Targets::new().with_default(LevelFilter::INFO);

        assert_eq!(filter_from_env_value(None, default.clone()), default);
        assert_eq!(filter_from_env_value(Some(String::new()), default.clone()), default);
    }

    #[test]
    fn filter_from_env_replaces_default_with_valid_directives() {
        let default = Targets::new().with_default(LevelFilter::INFO);

        let filter = filter_from_env_value(Some("saluki=debug".to_string()), default);
        assert_eq!(filter.to_string(), "saluki=debug");
    }

    #[test]
    fn filter_from_env_falls_back_to_default_on_invalid_directives() {
        let default = Targets::new().with_default(LevelFilter::INFO);

        let filter = filter_from_env_value(Some("saluki[span]=debug".to_string()), default.clone());
        assert_eq!(filter, default);
    }
}
