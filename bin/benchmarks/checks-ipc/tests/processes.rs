use std::process::Command;

use serde_json::Value;

fn run(transport: &str, workload: &str, rate: u64, batch: usize, ring: usize, delay_us: u64) -> Value {
    let output = tempfile::tempdir().unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_checks-ipc-bench"))
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args([
            "run",
            "--transport",
            transport,
            "--workload",
            workload,
            "--rate",
            &rate.to_string(),
            "--batch",
            &batch.to_string(),
            "--ring",
            &ring.to_string(),
            "--consumer-delay-us",
            &delay_us.to_string(),
            "--warmup",
            "1",
            "--duration",
            "1",
            "--output",
            output.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
    let run_dir = std::fs::read_dir(output.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    serde_json::from_slice(&std::fs::read(run_dir.join("trial-000.json")).unwrap()).unwrap()
}

#[test]
fn metrics_and_logs_cross_processes_on_both_transports() {
    for transport in ["fit", "grpc"] {
        for workload in ["metrics", "logs"] {
            let result = run(transport, workload, 130, 64, 1 << 20, 0);
            assert_eq!(result["producer"]["scheduled"], 130);
            assert_eq!(result["producer"]["accepted"], 130);
            assert_eq!(result["consumer"]["decoded"], 130);
            assert_eq!(result["producer"]["checksum_sum"], result["consumer"]["checksum_sum"]);
            assert_eq!(result["pass"], true, "{result}");
        }
    }
}

#[test]
fn tiny_fit_ring_reports_rejection_without_losing_accepted_records() {
    let result = run("fit", "logs", 5_000, 64, 1_024, 2_000);
    assert_eq!(result["pass"], false);
    assert!(result["producer"]["rejected"].as_u64().unwrap() > 0, "{result}");
    assert_eq!(result["producer"]["accepted"], result["consumer"]["decoded"]);
}

#[test]
fn slow_grpc_consumer_reports_missed_schedule() {
    let result = run("grpc", "metrics", 1_000, 64, 1 << 20, 2_000);
    assert_eq!(result["pass"], false);
    assert!(result["producer"]["schedule_missed"].as_u64().unwrap() > 0, "{result}");
}
