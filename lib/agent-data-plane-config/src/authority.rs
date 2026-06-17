//! Runtime authority classification: where authoritative runtime Datadog config comes from.

/// Where the authoritative runtime Datadog configuration originates.
///
/// This is the "runtime authority" axis from the design. It is distinct from provider attachments
/// (which are long-lived capabilities) and from the source-language and destination axes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub enum RuntimeAuthority {
    /// Not connected to the Agent: local Datadog sources are the runtime authority, read once as a
    /// snapshot.
    LocalSnapshot,

    /// Connected to the Agent: the Agent config stream is the sole runtime authority for
    /// Datadog-schema config and pushes updates. Local Datadog sources are bootstrap-only.
    AgentStream,
}
