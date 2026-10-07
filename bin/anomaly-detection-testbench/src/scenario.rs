//! Scenario discovery for the testbench.
//!
//! A scenarios directory holds one subdirectory per scenario. The Parquet data for a scenario
//! lives either in a `parquet/` subdirectory or, when no such subdirectory exists, directly under
//! the scenario directory as long as it contains at least one `*.parquet` file.

use std::path::{Path, PathBuf};

use crate::parquet::ParquetFormat;

/// Metadata about a discovered scenario.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scenario {
    /// Directory name of the scenario.
    pub name: String,
    /// Absolute or relative path to the scenario directory.
    pub path: PathBuf,
    /// Whether the scenario exposes Parquet data (via `parquet/` or `*.parquet` files directly).
    pub has_parquet: bool,
    /// Whether the scenario has a `logs/` directory.
    pub has_logs: bool,
    /// Whether the scenario has an `events/` directory.
    pub has_events: bool,
}

/// Lists the scenario subdirectories of `dir`, sorted by name.
///
/// Non-directory entries are ignored. Returns an I/O error if `dir` cannot be read.
pub fn discover_scenarios(dir: &Path) -> std::io::Result<Vec<Scenario>> {
    let mut scenarios = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }

        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        scenarios.push(Scenario {
            name,
            has_parquet: path.join("parquet").exists() || has_parquet_files(&path),
            has_logs: path.join("logs").exists(),
            has_events: path.join("events").exists(),
            path,
        });
    }

    scenarios.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(scenarios)
}

/// Returns the directory that holds a scenario's Parquet data, if any.
///
/// Prefers `<scenario>/parquet/`. Otherwise, when the scenario directory itself contains at least
/// one `*.parquet` file, returns the scenario directory. Returns `None` when neither holds data.
pub fn scenario_parquet_data_dir(scenario_path: &Path) -> Option<PathBuf> {
    let parquet = scenario_path.join("parquet");
    if parquet.exists() {
        return Some(parquet);
    }
    if has_parquet_files(scenario_path) {
        return Some(scenario_path.to_path_buf());
    }
    None
}

/// Auto-detects the Parquet layout of a data directory: v2 when `contexts.parquet` is present.
pub fn detect_format(data_dir: &Path) -> ParquetFormat {
    ParquetFormat::detect(data_dir)
}

fn has_parquet_files(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.ends_with(".parquet"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path, contents: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn discovers_scenarios_sorted_with_flags() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();

        touch(&root.join("beta/parquet/observer-metrics-0.parquet"), b"x");
        touch(&root.join("alpha/thing.parquet"), b"x");
        touch(&root.join("alpha/logs/keep"), b"");
        touch(&root.join("alpha/events/keep"), b"");
        touch(&root.join("gamma/notes.txt"), b"");

        let scenarios = discover_scenarios(root).unwrap();
        let names: Vec<&str> = scenarios.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["alpha", "beta", "gamma"]);

        let alpha = &scenarios[0];
        assert!(alpha.has_parquet, "direct *.parquet marks parquet data");
        assert!(alpha.has_logs);
        assert!(alpha.has_events);

        let beta = &scenarios[1];
        assert!(beta.has_parquet);
        assert!(!beta.has_logs);

        let gamma = &scenarios[2];
        assert!(!gamma.has_parquet);
    }

    #[test]
    fn parquet_data_dir_prefers_parquet_subdirectory() {
        let root = tempfile::tempdir().unwrap();
        let scenario = root.path().join("s");
        touch(&scenario.join("stray.parquet"), b"1234567890123456789012");
        touch(&scenario.join("parquet/observer-logs-0.parquet"), b"x");

        assert_eq!(scenario_parquet_data_dir(&scenario), Some(scenario.join("parquet")));
    }

    #[test]
    fn parquet_data_dir_falls_back_to_scenario_dir() {
        let root = tempfile::tempdir().unwrap();
        let scenario = root.path().join("s");
        touch(&scenario.join("metrics-0.parquet"), b"x");

        assert_eq!(scenario_parquet_data_dir(&scenario), Some(scenario.clone()));
        assert_eq!(scenario_parquet_data_dir(&root.path().join("missing")), None);
    }

    #[test]
    fn detects_v2_only_when_contexts_file_present() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        assert_eq!(detect_format(dir), crate::parquet::ParquetFormat::V1);
        touch(&dir.join("contexts.parquet"), b"x");
        assert_eq!(detect_format(dir), crate::parquet::ParquetFormat::V2);
    }
}
