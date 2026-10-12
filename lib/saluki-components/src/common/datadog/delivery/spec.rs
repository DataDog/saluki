//! The typed specifications of encode leases, recipients, and host settings.
//!
//! The installer resolves outputs, recipients, and host settings as plan parameters, which this crate cannot read: the
//! host application decodes them into these types, which the encode and delivery hosts run. Field names are what the
//! host application's parameter table names each parameter, in snake case.

use std::{fmt, num::NonZeroU8, sync::Arc, time::Duration};

use saluki_core::execution::EntryId;
use saluki_tls::TlsMinimumVersion;
use stringtheory::MetaString;

use crate::common::datadog::{
    config::ForwarderHttpProtocol,
    endpoints::MetricsRecipient,
    protocol::{MetricsPayloadInfo, MetricsPayloadType, MetricsProtocolVersion},
};

/// The slot of a recipient in its delivery host's recipient table.
///
/// The installer allocates slots per host and reuses one only after the recipient that held it retired, so a payload
/// names its recipients by slot.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RecipientSlot(u16);

impl RecipientSlot {
    /// Creates a slot.
    pub const fn new(slot: u16) -> Self {
        Self(slot)
    }

    /// Returns the numeric value of the slot.
    pub const fn get(self) -> u16 {
        self.0
    }
}

/// How control messages, acknowledgments, and host events address one recipient of a delivery host: its slot, its
/// identity, and its incarnation, which together name one I/O loop on one queue.
///
/// A host ignores, and logs, a message whose reference does not match its table, so a message repeated after an
/// installer restart never reaches another incarnation in the slot.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RecipientRef {
    /// The recipient's slot.
    pub slot: RecipientSlot,

    /// The recipient's identity, as the plan names it.
    pub id: MetaString,

    /// The recipient's incarnation, which the installer allocates per prepare.
    pub incarnation: u64,
}

impl fmt::Display for RecipientRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "'{}' (slot {}, incarnation {})",
            self.id,
            self.slot.get(),
            self.incarnation
        )
    }
}

/// Which payloads a recipient receives when a payload does not name its recipients itself.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum PayloadClasses {
    /// Metrics only: the alternate metrics intake natively, and every recipient a resources artifact declares.
    MetricsOnly,

    /// Every class of payload: metrics, logs, events, service checks, traces, and APM statistics, as the primary and
    /// additional endpoints natively receive.
    All,
}

/// Which metrics protocol variants a recipient takes in their V3 form.
///
/// Acceptance is exclusive per metric type: a recipient that takes V3 series takes no V2 series, and the reverse, and
/// likewise for sketches.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct V3Acceptance {
    /// Whether the recipient takes series in the V3 protocol.
    pub series: bool,

    /// Whether the recipient takes sketches in the V3 protocol.
    pub sketches: bool,
}

impl V3Acceptance {
    /// Returns whether a recipient with this acceptance takes a payload with the given metrics payload info: every
    /// payload without one, which is not a metrics payload or comes from a legacy encoder, and a metrics payload of
    /// the variant its type accepts.
    pub fn accepts(&self, info: Option<MetricsPayloadInfo>) -> bool {
        let Some(info) = info else {
            return true;
        };
        let v3 = match info.payload_type {
            MetricsPayloadType::Series => self.series,
            MetricsPayloadType::Sketches => self.sketches,
        };
        match info.version {
            MetricsProtocolVersion::V2 => !v3,
            MetricsProtocolVersion::V3 => v3,
        }
    }
}

/// The logs and traces intake authorities a Datadog intake recipient sends those payloads to, in place of its own
/// host, when its host names them.
#[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct IntakeAuthorities {
    /// The authority logs payloads go to, such as `agent-http-intake.logs.datadoghq.com`.
    pub logs: Option<MetaString>,

    /// The authority trace and APM statistics payloads go to, such as `trace.agent.datadoghq.com`.
    pub traces: Option<MetaString>,
}

/// How a recipient's requests are formed: which request mapper its I/O loop uses.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RecipientProtocol {
    /// The Datadog intake: the endpoint's scheme and authority (or the logs or traces authority), and the API key from
    /// the recipient's credential in `DD-API-KEY`, inserted at dispatch.
    DatadogIntake {
        /// The logs and traces authorities.
        authorities: IntakeAuthorities,

        /// Whether every request carries `Allow-Arbitrary-Tag-Value: true`.
        allow_arbitrary_tags: bool,

        /// Which metrics variants the recipient takes in their V3 form.
        v3: V3Acceptance,
    },

    /// The Cluster Agent: the path forced to `/series`, `DD-API-KEY` removed, and the bearer token from the
    /// recipient's credential inserted at dispatch; no retry of a `403`, no validation, V2 series of scalar kinds only.
    ClusterAgent,
}

impl RecipientProtocol {
    /// Returns which metrics variants the recipient takes in their V3 form: none for the Cluster Agent.
    pub fn v3(&self) -> V3Acceptance {
        match self {
            Self::DatadogIntake { v3, .. } => *v3,
            Self::ClusterAgent => V3Acceptance::default(),
        }
    }
}

/// Whether a recipient's queue spills to disk.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SpillPolicy {
    /// As the host's settings configure: to disk where disk storage is configured.
    #[default]
    Native,

    /// Never: queued work beyond the in-memory bound is dropped and counted.
    Never,
}

/// What a retiring recipient does with its queued work.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RetireClosure {
    /// As at native shutdown, after attempting queued work until the drain deadline.
    #[default]
    Native,

    /// Queued work is attempted until the drain deadline, then closed as at native shutdown.
    Attempt,

    /// Queued work is persisted where disk storage is configured, without further attempts, and otherwise dropped.
    Persist,

    /// Queued work is dropped and counted.
    Discard,
}

/// A recipient's delivery policy: its only knobs beyond its host's settings, within the host's native bounds.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RecipientPolicy {
    /// The bound of the recipient's in-memory retry queue, in bytes, at most the host's; `None` keeps the host's.
    pub queue_bytes: Option<u64>,

    /// Whether the recipient's queue spills to disk.
    pub spill: SpillPolicy,

    /// What the recipient does with its queued work when it retires.
    pub on_retire: RetireClosure,
}

/// Where a recipient comes from, which decides its task name and its queue directory.
///
/// Directory names are the native function of a recipient's identity, so a recipient added live gets the directory a
/// restart with that configuration would give it, and one that returns adopts its backlog.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RecipientOrigin {
    /// Native configuration: the forwarder's own recipient, as it resolves from configuration.
    Native(MetricsRecipient),

    /// A resources artifact, under the name it declares the recipient with, and the name of its credential.
    Declared {
        /// The recipient's declared name.
        name: MetaString,

        /// The name of the credential the recipient sends with, `<name>` of `credentials/<name>`.
        credential: MetaString,
    },
}

/// Everything a delivery host needs to prepare one recipient: its identity, endpoint, credential reference, classes,
/// protocol, and delivery policy.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RecipientSpec {
    /// The recipient's identity, as the plan names it.
    pub id: MetaString,

    /// The endpoint, as configured for a native recipient, or in canonical form for a declared one.
    pub endpoint: String,

    /// The credential reference the recipient's credential cell follows, through the host's credential source.
    pub credential: MetaString,

    /// Which payloads the recipient receives when a payload does not name its recipients.
    pub classes: PayloadClasses,

    /// How the recipient's requests are formed.
    pub protocol: RecipientProtocol,

    /// The recipient's delivery policy.
    pub policy: RecipientPolicy,

    /// Where the recipient comes from.
    pub origin: RecipientOrigin,
}

/// A delivery host's retry settings: how failed requests are retried and queued.
#[derive(Clone, Debug, PartialEq)]
pub struct RetrySettings {
    /// The backoff factor.
    pub backoff_factor: f64,

    /// The base of the backoff, in seconds.
    pub backoff_base: f64,

    /// The longest backoff, in seconds.
    pub backoff_max: f64,

    /// How much the error count decreases on each success.
    pub recovery_error_decrease_factor: u32,

    /// Whether a success resets the error count.
    pub recovery_reset: bool,

    /// The bound of each recipient's in-memory retry queue, in bytes.
    pub queue_max_size_bytes: u64,

    /// The bound of each recipient's persisted retry queue, in bytes.
    pub storage_max_size_bytes: u64,

    /// The ratio of the in-memory queue's bound above which entries are flushed to disk.
    pub flush_to_disk_mem_ratio: f64,

    /// The largest share of the disk the persisted queues may use.
    pub storage_max_disk_ratio: f64,

    /// How old, in days, a persisted file may get before it is removed.
    pub outdated_file_in_days: u32,
}

/// One proxy a delivery host's client sends through: its URL without user information, and the reference of the
/// credential that holds the user information.
///
/// The host follows the credential's live view through its [`CredentialSource`][super::CredentialSource]: when the
/// value changes, it builds a new client off its routing loop and swaps it in at a request boundary, keeping the old
/// client if the build fails. A proxy password rotation therefore applies without a change of settings, and the
/// password never appears in them.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProxyEndpoint {
    /// The proxy's URL, without user information.
    pub url: String,

    /// The reference of the credential whose value is the proxy's user information, `<user>:<password>`, such as
    /// `native:proxy.https`. A credential that is not configured sends no proxy authorization.
    pub credential: MetaString,
}

/// The proxies a delivery host's client sends through.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProxySettings {
    /// The proxy for `http` requests.
    pub http: Option<ProxyEndpoint>,

    /// The proxy for `https` requests.
    pub https: Option<ProxyEndpoint>,

    /// Hosts no proxy is used for.
    pub no_proxy: Vec<String>,

    /// Whether `no_proxy` matches domains with their subdomains, CIDR ranges, and the wildcard `*`, rather than only
    /// exact hosts.
    pub no_proxy_nonexact_match: bool,

    /// Whether requests to the cloud providers' metadata endpoints are proxied too, rather than always bypassing the
    /// proxies.
    pub use_proxy_for_cloud_metadata: bool,
}

impl ProxySettings {
    /// The reference of the native credential that holds the user information of the configured `http` proxy.
    pub const NATIVE_HTTP_CREDENTIAL: &'static str = "native:proxy.http";

    /// The reference of the native credential that holds the user information of the configured `https` proxy.
    pub const NATIVE_HTTPS_CREDENTIAL: &'static str = "native:proxy.https";
}

/// A delivery host's shared settings: its client profile, and its retry and concurrency settings.
///
/// Settings change how a recipient sends, never which payloads reach it, so a publication swaps them at a request
/// boundary; a change of the client profile rebuilds the host's client at prepare.
#[derive(Clone, Debug, PartialEq)]
pub struct DeliverySettings {
    /// The most concurrent requests of one recipient.
    pub endpoint_concurrency: usize,

    /// The multiplier of the request concurrency, which also sizes the idle connection pool.
    pub endpoint_concurrency_multiplier: usize,

    /// The most pending requests of one recipient.
    pub endpoint_buffer_size: usize,

    /// The timeout of a whole request.
    pub request_timeout: Duration,

    /// The retry settings.
    pub retry: RetrySettings,

    /// The proxies.
    pub proxy: ProxySettings,

    /// The HTTP protocol of outgoing requests.
    pub http_protocol: ForwarderHttpProtocol,

    /// How often connections are reset; zero never resets them.
    pub connection_reset_interval: Duration,

    /// Whether TLS certificate validation is disabled.
    pub skip_ssl_validation: bool,

    /// The lowest TLS protocol version accepted.
    pub min_tls_version: TlsMinimumVersion,

    /// The timeout of a TLS handshake; zero has none.
    pub tls_handshake_timeout: Duration,

    /// How often native API keys are validated.
    pub api_key_validation_interval: Duration,
}

/// Which variant a metrics lease encodes series in, besides V3.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum SeriesVariant {
    /// The V2 series protocol.
    #[default]
    V2,

    /// The V1 series protocol.
    V1,
}

/// Which protocol variants a metrics lease produces: those its resolved recipients accept.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct ProtocolVariants {
    /// Whether the lease encodes series in their V1 or V2 form ([`EncoderSettings::series`]).
    pub v2_series: bool,

    /// Whether the lease encodes sketches in their V2 form.
    pub v2_sketches: bool,

    /// Whether the lease encodes series in their V3 form.
    pub v3_series: bool,

    /// Whether the lease encodes sketches in their V3 form.
    pub v3_sketches: bool,
}

/// The settings of one metrics encoder stream: its protocol variants, compression, payload limits, and flush timeout.
#[derive(Clone, Debug, PartialEq)]
pub struct EncoderSettings {
    /// The protocol variants the stream produces.
    pub variants: ProtocolVariants,

    /// The protocol series are encoded in, besides V3.
    pub series: SeriesVariant,

    /// The compressor kind, such as `zstd` or `zlib`.
    pub compressor_kind: MetaString,

    /// The zstd compression level.
    pub zstd_compressor_level: i32,

    /// The compression level of V3 payloads; zero uses `zstd_compressor_level`.
    pub v3_compression_level: i32,

    /// The most metrics in one payload.
    pub max_metrics_per_payload: usize,

    /// The largest compressed payload, in bytes.
    pub max_payload_size: usize,

    /// The largest uncompressed payload, in bytes.
    pub max_uncompressed_payload_size: usize,

    /// The largest compressed series payload, in bytes.
    pub max_series_payload_size: usize,

    /// The largest uncompressed series payload, in bytes.
    pub max_series_uncompressed_payload_size: usize,

    /// The most points in one series payload.
    pub max_series_points_per_payload: usize,

    /// How long an open payload waits for more metrics before it is flushed.
    pub flush_timeout: Duration,

    /// Whether every metric is logged before it is encoded.
    pub log_payloads: bool,
}

/// The slots of the recipients a payload goes to, which a payload carries in its metadata.
///
/// A payload that carries recipients goes to exactly those, in slot order, and is never filtered by a recipient's
/// protocol settings: the lease resolved them for the payload's own variant. A payload without, from the native set's
/// payload classes or a legacy encoder, goes to the native set.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PayloadRecipients(Arc<[RecipientSlot]>);

impl PayloadRecipients {
    /// Creates the recipient list of a payload.
    pub fn new(slots: Arc<[RecipientSlot]>) -> Self {
        Self(slots)
    }

    /// Returns the slots, in order.
    pub fn slots(&self) -> &[RecipientSlot] {
        &self.0
    }
}

/// The recipients a lease resolved, per protocol variant: each list holds the recipients of the output that accept
/// that variant, so no recipient filters a payload it was resolved for.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ResolvedRecipients {
    /// The recipients of V1 or V2 series payloads.
    pub v2_series: PayloadRecipients,

    /// The recipients of V2 sketch payloads.
    pub v2_sketches: PayloadRecipients,

    /// The recipients of V3 series payloads.
    pub v3_series: PayloadRecipients,

    /// The recipients of V3 sketch payloads.
    pub v3_sketches: PayloadRecipients,
}

impl ResolvedRecipients {
    /// Returns the recipients of a payload with the given metrics payload info.
    pub fn for_payload(&self, info: MetricsPayloadInfo) -> &PayloadRecipients {
        match (info.version, info.payload_type) {
            (MetricsProtocolVersion::V2, MetricsPayloadType::Series) => &self.v2_series,
            (MetricsProtocolVersion::V2, MetricsPayloadType::Sketches) => &self.v2_sketches,
            (MetricsProtocolVersion::V3, MetricsPayloadType::Series) => &self.v3_series,
            (MetricsProtocolVersion::V3, MetricsPayloadType::Sketches) => &self.v3_sketches,
        }
    }
}

/// A lease's key as an encode host sees it: the output, and the digest of its binding.
///
/// Every control message addressed to a lease names `(EntryId, LeaseKeyDigest)`; a host ignores, and logs, a message
/// whose pair does not match its table.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LeaseKeyDigest {
    /// The output's identifier.
    pub output: MetaString,

    /// The SHA-256 digest of the output's binding.
    pub binding: [u8; 32],
}

/// Everything an encode host needs to run one lease: one output's encoder stream.
#[derive(Clone, Debug, PartialEq)]
pub struct EncodeLeaseSpec {
    /// The lease's identifier, which every batch addressed to it carries as its header's entry.
    pub lease: EntryId,

    /// The lease's key.
    pub key: LeaseKeyDigest,

    /// The component identifier the lease's encoder telemetry reports under: the output's native identifier.
    pub telemetry_id: MetaString,

    /// The encoder's settings.
    pub settings: EncoderSettings,

    /// The recipients of each protocol variant, which every payload of the lease carries.
    pub recipients: Arc<ResolvedRecipients>,

    /// The number of builder tasks the lease's buffers are spread over.
    pub builders: NonZeroU8,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slots(slots: &[u16]) -> PayloadRecipients {
        PayloadRecipients::new(slots.iter().copied().map(RecipientSlot::new).collect())
    }

    #[test]
    fn v3_acceptance_is_exclusive_per_metric_type_and_takes_every_other_payload() {
        let v3_series = V3Acceptance {
            series: true,
            sketches: false,
        };
        assert!(v3_series.accepts(Some(MetricsPayloadInfo::v3_series())));
        assert!(!v3_series.accepts(Some(MetricsPayloadInfo::v2_series())));
        assert!(v3_series.accepts(Some(MetricsPayloadInfo::v2_sketches())));
        assert!(!v3_series.accepts(Some(MetricsPayloadInfo::v3_sketches())));
        assert!(v3_series.accepts(None));
        assert_eq!(RecipientProtocol::ClusterAgent.v3(), V3Acceptance::default());
    }

    #[test]
    fn a_lease_gives_each_payload_the_recipients_of_its_own_variant() {
        let resolved = ResolvedRecipients {
            v2_series: slots(&[0]),
            v2_sketches: slots(&[0, 3]),
            v3_series: slots(&[3]),
            v3_sketches: slots(&[]),
        };
        assert_eq!(
            resolved.for_payload(MetricsPayloadInfo::v2_series()).slots(),
            &[RecipientSlot::new(0)]
        );
        assert_eq!(
            resolved.for_payload(MetricsPayloadInfo::v3_series()).slots(),
            &[RecipientSlot::new(3)]
        );
        assert_eq!(resolved.for_payload(MetricsPayloadInfo::v2_sketches()).slots().len(), 2);
        assert!(resolved
            .for_payload(MetricsPayloadInfo::v3_sketches())
            .slots()
            .is_empty());
    }
}
