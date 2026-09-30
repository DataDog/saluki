//! Replays config-recorder corpus cases through this crate's config system. `loader` turns a case
//! into the exact `ConfigEvent` stream the Agent sent.
//!
//! The corpus and its contract (`lib/datadog-agent/config-recorder/docs/record.md`) are owned
//! elsewhere; this module only reads them through `datadog_agent_config_corpus::read`.

mod loader;
