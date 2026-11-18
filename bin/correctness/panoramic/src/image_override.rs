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

/// Explicit image choices applied while preparing cases, before test selection.
pub(crate) struct ImageOverrides<'a> {
    entries: &'a [ImageOverride],
}

impl<'a> ImageOverrides<'a> {
    pub(crate) fn new(entries: &'a [ImageOverride]) -> Self {
        Self { entries }
    }

    /// Chooses an override or the image declared by the file or harness.
    pub(crate) fn image(&self, name: &str, default: &str) -> String {
        self.entries
            .iter()
            .find(|entry| entry.name == name)
            .map_or(default, |entry| entry.image.as_str())
            .to_string()
    }

    /// Applies explicit overrides to correctness file fields before case construction.
    pub(crate) fn apply_correctness(&self, config: &mut crate::correctness::config::Config) {
        use crate::correctness::case::{
            BASELINE_IMAGE_NAME, COMPARISON_IMAGE_NAME, INTAKE_IMAGE_NAME, MILLSTONE_IMAGE_NAME,
        };
        config.baseline.image = self.image(BASELINE_IMAGE_NAME, &config.baseline.image);
        config.comparison.image = self.image(COMPARISON_IMAGE_NAME, &config.comparison.image);
        config.datadog_intake.image = self.image(INTAKE_IMAGE_NAME, &config.datadog_intake.image);
        config.millstone.image = self.image(MILLSTONE_IMAGE_NAME, &config.millstone.image);
    }

    /// Rejects duplicate or unmatched names across all runtime-eligible cases, before selection.
    pub(crate) fn validate(&self, tests: &[Box<dyn Test>]) -> Result<(), GenericError> {
        for (position, entry) in self.entries.iter().enumerate() {
            if self.entries[..position]
                .iter()
                .any(|earlier| earlier.name == entry.name)
            {
                return Err(generic_error!(
                    "Image override '{}' is given more than once. Name each container at most once.",
                    entry.name
                ));
            }
            if !tests.iter().any(|test| test.images().contains_key(entry.name.as_str())) {
                return Err(generic_error!(
                    "No test in this run uses a container named '{}'. Available: {}.",
                    entry.name,
                    available_names(tests)
                ));
            }
        }
        Ok(())
    }
}

/// Lists the container names the discovered tests declare, for an error message.
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

    /// An integration case scoped to the linux runtime, with or without the intake sidecar.
    fn integration_case(overrides: &ImageOverrides<'_>, intake: bool) -> Box<dyn Test> {
        let case: IntegrationConfig = serde_saphyr::from_str(&format!(
            r#"
name: integration-case
timeout: 10s
intake:
  enabled: {intake}
procedure: []
"#
        ))
        .expect("integration case should parse");
        let settings = crate::integration::IntegrationSettings::new(crate::config::LINUX_RUNTIME, overrides);
        Box::new(crate::integration::IntegrationTestCase::new(case, &settings))
    }

    #[test]
    fn a_pair_splits_at_the_first_equals() {
        let parsed: ImageOverride = "container=registry.example.com/agent:abc=123"
            .parse()
            .expect("a pair should parse");

        assert_eq!(parsed.name, "container");
        assert_eq!(parsed.image, "registry.example.com/agent:abc=123");
    }

    #[test]
    fn a_pair_missing_a_name_or_an_image_is_rejected() {
        for entry in ["container", "=image", "container=", ""] {
            let error = entry
                .parse::<ImageOverride>()
                .expect_err("an incomplete pair should be rejected");
            assert!(error.contains("<name>=<image>"), "unexpected error: {error}");
        }
    }

    #[test]
    fn every_name_a_test_declares_can_be_overridden() {
        let defaults = ImageOverrides::new(&[]);
        let names: Vec<String> = integration_case(&defaults, true)
            .images()
            .into_keys()
            .map(str::to_string)
            .collect();
        let entries: Vec<ImageOverride> = names
            .into_iter()
            .map(|name| ImageOverride {
                name,
                image: "registry.example.com/replacement:v1".to_string(),
            })
            .collect();
        let overrides = ImageOverrides::new(&entries);
        let tests = [integration_case(&overrides, true)];
        overrides.validate(&tests).unwrap();
        for test in tests {
            for image in test.images().values() {
                assert_eq!(image, "registry.example.com/replacement:v1");
            }
        }
    }

    #[test]
    fn an_override_reaches_only_the_tests_declaring_its_name() {
        let entries = [
            "intake=registry.example.com/tools:abc123".parse().unwrap(),
            "container=registry.example.com/agent:abc123".parse().unwrap(),
        ];
        let overrides = ImageOverrides::new(&entries);
        let tests = vec![integration_case(&overrides, true), integration_case(&overrides, false)];
        overrides.validate(&tests).expect("both names are declared in this run");

        assert_eq!(
            tests[0].images().get("intake"),
            Some(&"registry.example.com/tools:abc123".to_string())
        );
        assert_eq!(
            tests[1].images().get("container"),
            Some(&"registry.example.com/agent:abc123".to_string())
        );
        assert_eq!(tests[1].images().get("intake"), None);
    }

    #[test]
    fn a_name_no_test_declares_is_rejected_with_the_names_that_exist() {
        let entries = ["contaner=registry.example.com/agent:abc123".parse().unwrap()];
        let overrides = ImageOverrides::new(&entries);
        let tests = vec![integration_case(&overrides, true)];
        let error = overrides
            .validate(&tests)
            .expect_err("a misspelled name should be rejected");
        let error = format!("{error:?}");

        assert!(error.contains("contaner"), "unexpected error: {error}");
        assert!(error.contains("container"), "unexpected error: {error}");
    }

    #[test]
    fn the_same_name_twice_is_rejected() {
        let entries = [
            "container=registry.example.com/agent:first".parse().unwrap(),
            "container=registry.example.com/agent:second".parse().unwrap(),
        ];
        let overrides = ImageOverrides::new(&entries);
        let tests = vec![integration_case(&overrides, true)];
        let error = overrides
            .validate(&tests)
            .expect_err("a repeated name should be rejected");
        let error = format!("{error:?}");

        assert!(error.contains("more than once"), "unexpected error: {error}");
    }
}
