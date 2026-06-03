//! Framework-level environment overrides applied to every integration test target.
//!
//! Today this is just port isolation, but the module is named generically so future
//! framework-wide env defaults (test-specific API keys, host names, log levels, etc.) have a
//! natural home here.

use std::collections::HashMap;

/// Framework-level env overrides that move every default port the test target binds off its
/// canonical value, so concurrent test runs and any system Agent / system ADP on the host can
/// coexist with the per-test processes. Tests can override any of these via their `env` block;
/// tests that exercise specific port behavior (`adp-cmd-port`) supply their own values.
///
/// Naming convention: every default port that's 4 digits gets a `5` prepended (8125 -> 58125,
/// 5001 -> 55001, etc.). The GUI is disabled outright since we don't exercise it.
///
/// Note on env-var nesting: saluki-config (and figment) split env-var names on `__` to map to
/// nested config keys. Single-underscore env vars like `DD_DATA_PLANE_API_LISTEN_ADDRESS` map
/// to the flat key `data_plane_api_listen_address` and are silently ignored; we use double
/// underscores at every dot boundary for the deep ADP / OTLP keys below. The top-level Agent
/// env vars (`DD_CMD_PORT` etc.) are explicitly queried by the Agent so they don't need it.
pub fn port_isolation_env() -> HashMap<String, String> {
    HashMap::from([
        // ----- Core Agent ports -----
        // CMD/IPC API. Shared key between the Core Agent (listener) and ADP (IPC client).
        // `adp-cmd-port` overrides this via its `env` block to validate the non-default path.
        ("DD_CMD_PORT".to_string(), "55001".to_string()),
        // GUI — disabled outright. No integration test exercises it.
        ("DD_GUI_PORT".to_string(), "-1".to_string()),
        // expvar / APM / process / secondary IPC — not assertion targets, but the Agent will
        // still try to bind them on startup, so shift them out of the way.
        ("DD_EXPVAR_PORT".to_string(), "55000".to_string()),
        ("DD_APM_RECEIVER_PORT".to_string(), "58126".to_string()),
        ("DD_PROCESS_CONFIG_CMD_PORT".to_string(), "56062".to_string()),
        ("DD_AGENT_IPC_PORT".to_string(), "55004".to_string()),
        // DogStatsD UDP. In converged tests the Core Agent's DSD is disabled by
        // DD_DATA_PLANE_ENABLED so this mainly affects ADP (the actual listener) and the
        // bootstrap-mode Agent.
        ("DD_DOGSTATSD_PORT".to_string(), "58125".to_string()),
        // ----- ADP listen addresses ----- (URI-style; ListenAddress accepts `tcp://host:port`)
        (
            "DD_DATA_PLANE__API_LISTEN_ADDRESS".to_string(),
            "tcp://0.0.0.0:55100".to_string(),
        ),
        (
            "DD_DATA_PLANE__SECURE_API_LISTEN_ADDRESS".to_string(),
            "tcp://0.0.0.0:55101".to_string(),
        ),
        (
            "DD_DATA_PLANE__TELEMETRY_LISTEN_ADDR".to_string(),
            "tcp://0.0.0.0:55102".to_string(),
        ),
        // ----- OTLP receiver endpoints ----- (same shape as the Datadog Agent's OTLP env vars)
        (
            "DD_OTLP_CONFIG__RECEIVER__PROTOCOLS__GRPC__ENDPOINT".to_string(),
            "0.0.0.0:54317".to_string(),
        ),
        (
            "DD_OTLP_CONFIG__RECEIVER__PROTOCOLS__HTTP__ENDPOINT".to_string(),
            "0.0.0.0:54318".to_string(),
        ),
    ])
}
