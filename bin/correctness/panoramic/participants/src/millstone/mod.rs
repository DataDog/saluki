//! Paired load generators sharing a seed and container identity through the targets' flush window.

use std::collections::BTreeMap;

use panoramic_core::{
    artifact::ArtifactRole,
    endpoint::{ConsumedEndpoint, Delivery, EndpointKind},
    layout::{Os, Realization, RealizationKind},
    participant::{
        CaseValues, CollectDestination, CollectFile, EffectiveConfig, ExitChannel, OutputChannel, OutputStream,
        ParticipantType, ProcessSpec, RenderedConfig,
    },
    recipe::{ContainerRecipe, Recipe},
};
use serde_json::Value;

use crate::TOOLS_LINUX_IMAGE;

const CONFIG_FILE: &str = "millstone.yaml";
const CONFIG_DIR: &str = "/etc/millstone";
const STATE_DIR: &str = "/var/lib/millstone";
const WRAPPER_PATH: &str = "/usr/local/bin/panoramic-millstone.sh";
const WRAPPER: &[u8] = include_bytes!("assets/run.sh");
const TARGET_ENV: &str = "MILLSTONE_TARGET";
const BASELINE_TARGET_ENV: &str = "MILLSTONE_BASELINE_TARGET";

/// Two deterministic senders, kept alive after sending so targets can resolve their origin while flushing.
pub struct Millstone;

impl Millstone {
    /// Participant type name used in case files.
    pub const NAME: &'static str = "millstone";

    fn configuration(files: &BTreeMap<String, Vec<u8>>) -> Result<Value, String> {
        let contents = files
            .get(CONFIG_FILE)
            .ok_or_else(|| "millstone requires config.files.millstone.yaml with template: true".to_string())?;
        let contents =
            simdutf8::basic::from_utf8(contents).map_err(|error| format!("millstone.yaml must be UTF-8: {error}"))?;
        let config: Value =
            serde_saphyr::from_str(contents).map_err(|error| format!("cannot parse millstone.yaml: {error}"))?;
        if !config.is_object() {
            return Err("millstone.yaml must contain a mapping".into());
        }
        if config.get("target").and_then(Value::as_str).is_none_or(str::is_empty) {
            return Err("millstone.yaml key 'target' must be a nonempty string".into());
        }
        serde_json::from_value::<[u8; 32]>(config.get("seed").cloned().unwrap_or_default())
            .map_err(|_| "millstone.yaml key 'seed' must be an array of 32 bytes (0–255)".to_string())?;
        Ok(config)
    }
}

impl ParticipantType for Millstone {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn supports(&self) -> &'static [(Os, RealizationKind)] {
        &[(Os::Linux, RealizationKind::LinuxContainer)]
    }

    fn artifacts(&self, realization: Realization) -> &'static [ArtifactRole] {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer) => &[TOOLS_LINUX_IMAGE],
            _ => &[],
        }
    }

    fn recipe(&self, realization: Realization, config: &RenderedConfig) -> Result<Recipe, String> {
        if !self.supports().contains(&(realization.os, realization.kind)) {
            return Err(format!(
                "participant type '{}' has no recipe for {} on {}",
                Self::NAME,
                realization.kind.name(),
                realization.os.name(),
            ));
        }
        let mut sender_config = Self::configuration(config.files())?;
        let mut recipe = ContainerRecipe::new(TOOLS_LINUX_IMAGE)
            .with_entrypoint(["/bin/sh", WRAPPER_PATH])
            .with_command(["/usr/local/bin/millstone", CONFIG_DIR, STATE_DIR])
            .with_env(config.env().clone())
            .with_file(WRAPPER_PATH, 0o755, WRAPPER);
        for (process, key) in [("subject", TARGET_ENV), ("baseline", BASELINE_TARGET_ENV)] {
            let target = config
                .env()
                .get(key)
                .filter(|target| !target.is_empty())
                .ok_or_else(|| format!("millstone requires the {process} connection delivery '{key}'"))?;
            if process == "subject" && sender_config["target"].as_str() != Some(target) {
                return Err("millstone.yaml key 'target' must equal the delivered target URL".into());
            }
            // Re-serialize with only `target` changed; key order and formatting may differ
            sender_config["target"] = Value::String(target.clone());
            let contents = serde_saphyr::to_string(&sender_config)
                .map_err(|error| format!("cannot serialize {process} millstone.yaml: {error}"))?;
            recipe = recipe.with_file(format!("{CONFIG_DIR}/{process}.yaml"), 0o644, contents.into_bytes());
        }
        Ok(Recipe::Container(recipe))
    }

    fn collect(&self, realization: Realization) -> &'static [CollectFile] {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer) => &[
                CollectFile {
                    path: "/var/lib/millstone/baseline/input.jsonl.zst",
                    name: "input.jsonl.zst",
                    kind: "input",
                    destination: CollectDestination::Traffic,
                },
                CollectFile {
                    path: "/var/lib/millstone/baseline/input-manifest.json",
                    name: "input-manifest.json",
                    kind: "input_manifest",
                    destination: CollectDestination::Traffic,
                },
                CollectFile {
                    path: "/var/lib/millstone/subject/input-manifest.json",
                    name: "input-manifest.json",
                    kind: "subject_input_manifest",
                    destination: CollectDestination::Participant,
                },
            ],
            _ => &[],
        }
    }

    fn config_files(&self) -> &'static [&'static str] {
        &[CONFIG_FILE]
    }

    fn validate_config(&self, _: Realization, config: &CaseValues) -> Result<(), String> {
        if !config.files.get(CONFIG_FILE).is_some_and(|file| file.template) {
            return Err("millstone requires config.files.millstone.yaml with template: true".into());
        }
        let document = Self::configuration(&config.file_contents)?;
        let target = document["target"].as_str().expect("configuration checks target");
        if target
            .strip_prefix("{{")
            .and_then(|target| target.strip_suffix("}}"))
            .map(str::trim)
            != Some("self.connections.target.url")
        {
            return Err("millstone.yaml key 'target' must be '{{ self.connections.target.url }}'".into());
        }
        Ok(())
    }

    fn consumes(&self, realization: Realization, _: &EffectiveConfig) -> BTreeMap<String, ConsumedEndpoint> {
        if !self.supports().contains(&(realization.os, realization.kind)) {
            return BTreeMap::new();
        }
        let accepts = vec![
            EndpointKind::Tcp,
            EndpointKind::Udp,
            EndpointKind::Http,
            EndpointKind::Grpc,
            EndpointKind::UnixDatagram,
            EndpointKind::UnixStream,
        ];
        BTreeMap::from([
            (
                "baseline_target".into(),
                ConsumedEndpoint {
                    accepts: accepts.clone(),
                    required: true,
                    delivery: Delivery::Env {
                        keys: [BASELINE_TARGET_ENV.into()].into(),
                        value: |address| BTreeMap::from([(BASELINE_TARGET_ENV.into(), address.url().into())]),
                    },
                },
            ),
            (
                "target".into(),
                ConsumedEndpoint {
                    accepts,
                    required: true,
                    delivery: Delivery::Env {
                        keys: [TARGET_ENV.into()].into(),
                        value: |address| BTreeMap::from([(TARGET_ENV.into(), address.url().into())]),
                    },
                },
            ),
        ])
    }

    fn processes(&self) -> &'static [&'static str] {
        &["baseline", "subject"]
    }

    fn process_specs(&self, realization: Realization) -> &'static [ProcessSpec] {
        match (realization.os, realization.kind) {
            (Os::Linux, RealizationKind::LinuxContainer) => &[
                ProcessSpec {
                    name: "baseline",
                    exit: ExitChannel::Record { file: "baseline" },
                    output: OutputChannel::File {
                        file: "baseline.log",
                        stream: OutputStream::Merged,
                    },
                },
                ProcessSpec {
                    name: "subject",
                    exit: ExitChannel::Record { file: "subject" },
                    output: OutputChannel::File {
                        file: "subject.log",
                        stream: OutputStream::Merged,
                    },
                },
            ],
            _ => &[],
        }
    }
}

#[cfg(test)]
mod tests;
