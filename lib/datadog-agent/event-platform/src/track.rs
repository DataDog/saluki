//! The tracks of the event platform intake.
//!
//! [`TRACKS`] matches the pipelines of the [event platform forwarder][forwarder] of the Datadog Agent, in the same
//! order. [`Track::headers`] matches the headers that its [HTTP destination][destination] sets, and the extra header of
//! the [`data-streams-message` track][data-streams].
//!
//! [forwarder]: https://github.com/DataDog/datadog-agent/tree/6f92b170b2d58cb92a6e08e3bc9ae111281bed0c/comp/forwarder/eventplatform/impl
//! [destination]: https://github.com/DataDog/datadog-agent/blob/6f92b170b2d58cb92a6e08e3bc9ae111281bed0c/comp/logs-library/client/http/destination.go#L403-L422
//! [data-streams]: https://github.com/DataDog/datadog-agent/blob/6f92b170b2d58cb92a6e08e3bc9ae111281bed0c/comp/forwarder/eventplatform/impl/epforwarder.go#L333-L344

use std::collections::BTreeMap;

// The defaults of the logs pipelines of the Datadog Agent, which a track keeps unless it sets its own. See
// https://github.com/DataDog/datadog-agent/blob/6f92b170b2d58cb92a6e08e3bc9ae111281bed0c/pkg/config/setup/constants/generated.go#L19-L25
const DEFAULT_BATCH_MAX_CONCURRENT_SEND: usize = 0;
const DEFAULT_BATCH_MAX_CONTENT_SIZE: usize = 5_000_000;
const DEFAULT_BATCH_MAX_SIZE: usize = 1000;
const DEFAULT_INPUT_CHAN_SIZE: usize = 100;

/// The value of the `DD-EVP-ORIGIN` header on the requests of every track.
const INTAKE_ORIGIN: &str = "agent";

/// The content type of the events of a track.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentType {
    /// JSON documents.
    Json,

    /// Protobuf messages.
    Protobuf,
}

impl ContentType {
    /// Returns the value of the `Content-Type` header for this content type.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Json => "application/json",
            Self::Protobuf => "application/x-protobuf",
        }
    }
}

/// How the Datadog Agent puts the events of a track into requests.
///
/// The Datadog Agent sends each protobuf event in a request of its own.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Strategy {
    /// Puts many events in one request. The body of the request is a JSON array of the events.
    Batch,

    /// Puts each event in a request of its own.
    Stream,
}

/// What the producer of events tells the intake about itself.
#[derive(Clone, Copy, Debug)]
pub struct Origin<'a> {
    /// The name of the producer, which the `User-Agent` header gives before the version. The Datadog Agent sends
    /// `datadog-agent`.
    pub product: &'a str,

    /// The version of the producer. The Datadog Agent sends its own version.
    pub version: &'a str,

    /// The hostname of the producer.
    pub hostname: &'a str,

    /// The ARN of the ECS task of the producer, if the producer runs on ECS Fargate.
    pub ecs_task_arn: Option<&'a str>,
}

/// A track of the event platform intake, as the Datadog Agent describes it.
///
/// The configuration of the Datadog Agent can change the values of the `default_*` fields.
#[derive(Debug)]
pub struct Track {
    /// The event type that a producer submits the events of the track with, such as `dbm-samples`.
    pub event_type: &'static str,

    /// The category that the Datadog Agent reports the track in, if the track has one.
    pub category: Option<&'static str>,

    /// The content type of the events.
    pub content_type: ContentType,

    /// How the Datadog Agent puts the events into requests.
    pub strategy: Strategy,

    /// The track type in the path of the intake API, such as `databasequery` in `/api/v2/databasequery`.
    pub intake_track_type: &'static str,

    /// The prefix of the host of the intake, before the site, such as `dbm-metrics-intake.`.
    pub hostname_endpoint_prefix: &'static str,

    /// The prefix of the configuration keys of the track, such as `database_monitoring.samples.`.
    pub endpoints_config_prefix: &'static str,

    /// The configuration key that turns on the track, if the track is off by default.
    pub enabled_by: Option<&'static str>,

    /// Whether each request has the `X-Datadog-Additional-Tags` header, with the hostname and the version of the
    /// producer.
    pub additional_tags: bool,

    /// The number of requests that the Datadog Agent sends at the same time. For `0`, it sends one request at a time.
    pub default_batch_max_concurrent_send: usize,

    /// The largest total size of the events in a request, in bytes, before compression.
    ///
    /// The Datadog Agent drops an event that is larger than this size by itself.
    pub default_batch_max_content_size: usize,

    /// The largest number of events in a request.
    pub default_batch_max_size: usize,

    /// The number of events that can wait for the track. The Datadog Agent drops an event when this queue is full.
    pub default_input_chan_size: usize,
}

impl Track {
    /// Returns the track with the given event type, if the Datadog Agent has one.
    pub fn by_event_type(event_type: &str) -> Option<&'static Track> {
        TRACKS.iter().find(|track| track.event_type == event_type)
    }

    /// Returns the HTTP headers that the Datadog Agent sets on each request of this track, except the headers that
    /// only the sender knows.
    ///
    /// The sender sets these headers:
    ///
    /// - `DD-API-KEY`
    /// - `Content-Encoding`
    /// - `dd-message-timestamp` and `dd-current-timestamp`
    pub fn headers(&self, origin: &Origin<'_>) -> BTreeMap<String, String> {
        let mut headers = BTreeMap::from([
            ("Content-Type".to_string(), self.content_type.as_str().to_string()),
            ("DD-EVP-ORIGIN".to_string(), INTAKE_ORIGIN.to_string()),
            ("DD-EVP-ORIGIN-VERSION".to_string(), origin.version.to_string()),
            (
                "User-Agent".to_string(),
                format!("{}/{}", origin.product, origin.version),
            ),
        ]);

        if self.additional_tags {
            let mut tags = format!("host:{},agent_version:{}", origin.hostname, origin.version);
            if let Some(ecs_task_arn) = origin.ecs_task_arn.filter(|arn| !arn.is_empty()) {
                tags.push_str(",task_arn:");
                tags.push_str(ecs_task_arn);
            }
            headers.insert("X-Datadog-Additional-Tags".to_string(), tags);
        }

        headers
    }
}

/// The tracks of the event platform intake that the Datadog Agent knows.
pub static TRACKS: &[Track] = &[
    Track {
        event_type: "dbm-samples",
        category: Some("DBM"),
        content_type: ContentType::Json,
        strategy: Strategy::Batch,
        intake_track_type: "databasequery",
        hostname_endpoint_prefix: "dbm-metrics-intake.",
        endpoints_config_prefix: "database_monitoring.samples.",
        enabled_by: None,
        additional_tags: false,
        default_batch_max_concurrent_send: 10,
        default_batch_max_content_size: 10_000_000,
        default_batch_max_size: DEFAULT_BATCH_MAX_SIZE,
        default_input_chan_size: 500,
    },
    Track {
        event_type: "dbm-metrics",
        category: Some("DBM"),
        content_type: ContentType::Json,
        strategy: Strategy::Batch,
        intake_track_type: "dbmmetrics",
        hostname_endpoint_prefix: "dbm-metrics-intake.",
        endpoints_config_prefix: "database_monitoring.metrics.",
        enabled_by: None,
        additional_tags: false,
        default_batch_max_concurrent_send: 10,
        default_batch_max_content_size: 20_000_000,
        default_batch_max_size: DEFAULT_BATCH_MAX_SIZE,
        default_input_chan_size: 500,
    },
    Track {
        event_type: "dbm-metadata",
        category: None,
        content_type: ContentType::Json,
        strategy: Strategy::Batch,
        intake_track_type: "dbmmetadata",
        hostname_endpoint_prefix: "dbm-metrics-intake.",
        endpoints_config_prefix: "database_monitoring.metrics.",
        enabled_by: None,
        additional_tags: false,
        default_batch_max_concurrent_send: 10,
        default_batch_max_content_size: 20_000_000,
        default_batch_max_size: DEFAULT_BATCH_MAX_SIZE,
        default_input_chan_size: 500,
    },
    Track {
        event_type: "dbm-activity",
        category: Some("DBM"),
        content_type: ContentType::Json,
        strategy: Strategy::Batch,
        intake_track_type: "dbmactivity",
        hostname_endpoint_prefix: "dbm-metrics-intake.",
        endpoints_config_prefix: "database_monitoring.activity.",
        enabled_by: None,
        additional_tags: false,
        default_batch_max_concurrent_send: 10,
        default_batch_max_content_size: 20_000_000,
        default_batch_max_size: DEFAULT_BATCH_MAX_SIZE,
        default_input_chan_size: 500,
    },
    Track {
        event_type: "dbm-health",
        category: None,
        content_type: ContentType::Json,
        strategy: Strategy::Batch,
        intake_track_type: "dbmhealth",
        hostname_endpoint_prefix: "dbm-metrics-intake.",
        endpoints_config_prefix: "database_monitoring.metrics.",
        enabled_by: None,
        additional_tags: false,
        default_batch_max_concurrent_send: 10,
        default_batch_max_content_size: 20_000_000,
        default_batch_max_size: DEFAULT_BATCH_MAX_SIZE,
        default_input_chan_size: 500,
    },
    Track {
        event_type: "dbm-column-statistics",
        category: None,
        content_type: ContentType::Json,
        strategy: Strategy::Batch,
        intake_track_type: "dbmcolumnstatistics",
        hostname_endpoint_prefix: "dbm-metrics-intake.",
        endpoints_config_prefix: "database_monitoring.metrics.",
        enabled_by: None,
        additional_tags: false,
        default_batch_max_concurrent_send: 10,
        default_batch_max_content_size: 20_000_000,
        default_batch_max_size: DEFAULT_BATCH_MAX_SIZE,
        default_input_chan_size: 500,
    },
    Track {
        event_type: "network-devices-metadata",
        category: Some("NDM"),
        content_type: ContentType::Json,
        strategy: Strategy::Batch,
        intake_track_type: "ndm",
        hostname_endpoint_prefix: "ndm-intake.",
        endpoints_config_prefix: "network_devices.metadata.",
        enabled_by: None,
        additional_tags: false,
        default_batch_max_concurrent_send: 10,
        default_batch_max_content_size: DEFAULT_BATCH_MAX_CONTENT_SIZE,
        default_batch_max_size: DEFAULT_BATCH_MAX_SIZE,
        default_input_chan_size: DEFAULT_INPUT_CHAN_SIZE,
    },
    Track {
        event_type: "network-devices-snmp-traps",
        category: Some("NDM"),
        content_type: ContentType::Json,
        strategy: Strategy::Batch,
        intake_track_type: "ndmtraps",
        hostname_endpoint_prefix: "snmp-traps-intake.",
        endpoints_config_prefix: "network_devices.snmp_traps.forwarder.",
        enabled_by: None,
        additional_tags: false,
        default_batch_max_concurrent_send: 10,
        default_batch_max_content_size: DEFAULT_BATCH_MAX_CONTENT_SIZE,
        default_batch_max_size: DEFAULT_BATCH_MAX_SIZE,
        default_input_chan_size: DEFAULT_INPUT_CHAN_SIZE,
    },
    Track {
        event_type: "network-devices-netflow",
        category: Some("NDM"),
        content_type: ContentType::Json,
        strategy: Strategy::Batch,
        intake_track_type: "ndmflow",
        hostname_endpoint_prefix: "ndmflow-intake.",
        endpoints_config_prefix: "network_devices.netflow.forwarder.",
        enabled_by: None,
        additional_tags: false,
        default_batch_max_concurrent_send: 10,
        default_batch_max_content_size: DEFAULT_BATCH_MAX_CONTENT_SIZE,
        default_batch_max_size: 250,
        default_input_chan_size: 10_000,
    },
    Track {
        event_type: "ndmconfig",
        category: Some("Network Config Management"),
        content_type: ContentType::Json,
        strategy: Strategy::Batch,
        intake_track_type: "ndmconfig",
        hostname_endpoint_prefix: "ndm-intake.",
        endpoints_config_prefix: "network_devices.config_management.forwarder.",
        enabled_by: None,
        additional_tags: false,
        default_batch_max_concurrent_send: 10,
        default_batch_max_content_size: DEFAULT_BATCH_MAX_CONTENT_SIZE,
        default_batch_max_size: DEFAULT_BATCH_MAX_SIZE,
        default_input_chan_size: DEFAULT_INPUT_CHAN_SIZE,
    },
    Track {
        event_type: "network-path",
        category: Some("Network Path"),
        content_type: ContentType::Json,
        strategy: Strategy::Batch,
        intake_track_type: "netpath",
        hostname_endpoint_prefix: "netpath-intake.",
        endpoints_config_prefix: "network_path.forwarder.",
        enabled_by: None,
        additional_tags: false,
        default_batch_max_concurrent_send: 10,
        default_batch_max_content_size: DEFAULT_BATCH_MAX_CONTENT_SIZE,
        default_batch_max_size: DEFAULT_BATCH_MAX_SIZE,
        default_input_chan_size: DEFAULT_INPUT_CHAN_SIZE,
    },
    Track {
        event_type: "container-lifecycle",
        category: Some("Container"),
        content_type: ContentType::Protobuf,
        strategy: Strategy::Stream,
        intake_track_type: "contlcycle",
        hostname_endpoint_prefix: "contlcycle-intake.",
        endpoints_config_prefix: "container_lifecycle.",
        enabled_by: None,
        additional_tags: false,
        default_batch_max_concurrent_send: 10,
        default_batch_max_content_size: DEFAULT_BATCH_MAX_CONTENT_SIZE,
        default_batch_max_size: DEFAULT_BATCH_MAX_SIZE,
        default_input_chan_size: DEFAULT_INPUT_CHAN_SIZE,
    },
    Track {
        event_type: "container-images",
        category: Some("Container"),
        content_type: ContentType::Protobuf,
        strategy: Strategy::Stream,
        intake_track_type: "contimage",
        hostname_endpoint_prefix: "contimage-intake.",
        endpoints_config_prefix: "container_image.",
        enabled_by: None,
        additional_tags: false,
        default_batch_max_concurrent_send: 10,
        default_batch_max_content_size: DEFAULT_BATCH_MAX_CONTENT_SIZE,
        default_batch_max_size: DEFAULT_BATCH_MAX_SIZE,
        default_input_chan_size: DEFAULT_INPUT_CHAN_SIZE,
    },
    Track {
        event_type: "kube-actions",
        category: Some("Kubernetes Actions"),
        content_type: ContentType::Json,
        strategy: Strategy::Batch,
        intake_track_type: "kubeactions",
        hostname_endpoint_prefix: "kubeops-intake.",
        endpoints_config_prefix: "kubeactions.forwarder.",
        enabled_by: Some("kubeactions.enabled"),
        additional_tags: false,
        default_batch_max_concurrent_send: 10,
        default_batch_max_content_size: DEFAULT_BATCH_MAX_CONTENT_SIZE,
        default_batch_max_size: DEFAULT_BATCH_MAX_SIZE,
        default_input_chan_size: DEFAULT_INPUT_CHAN_SIZE,
    },
    Track {
        event_type: "container-sbom",
        category: Some("SBOM"),
        content_type: ContentType::Protobuf,
        strategy: Strategy::Stream,
        intake_track_type: "sbom",
        hostname_endpoint_prefix: "sbom-intake.",
        endpoints_config_prefix: "sbom.",
        enabled_by: None,
        additional_tags: false,
        default_batch_max_concurrent_send: 10,
        default_batch_max_content_size: DEFAULT_BATCH_MAX_CONTENT_SIZE,
        default_batch_max_size: DEFAULT_BATCH_MAX_SIZE,
        default_input_chan_size: 1000,
    },
    Track {
        event_type: "genresources",
        category: Some("Generic Resources"),
        content_type: ContentType::Protobuf,
        strategy: Strategy::Stream,
        intake_track_type: "genresources",
        hostname_endpoint_prefix: "resources-intake.",
        endpoints_config_prefix: "genresources.",
        enabled_by: None,
        additional_tags: false,
        default_batch_max_concurrent_send: 10,
        default_batch_max_content_size: DEFAULT_BATCH_MAX_CONTENT_SIZE,
        default_batch_max_size: DEFAULT_BATCH_MAX_SIZE,
        default_input_chan_size: DEFAULT_INPUT_CHAN_SIZE,
    },
    Track {
        event_type: "synthetics",
        category: Some("Synthetics"),
        content_type: ContentType::Json,
        strategy: Strategy::Batch,
        intake_track_type: "synthetics",
        hostname_endpoint_prefix: "http-synthetics.",
        endpoints_config_prefix: "synthetics.forwarder.",
        enabled_by: None,
        additional_tags: false,
        default_batch_max_concurrent_send: 10,
        default_batch_max_content_size: DEFAULT_BATCH_MAX_CONTENT_SIZE,
        default_batch_max_size: DEFAULT_BATCH_MAX_SIZE,
        default_input_chan_size: DEFAULT_INPUT_CHAN_SIZE,
    },
    Track {
        event_type: "event-management",
        category: Some("Event Management"),
        content_type: ContentType::Json,
        // The intake of this track doesn't accept a JSON array.
        strategy: Strategy::Stream,
        intake_track_type: "events",
        hostname_endpoint_prefix: "event-management-intake.",
        endpoints_config_prefix: "event_management.forwarder.",
        enabled_by: None,
        additional_tags: false,
        default_batch_max_concurrent_send: DEFAULT_BATCH_MAX_CONCURRENT_SEND,
        default_batch_max_content_size: DEFAULT_BATCH_MAX_CONTENT_SIZE,
        default_batch_max_size: DEFAULT_BATCH_MAX_SIZE,
        default_input_chan_size: DEFAULT_INPUT_CHAN_SIZE,
    },
    Track {
        event_type: "data-streams-message",
        category: Some("Data Streams"),
        content_type: ContentType::Json,
        strategy: Strategy::Batch,
        intake_track_type: "data_streams_messages",
        hostname_endpoint_prefix: "trace.agent.",
        endpoints_config_prefix: "data_streams.forwarder.",
        enabled_by: None,
        additional_tags: true,
        default_batch_max_concurrent_send: 10,
        default_batch_max_content_size: DEFAULT_BATCH_MAX_CONTENT_SIZE,
        default_batch_max_size: DEFAULT_BATCH_MAX_SIZE,
        default_input_chan_size: 500,
    },
    Track {
        event_type: "do-query-results",
        category: Some("DO"),
        content_type: ContentType::Json,
        strategy: Strategy::Batch,
        intake_track_type: "query-actions",
        hostname_endpoint_prefix: "data-obs-intake.",
        endpoints_config_prefix: "data_observability.forwarder.",
        enabled_by: None,
        additional_tags: false,
        default_batch_max_concurrent_send: 10,
        default_batch_max_content_size: 20_000_000,
        default_batch_max_size: DEFAULT_BATCH_MAX_SIZE,
        default_input_chan_size: 500,
    },
    Track {
        event_type: "software-inventory",
        category: Some("EUDM"),
        content_type: ContentType::Json,
        strategy: Strategy::Batch,
        intake_track_type: "softinv",
        hostname_endpoint_prefix: "softinv-intake.",
        endpoints_config_prefix: "software_inventory.forwarder.",
        enabled_by: Some("software_inventory.enabled"),
        additional_tags: false,
        default_batch_max_concurrent_send: DEFAULT_BATCH_MAX_CONCURRENT_SEND,
        default_batch_max_content_size: DEFAULT_BATCH_MAX_CONTENT_SIZE,
        default_batch_max_size: DEFAULT_BATCH_MAX_SIZE,
        default_input_chan_size: DEFAULT_INPUT_CHAN_SIZE,
    },
    Track {
        event_type: "agentdiscovery",
        category: Some("Agent Discovery"),
        content_type: ContentType::Protobuf,
        strategy: Strategy::Stream,
        intake_track_type: "agentdiscovery",
        hostname_endpoint_prefix: "instrumentation-telemetry-intake.",
        endpoints_config_prefix: "config_files_discovery.forwarder.",
        enabled_by: None,
        additional_tags: false,
        default_batch_max_concurrent_send: 0,
        default_batch_max_content_size: 5_000_000,
        default_batch_max_size: 1000,
        default_input_chan_size: 100,
    },
    Track {
        event_type: "sds-result",
        category: Some("Data Security"),
        content_type: ContentType::Protobuf,
        strategy: Strategy::Stream,
        intake_track_type: "sdsresult",
        hostname_endpoint_prefix: "sds-intake.",
        endpoints_config_prefix: "sds_result.forwarder.",
        enabled_by: Some("data_security.enabled"),
        additional_tags: false,
        default_batch_max_concurrent_send: 10,
        default_batch_max_content_size: DEFAULT_BATCH_MAX_CONTENT_SIZE,
        default_batch_max_size: DEFAULT_BATCH_MAX_SIZE,
        default_input_chan_size: DEFAULT_INPUT_CHAN_SIZE,
    },
];

#[cfg(test)]
mod tests {
    use std::ptr;

    use super::*;

    #[test]
    fn each_event_type_finds_its_own_track() {
        // A second track with the same event type would never be found.
        for track in TRACKS {
            let found = Track::by_event_type(track.event_type).expect("should find a track for each event type");
            assert!(ptr::eq(found, track), "`{}` finds another track", track.event_type);
        }

        assert!(Track::by_event_type("unknown").is_none());
    }

    #[test]
    fn each_protobuf_track_sends_each_event_in_a_request_of_its_own() {
        for track in TRACKS
            .iter()
            .filter(|track| track.content_type == ContentType::Protobuf)
        {
            assert_eq!(track.strategy, Strategy::Stream, "`{}`", track.event_type);
        }
    }

    #[test]
    fn each_track_gets_the_headers_that_the_agent_sets() {
        struct Case {
            name: &'static str,
            event_type: &'static str,
            ecs_task_arn: Option<&'static str>,
            expected: &'static [(&'static str, &'static str)],
        }

        const TASK_ARN: &str = "arn:aws:ecs:us-east-1:123456789012:task/abc";

        let cases = [
            Case {
                name: "JSON track",
                event_type: "dbm-samples",
                ecs_task_arn: None,
                expected: &[
                    ("Content-Type", "application/json"),
                    ("DD-EVP-ORIGIN", "agent"),
                    ("DD-EVP-ORIGIN-VERSION", "7.70.0"),
                    ("User-Agent", "datadog-agent/7.70.0"),
                ],
            },
            Case {
                name: "protobuf track",
                event_type: "genresources",
                ecs_task_arn: Some(TASK_ARN),
                expected: &[
                    ("Content-Type", "application/x-protobuf"),
                    ("DD-EVP-ORIGIN", "agent"),
                    ("DD-EVP-ORIGIN-VERSION", "7.70.0"),
                    ("User-Agent", "datadog-agent/7.70.0"),
                ],
            },
            Case {
                name: "track with additional tags",
                event_type: "data-streams-message",
                ecs_task_arn: None,
                expected: &[
                    ("Content-Type", "application/json"),
                    ("DD-EVP-ORIGIN", "agent"),
                    ("DD-EVP-ORIGIN-VERSION", "7.70.0"),
                    ("User-Agent", "datadog-agent/7.70.0"),
                    ("X-Datadog-Additional-Tags", "host:my-host,agent_version:7.70.0"),
                ],
            },
            Case {
                name: "track with additional tags, on ECS Fargate",
                event_type: "data-streams-message",
                ecs_task_arn: Some(TASK_ARN),
                expected: &[
                    ("Content-Type", "application/json"),
                    ("DD-EVP-ORIGIN", "agent"),
                    ("DD-EVP-ORIGIN-VERSION", "7.70.0"),
                    ("User-Agent", "datadog-agent/7.70.0"),
                    (
                        "X-Datadog-Additional-Tags",
                        "host:my-host,agent_version:7.70.0,task_arn:arn:aws:ecs:us-east-1:123456789012:task/abc",
                    ),
                ],
            },
            Case {
                name: "track with additional tags, and an empty task ARN",
                event_type: "data-streams-message",
                ecs_task_arn: Some(""),
                expected: &[
                    ("Content-Type", "application/json"),
                    ("DD-EVP-ORIGIN", "agent"),
                    ("DD-EVP-ORIGIN-VERSION", "7.70.0"),
                    ("User-Agent", "datadog-agent/7.70.0"),
                    ("X-Datadog-Additional-Tags", "host:my-host,agent_version:7.70.0"),
                ],
            },
        ];

        for case in cases {
            let track = Track::by_event_type(case.event_type).expect("should find the track");
            let origin = Origin {
                product: "datadog-agent",
                version: "7.70.0",
                hostname: "my-host",
                ecs_task_arn: case.ecs_task_arn,
            };

            let expected = case
                .expected
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect::<BTreeMap<_, _>>();
            assert_eq!(track.headers(&origin), expected, "{}", case.name);
        }
    }
}
