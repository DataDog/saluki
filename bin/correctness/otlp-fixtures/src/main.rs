//! Builds the static OTLP payloads used by some correctness and integration cases.
//!
//! A payload goes in one of two places. A correctness case's sender config, such as its `millstone.yaml`, holds the
//! payload as a `static_base64` string. The config must be named `millstone*.yaml`, must already exist, and must hold a
//! `static_base64: ""` placeholder. A body file, such as the bodies of the `otlp-rcv-http` case, holds the raw bytes of
//! the payload. The file must already exist, and can be empty. After changing a fixture, regenerate the payloads with:
//!
//! ```text
//! make generate-correctness-fixtures
//! ```
//!
//! CI runs the same target and fails if it changes any committed payload.

use std::{path::Path, process::ExitCode};

use base64::Engine as _;
use saluki_error::{generic_error, ErrorContext as _, GenericError};

mod common;
mod logs;
mod metrics;
mod rcv_http;
mod routing;
mod traces;

/// Writes `payload` into the `static_base64` payload of the sender config at `target`, a path relative to `root`.
///
/// Returns whether the file changed.
fn write_fixture(root: &Path, target: &str, payload: &[u8]) -> Result<bool, GenericError> {
    let path = root.join(target);
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
    if !(name.starts_with("millstone") && name.ends_with(".yaml")) {
        return Err(generic_error!(
            "{}: sender configs must be named `millstone*.yaml` so millstone's config parse test covers them",
            path.display()
        ));
    }
    if !path.is_file() {
        return Err(generic_error!(
            "{}: no such file; create it with a `static_base64: \"\"` placeholder",
            path.display()
        ));
    }
    let config = std::fs::read_to_string(&path).with_error_context(|| format!("reading {}", path.display()))?;
    let expected = base64::engine::general_purpose::STANDARD.encode(payload);

    let prefix = "static_base64: \"";
    let start = config.find(prefix).map(|idx| idx + prefix.len()).ok_or_else(|| {
        generic_error!(
            "{}: no `static_base64` placeholder; add `static_base64: \"\"` to the payload",
            path.display()
        )
    })?;
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

/// Writes `body` as the contents of the body file at `target`, a path relative to `root`.
///
/// Returns whether the file changed.
fn write_body(root: &Path, target: &str, body: &[u8]) -> Result<bool, GenericError> {
    let path = root.join(target);
    if !path.is_file() {
        return Err(generic_error!(
            "{}: no such file; create an empty file for the body",
            path.display()
        ));
    }
    let current = std::fs::read(&path).with_error_context(|| format!("reading {}", path.display()))?;
    if current == body {
        return Ok(false);
    }
    std::fs::write(&path, body).with_error_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}

/// Builds the encoded request for a fixture.
type FixtureBuilder = fn() -> Vec<u8>;

fn main() -> ExitCode {
    let fixtures: &[(&str, FixtureBuilder)] = &[
        (
            "test/correctness/cases/otlp-logs-attributes/millstone.yaml",
            logs::attributes_fixture,
        ),
        (
            "test/correctness/cases/otlp-logs-enrichment/millstone.yaml",
            logs::enrichment_fixture,
        ),
        (
            "test/correctness/cases/otlp-logs-severity-body/millstone.yaml",
            logs::severity_body_fixture,
        ),
        (
            "test/correctness/cases/otlp-logs-resource-host/millstone.yaml",
            logs::resource_host_fixture,
        ),
        (
            "test/correctness/cases/otlp-metrics/millstone-control.yaml",
            metrics::control_fixture,
        ),
        (
            "test/correctness/cases/otlp-metrics-as-type-rate/millstone.yaml",
            metrics::as_type_rate_fixture,
        ),
        (
            "test/correctness/cases/otlp-metrics-configured-tags/millstone.yaml",
            metrics::configured_tags_fixture,
        ),
        (
            "test/correctness/cases/otlp-metrics-core-resource-attributes/millstone.yaml",
            metrics::core_resource_attributes_fixture,
        ),
        (
            "test/correctness/cases/otlp-metrics-cumulative-monotonic/millstone.yaml",
            metrics::cumulative_monotonic_fixture,
        ),
        (
            "test/correctness/cases/otlp-metrics-ec2-host/millstone.yaml",
            metrics::ec2_host_fixture,
        ),
        (
            "test/correctness/cases/otlp-metrics-fargate/millstone.yaml",
            metrics::fargate_fixture,
        ),
        (
            "test/correctness/cases/otlp-metrics-gcp-host/millstone.yaml",
            metrics::gcp_host_fixture,
        ),
        (
            "test/correctness/cases/otlp-metrics-histogram-counters/millstone.yaml",
            metrics::histogram_fixture,
        ),
        (
            "test/correctness/cases/otlp-metrics-histogram-nobuckets/millstone.yaml",
            metrics::histogram_fixture,
        ),
        (
            "test/correctness/cases/otlp-metrics-host/millstone.yaml",
            metrics::host_fixture,
        ),
        (
            "test/correctness/cases/otlp-metrics-kafka-parity/millstone.yaml",
            metrics::kafka_parity_fixture,
        ),
        (
            "test/correctness/cases/otlp-metrics-resource-attributes/millstone.yaml",
            metrics::resource_attributes_fixture,
        ),
        (
            "test/correctness/cases/otlp-metrics-scope-metadata/millstone.yaml",
            metrics::scope_metadata_fixture,
        ),
        (
            "test/correctness/cases/otlp-metrics-summary-mode/millstone.yaml",
            metrics::summary_fixture,
        ),
        (
            "test/correctness/cases/otlp-metrics-v3/millstone.yaml",
            metrics::v3_fixture,
        ),
        (
            "test/correctness/cases/otlp-traces-conversion/millstone.yaml",
            traces::conversion_fixture,
        ),
        (
            "test/correctness/cases/otlp-traces-defaults/millstone.yaml",
            traces::defaults_fixture,
        ),
        (
            "test/correctness/cases/otlp-traces-obfuscation/millstone.yaml",
            traces::obfuscation_fixture,
        ),
        (
            "test/correctness/cases/otlp-traces-ets-decisions/millstone.yaml",
            traces::ets_fixture,
        ),
        (
            "test/correctness/cases/otlp-traces-sampling/millstone.yaml",
            traces::sampling_fixture,
        ),
        (
            "test/correctness/cases/otlp-traces-stats/millstone.yaml",
            traces::stats_fixture,
        ),
        (
            "test/integration/cases/otlp-routing-mixed-grpc/millstone-metrics-grpc.yaml",
            || routing::metrics_fixture("mixed-grpc", "grpc"),
        ),
        (
            "test/integration/cases/otlp-routing-mixed-grpc/millstone-traces-grpc.yaml",
            || routing::traces_fixture("mixed-grpc", "grpc", 1),
        ),
        (
            "test/integration/cases/otlp-routing-mixed-grpc/millstone-logs-grpc.yaml",
            || routing::logs_fixture("mixed-grpc", "grpc"),
        ),
        (
            "test/integration/cases/otlp-routing-mixed-http/millstone-metrics-http.yaml",
            || routing::metrics_fixture("mixed-http", "http"),
        ),
        (
            "test/integration/cases/otlp-routing-mixed-http/millstone-traces-http.yaml",
            || routing::traces_fixture("mixed-http", "http", 2),
        ),
        (
            "test/integration/cases/otlp-routing-mixed-http/millstone-logs-http.yaml",
            || routing::logs_fixture("mixed-http", "http"),
        ),
        (
            "test/integration/cases/otlp-routing-internal/millstone-metrics-grpc.yaml",
            || routing::metrics_fixture("internal", "grpc"),
        ),
        (
            "test/integration/cases/otlp-routing-internal/millstone-traces-grpc.yaml",
            || routing::traces_fixture("internal", "grpc", 3),
        ),
        (
            "test/integration/cases/otlp-routing-internal/millstone-logs-grpc.yaml",
            || routing::logs_fixture("internal", "grpc"),
        ),
        (
            "test/integration/cases/otlp-routing-internal/millstone-metrics-http.yaml",
            || routing::metrics_fixture("internal", "http"),
        ),
        (
            "test/integration/cases/otlp-routing-internal/millstone-traces-http.yaml",
            || routing::traces_fixture("internal", "http", 4),
        ),
        (
            "test/integration/cases/otlp-routing-internal/millstone-logs-http.yaml",
            || routing::logs_fixture("internal", "http"),
        ),
        (
            "test/integration/cases/otlp-traces-endpoints/millstone-metrics-split.yaml",
            || routing::metrics_fixture("endpoints-split", "http"),
        ),
        (
            "test/integration/cases/otlp-traces-endpoints/millstone-traces-split.yaml",
            || routing::traces_fixture("endpoints-split", "http", 5),
        ),
        (
            "test/integration/cases/otlp-traces-endpoints/millstone-metrics-skipmain.yaml",
            || routing::metrics_fixture("endpoints-skipmain", "http"),
        ),
        (
            "test/integration/cases/otlp-traces-endpoints/millstone-traces-skipmain.yaml",
            || routing::traces_fixture("endpoints-skipmain", "http", 6),
        ),
        (
            "test/integration/cases/otlp-traces-endpoints/millstone-metrics-opw.yaml",
            || routing::metrics_fixture("endpoints-opw", "http"),
        ),
        (
            "test/integration/cases/otlp-traces-endpoints/millstone-traces-opw.yaml",
            || routing::traces_fixture("endpoints-opw", "http", 7),
        ),
        (
            "test/integration/cases/otlp-traces-endpoints-mrf/millstone-metrics-adp.yaml",
            || routing::metrics_fixture("endpoints-mrf-adp", "http"),
        ),
        (
            "test/integration/cases/otlp-traces-endpoints-mrf/millstone-traces-adp.yaml",
            || routing::traces_fixture("endpoints-mrf-adp", "http", 8),
        ),
        (
            "test/integration/cases/otlp-traces-endpoints-mrf/millstone-metrics-agent.yaml",
            || routing::metrics_fixture("endpoints-mrf-agent", "http"),
        ),
        (
            "test/integration/cases/otlp-traces-endpoints-mrf/millstone-traces-agent.yaml",
            || routing::traces_fixture("endpoints-mrf-agent", "http", 9),
        ),
    ];

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let mut failed = false;
    let mut report = |target: &str, written: Result<bool, GenericError>| match written {
        Ok(true) => println!("updated {}", target),
        Ok(false) => {}
        Err(e) => {
            eprintln!("{:#}", e);
            failed = true;
        }
    };
    for (target, build) in fixtures {
        report(target, write_fixture(&root, target, &build()));
    }
    for (name, body) in rcv_http::bodies() {
        let target = format!("{}/{}", rcv_http::BODIES_DIR, name);
        report(&target, write_body(&root, &target, &body));
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAYLOAD: &[u8] = b"fixture payload";
    const ENCODED: &str = "Zml4dHVyZSBwYXlsb2Fk";

    fn root_with(file: &str, contents: &str) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
        root
    }

    #[test]
    fn write_fixture_fills_the_placeholder_in_a_named_file() {
        let root = root_with(
            "cases/case/millstone-traces-grpc.yaml",
            "payload:\n  static_base64: \"\"\nvolume: 1\n",
        );

        assert!(write_fixture(root.path(), "cases/case/millstone-traces-grpc.yaml", PAYLOAD).unwrap());

        let written = std::fs::read_to_string(root.path().join("cases/case/millstone-traces-grpc.yaml")).unwrap();
        assert_eq!(
            written,
            format!("payload:\n  static_base64: \"{}\"\nvolume: 1\n", ENCODED)
        );
    }

    #[test]
    fn write_fixture_replaces_a_stale_payload_and_then_reports_no_change() {
        let root = root_with("millstone.yaml", "static_base64: \"c3RhbGU=\"\n");

        assert!(write_fixture(root.path(), "millstone.yaml", PAYLOAD).unwrap());
        assert!(!write_fixture(root.path(), "millstone.yaml", PAYLOAD).unwrap());

        let written = std::fs::read_to_string(root.path().join("millstone.yaml")).unwrap();
        assert_eq!(written, format!("static_base64: \"{}\"\n", ENCODED));
    }

    #[test]
    fn write_fixture_refuses_a_missing_file() {
        let root = tempfile::tempdir().unwrap();

        let error = write_fixture(root.path(), "cases/case/millstone-missing.yaml", PAYLOAD).unwrap_err();

        let message = format!("{:#}", error);
        assert!(message.contains("cases/case/millstone-missing.yaml"), "{}", message);
        assert!(message.contains("no such file"), "{}", message);
        assert!(!root.path().join("cases").exists());
    }

    #[test]
    fn write_fixture_refuses_a_name_millstone_does_not_test() {
        let root = root_with("cases/case/traces-grpc.yaml", "static_base64: \"\"\n");

        let error = write_fixture(root.path(), "cases/case/traces-grpc.yaml", PAYLOAD).unwrap_err();

        assert!(format!("{:#}", error).contains("millstone*.yaml"));
        assert_eq!(
            std::fs::read_to_string(root.path().join("cases/case/traces-grpc.yaml")).unwrap(),
            "static_base64: \"\"\n"
        );
    }

    #[test]
    fn write_fixture_refuses_a_file_without_a_placeholder() {
        let contents = "volume: 1\n";
        let root = root_with("millstone.yaml", contents);

        let error = write_fixture(root.path(), "millstone.yaml", PAYLOAD).unwrap_err();

        let message = format!("{:#}", error);
        assert!(message.contains("millstone.yaml"), "{}", message);
        assert!(message.contains("no `static_base64` placeholder"), "{}", message);
        assert_eq!(
            std::fs::read_to_string(root.path().join("millstone.yaml")).unwrap(),
            contents
        );
    }

    #[test]
    fn write_body_replaces_the_contents_and_then_reports_no_change() {
        let root = root_with("cases/case/bodies/body.pb", "");

        assert!(write_body(root.path(), "cases/case/bodies/body.pb", b"\x0a\x00").unwrap());
        assert!(!write_body(root.path(), "cases/case/bodies/body.pb", b"\x0a\x00").unwrap());

        assert_eq!(
            std::fs::read(root.path().join("cases/case/bodies/body.pb")).unwrap(),
            b"\x0a\x00"
        );
    }

    #[test]
    fn write_body_replaces_a_stale_body() {
        let root = root_with("body.json", "{\"stale\":true}\n");

        assert!(write_body(root.path(), "body.json", b"{}\n").unwrap());

        assert_eq!(std::fs::read(root.path().join("body.json")).unwrap(), b"{}\n");
    }

    #[test]
    fn write_body_refuses_a_missing_file() {
        let root = tempfile::tempdir().unwrap();

        let error = write_body(root.path(), "cases/case/bodies/body.pb", b"body").unwrap_err();

        let message = format!("{:#}", error);
        assert!(message.contains("cases/case/bodies/body.pb"), "{}", message);
        assert!(message.contains("no such file"), "{}", message);
        assert!(!root.path().join("cases").exists());
    }

    #[test]
    fn write_fixture_refuses_an_unterminated_payload() {
        let contents = "static_base64: \"abc\n";
        let root = root_with("millstone.yaml", contents);

        let error = write_fixture(root.path(), "millstone.yaml", PAYLOAD).unwrap_err();

        assert!(format!("{:#}", error).contains("unterminated"));
        assert_eq!(
            std::fs::read_to_string(root.path().join("millstone.yaml")).unwrap(),
            contents
        );
    }
}
