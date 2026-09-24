//! Replacing the container images a test case declares, from the command line.
//!
//! A case names its images so it can run locally, where images are built under `saluki-images/`
//! tags. CI builds its images per commit and names them in a registry, so it runs the same case with
//! `--image-override <name>=<image>`. The names are the ones the test reports through
//! [`Test::images`]; `panoramic list --json` prints them.

use std::str::FromStr;

use saluki_error::{generic_error, GenericError};

use crate::test::Test;

/// One `name=image` pair from the command line.
#[derive(Clone, Debug)]
pub(crate) struct ImageOverride {
    /// Container name whose image this replaces.
    pub(crate) name: String,

    /// Image reference to use instead of the one the case declares.
    pub(crate) image: String,
}

impl FromStr for ImageOverride {
    type Err = String;

    /// Parses `name=image`, splitting at the first `=`.
    ///
    /// # Errors
    ///
    /// Returns an error when there is no `=`, or when either side of it is empty. An image
    /// reference may itself contain `=`, so only the first one separates the pair.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (name, image) = s
            .split_once('=')
            .ok_or_else(|| format!("expected <name>=<image>, got '{}'", s))?;

        if name.is_empty() || image.is_empty() {
            return Err(format!("expected <name>=<image>, got '{}'", s));
        }

        Ok(Self {
            name: name.to_string(),
            image: image.to_string(),
        })
    }
}

/// Applies image overrides to the tests selected for a run.
///
/// An override reaches every test that declares its name, so one flag covers the whole run. Tests
/// that declare no container by that name are left alone, which is how a run holding both suites
/// takes an override that only one of them has.
///
/// # Errors
///
/// Returns an error when a name is given twice, or when no test in the run declares it. Both are
/// reported rather than applied silently: an override nothing consumes means the run tests
/// something other than what the caller asked for.
pub(crate) fn apply(tests: &mut [Box<dyn Test>], overrides: &[ImageOverride]) -> Result<(), GenericError> {
    for (position, entry) in overrides.iter().enumerate() {
        if overrides[..position].iter().any(|earlier| earlier.name == entry.name) {
            return Err(generic_error!(
                "Image override '{}' is given more than once. Name each container at most once.",
                entry.name
            ));
        }

        let mut applied = false;
        for test in tests.iter_mut() {
            applied |= test.set_image(&entry.name, &entry.image);
        }

        if !applied {
            return Err(generic_error!(
                "No test in this run uses a container named '{}'. Available: {}.",
                entry.name,
                available_names(tests)
            ));
        }
    }

    Ok(())
}

/// Lists the container names the selected tests declare, for an error message.
fn available_names(tests: &[Box<dyn Test>]) -> String {
    let mut names: Vec<&str> = tests.iter().flat_map(|test| test.images().into_keys()).collect();
    names.sort_unstable();
    names.dedup();

    if names.is_empty() {
        "no test in this run declares a container image".to_string()
    } else {
        names.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::IntegrationConfig;
    use crate::correctness::config::Config as CorrectnessConfig;

    /// An integration case scoped to the linux runtime, with the intake sidecar enabled.
    fn integration_case() -> Box<dyn Test> {
        let mut case: IntegrationConfig = serde_yaml::from_str(
            r#"
name: integration-case
timeout: 10s
intake:
  enabled: true
procedure: []
"#,
        )
        .expect("integration case should parse");
        case.bind_to_runtime(crate::config::LINUX_RUNTIME);

        Box::new(case)
    }

    fn correctness_case() -> Box<dyn Test> {
        let case: CorrectnessConfig = serde_yaml::from_str(
            r#"
runtime: docker
analysis_mode: metrics
baseline:
  image: saluki-images/datadog-agent:testing-release
comparison:
  image: saluki-images/datadog-agent:testing-release
"#,
        )
        .expect("correctness case should parse");

        Box::new(case)
    }

    #[test]
    fn a_pair_splits_at_the_first_equals() {
        let parsed: ImageOverride = "millstone=registry.example.com/tools:abc=123"
            .parse()
            .expect("a pair should parse");

        assert_eq!(parsed.name, "millstone");
        assert_eq!(parsed.image, "registry.example.com/tools:abc=123");
    }

    #[test]
    fn a_pair_missing_a_name_or_an_image_is_rejected() {
        for entry in ["millstone", "=image", "millstone=", ""] {
            let error = entry
                .parse::<ImageOverride>()
                .expect_err("an incomplete pair should be rejected");
            assert!(error.contains("<name>=<image>"), "unexpected error: {error}");
        }
    }

    #[test]
    fn every_name_a_test_declares_can_be_overridden() {
        // The names in `images` and the names `set_image` accepts are the same interface, declared
        // by each test type. This is what keeps the two from drifting apart as containers are added.
        for mut test in [integration_case(), correctness_case()] {
            let names: Vec<String> = test.images().into_keys().map(str::to_string).collect();
            for name in names {
                assert!(
                    test.set_image(&name, "registry.example.com/replacement:v1"),
                    "'{}' is reported by images() but cannot be overridden",
                    name
                );
                assert_eq!(
                    test.images().get(name.as_str()),
                    Some(&"registry.example.com/replacement:v1".to_string()),
                    "overriding '{}' did not change the image it reports",
                    name
                );
            }
        }
    }

    #[test]
    fn an_override_reaches_only_the_tests_declaring_its_name() {
        let mut tests = vec![integration_case(), correctness_case()];

        apply(
            &mut tests,
            &[
                "millstone=registry.example.com/tools:abc123".parse().unwrap(),
                "container=registry.example.com/agent:abc123".parse().unwrap(),
            ],
        )
        .expect("both names are declared in this run");

        assert_eq!(
            tests[0].images().get("container"),
            Some(&"registry.example.com/agent:abc123".to_string())
        );
        assert_eq!(
            tests[1].images().get("millstone"),
            Some(&"registry.example.com/tools:abc123".to_string())
        );

        // The correctness case keeps the image its own file declares for a container the overrides
        // did not name.
        assert_eq!(
            tests[1].images().get("baseline"),
            Some(&"saluki-images/datadog-agent:testing-release".to_string())
        );
    }

    #[test]
    fn a_name_no_test_declares_is_rejected_with_the_names_that_exist() {
        let mut tests = vec![correctness_case()];

        let error = apply(
            &mut tests,
            &["millstoen=registry.example.com/tools:abc123".parse().unwrap()],
        )
        .expect_err("a misspelled name should be rejected");
        let error = format!("{error:?}");

        assert!(error.contains("millstoen"), "unexpected error: {error}");
        assert!(error.contains("millstone"), "unexpected error: {error}");
    }

    #[test]
    fn the_same_name_twice_is_rejected() {
        let mut tests = vec![correctness_case()];

        let error = apply(
            &mut tests,
            &[
                "millstone=registry.example.com/tools:first".parse().unwrap(),
                "millstone=registry.example.com/tools:second".parse().unwrap(),
            ],
        )
        .expect_err("a repeated name should be rejected");
        let error = format!("{error:?}");

        assert!(error.contains("more than once"), "unexpected error: {error}");
    }
}
