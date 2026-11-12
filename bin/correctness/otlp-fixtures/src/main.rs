//! Builds the `static_base64` OTLP corpora used by some correctness cases.
//!
//! Each corpus is built here and written into the `static_base64` payload of the case's
//! `millstone.yaml`. After changing a fixture, regenerate the payloads with:
//!
//! ```text
//! make generate-correctness-fixtures
//! ```
//!
//! CI runs the same target and fails if it changes any committed payload.

use std::{
    path::{Path, PathBuf},
    process::ExitCode,
};

use base64::Engine as _;
use saluki_error::{generic_error, ErrorContext as _, GenericError};

mod common;
mod logs;

fn case_millstone_config(case: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../test/correctness/cases")
        .join(case)
        .join("millstone.yaml")
}

/// Writes `payload` into the case's `static_base64` payload, returning whether the file changed.
fn write_fixture(case: &str, payload: &[u8]) -> Result<bool, GenericError> {
    let path = case_millstone_config(case);
    let config = std::fs::read_to_string(&path).with_error_context(|| format!("reading {}", path.display()))?;
    let expected = base64::engine::general_purpose::STANDARD.encode(payload);

    let prefix = "static_base64: \"";
    let start = config
        .find(prefix)
        .map(|idx| idx + prefix.len())
        .ok_or_else(|| generic_error!("{}: no `static_base64` payload", path.display()))?;
    let end = start
        + config[start..]
            .find('"')
            .ok_or_else(|| generic_error!("{}: unterminated `static_base64` payload", path.display()))?;

    if config[start..end] == expected {
        return Ok(false);
    }
    let updated = format!("{}{}{}", &config[..start], expected, &config[end..]);
    std::fs::write(&path, updated).with_error_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}

/// Builds the encoded request for a fixture.
type FixtureBuilder = fn() -> Vec<u8>;

fn main() -> ExitCode {
    let fixtures: &[(&str, FixtureBuilder)] = &[
        ("otlp-logs-attributes", logs::attributes_fixture),
        ("otlp-logs-severity-body", logs::severity_body_fixture),
        ("otlp-logs-resource-host", logs::resource_host_fixture),
    ];

    let mut failed = false;
    for (case, build) in fixtures {
        match write_fixture(case, &build()) {
            Ok(true) => println!("updated {}", case),
            Ok(false) => {}
            Err(e) => {
                eprintln!("{:#}", e);
                failed = true;
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
