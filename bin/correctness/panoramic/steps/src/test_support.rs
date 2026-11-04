//! Fields shared by tests that need valid registered steps.

use panoramic_core::step::StepFields;

use crate::{
    config_key_equals::ConfigKeyEquals, exits_with::ExitsWith, http_check::HttpCheck,
    intake_has_forwarded_packets::IntakeHasForwardedPackets, intake_has_metric::IntakeHasMetric,
    port_listening::PortListening, process_stable_for::ProcessStableFor, send::Send, FileContains, LogContains,
    LogNotContains,
};

/// Returns fields that build the registered step `name`, on participant `subject`.
///
/// # Panics
///
/// Panics if `name` is not a registered step.
pub fn valid_fields(name: &str) -> StepFields {
    let yaml = match name {
        ConfigKeyEquals::NAME => "on: subject\nkey: domains.enabled\nvalue: true\nview: runtime\ntimeout: 30s",
        ExitsWith::NAME => "on: subject\nprocess: main\ncode: 1\ntimeout: 30s",
        FileContains::NAME => "on: subject\npath: /logs/output.log\npattern: ready\ntimeout: 30s",
        HttpCheck::NAME => "on: subject\nendpoint: http\npath: /metrics\nstatus: {equal: 200}\ntimeout: 30s",
        IntakeHasForwardedPackets::NAME => "on: subject\ncontains: ['packet']\ntimeout: 30s",
        IntakeHasMetric::NAME => "on: subject\nname: some.metric\ntimeout: 30s",
        LogContains::NAME => "on: subject\npattern: ready\ntimeout: 30s",
        LogNotContains::NAME => "on: subject\npattern: panic\nduring: 10s",
        PortListening::NAME => "on: subject\nendpoint: tcp\ntimeout: 30s",
        ProcessStableFor::NAME => "on: subject\nduration: 10s",
        Send::NAME => "on: subject\nendpoint: udp\npayload: 'metric:1|c'\ntimeout: 30s",
        other => panic!("missing valid fields for '{other}'"),
    };
    serde_saphyr::from_str(yaml).unwrap()
}

/// Constructs a process exit with the given code or signal.
#[cfg(test)]
pub(crate) fn exited(code: Option<i32>, signal: Option<i32>) -> panoramic_core::step::ExitObservation {
    panoramic_core::step::ExitObservation::Exited(panoramic_core::step::ExitStatus {
        code,
        signal,
        reason: None,
        recorded_at: None,
    })
}
