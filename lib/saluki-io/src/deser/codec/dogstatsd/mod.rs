use saluki_context::{
    origin::OriginTagCardinality,
    tags::{BorrowedTag, RawTags},
};
use saluki_core::constants::datadog::{
    CARDINALITY_TAG_KEY, ENTITY_ID_IGNORE_VALUE, ENTITY_ID_TAG_KEY, HOST_TAG_KEY, JMX_CHECK_NAME_TAG_KEY,
};
use snafu::Snafu;

mod message;
pub use self::message::{parse_message_type, MessageType};

mod event;
pub use self::event::EventPacket;

mod helpers;
mod metric;
pub use self::metric::MetricPacket;

mod service_check;
pub use self::service_check::ServiceCheckPacket;

type NomParserError<'a> = nom::Err<nom::error::Error<&'a [u8]>>;

#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum ParseError {
    #[snafu(display("encountered error '{:?}' while processing message '{}'", kind, data))]
    Structural { kind: nom::error::ErrorKind, data: String },
}

impl<'a> From<NomParserError<'a>> for ParseError {
    fn from(err: NomParserError<'a>) -> Self {
        match err {
            nom::Err::Error(e) | nom::Err::Failure(e) => ParseError::Structural {
                kind: e.code,
                data: String::from_utf8_lossy(e.input).to_string(),
            },
            nom::Err::Incomplete(_) => unreachable!("dogstatsd codec only supports complete payloads"),
        }
    }
}

/// Well-known tags shared across DogStatsD payloads.
///
/// Well-known tags are tags which are generally set by the DogStatsD client to convey structured information
/// about the source of the metric, event, or service check.
#[derive(Default)]
pub struct WellKnownTags<'a> {
    pub hostname: Option<&'a str>,
    pub pod_uid: Option<&'a str>,
    pub jmx_check_name: Option<&'a str>,
    pub cardinality: Option<OriginTagCardinality>,
}

impl<'a> WellKnownTags<'a> {
    /// Extracts well-known tags from the raw tags of a DogStatsD payload.
    ///
    /// All fields default to `None`.
    fn from_raw_tags(tags: RawTags<'a>) -> Self {
        let mut well_known_tags = Self::default();

        for tag in tags {
            let tag = BorrowedTag::from(tag);
            match tag.name_and_value() {
                (HOST_TAG_KEY, Some(hostname)) => {
                    well_known_tags.hostname = Some(hostname);
                }
                (ENTITY_ID_TAG_KEY, Some(entity_id)) if entity_id != ENTITY_ID_IGNORE_VALUE => {
                    well_known_tags.pod_uid = Some(entity_id);
                }
                (JMX_CHECK_NAME_TAG_KEY, Some(name)) => {
                    well_known_tags.jmx_check_name = Some(name);
                }
                (CARDINALITY_TAG_KEY, Some(value)) => {
                    if let Ok(card) = OriginTagCardinality::try_from(value) {
                        well_known_tags.cardinality = Some(card);
                    }
                }
                _ => {}
            }
        }
        well_known_tags
    }
}

/// DogStatsD codec configuration.
#[derive(Clone, Debug)]
pub struct DogstatsdCodecConfiguration {
    permissive: bool,
    maximum_tag_length: usize,
    maximum_tag_count: usize,
    timestamps: bool,
}

impl DogstatsdCodecConfiguration {
    /// Sets whether or not the codec should operate in permissive mode.
    ///
    /// In permissive mode, the codec will attempt to parse as much of the input as possible, relying solely on
    /// structural markers (specific delimiting characters) to determine the boundaries of different parts of the
    /// payload. This allows for decoding payloads with invalid contents (e.g., characters that are valid UTF-8, but
    /// aren't within ASCII bounds, etc) such that the data plane can attempt to process them further.
    ///
    /// Permissive mode does not allow for decoding payloads with structural errors (e.g., missing delimiters, etc) or
    /// that cannot be safely handled internally (e.g., invalid UTF-8 characters for the metric name or tags).
    ///
    /// Defaults to `false`.
    pub fn with_permissive_mode(mut self, permissive: bool) -> Self {
        self.permissive = permissive;
        self
    }

    /// Sets the maximum tag length.
    ///
    /// This controls the number of bytes that are allowed for a single tag. If a tag exceeds this limit, it is
    /// truncated to the closest previous UTF-8 character boundary, in order to preserve UTF-8 validity.
    ///
    /// Defaults to no limit.
    pub fn with_maximum_tag_length(mut self, maximum_tag_length: usize) -> Self {
        self.maximum_tag_length = maximum_tag_length;
        self
    }

    /// Sets the maximum tag count.
    ///
    /// This is the maximum number of tags allowed for a single metric. If the number of tags exceeds this limit,
    /// remaining tags are simply ignored.
    ///
    /// Defaults to no limit.
    pub fn with_maximum_tag_count(mut self, maximum_tag_count: usize) -> Self {
        self.maximum_tag_count = maximum_tag_count;
        self
    }

    /// Sets whether or not timestamps are read from metrics.
    ///
    /// This is generally used in conjunction with aggregating metrics pipelines to control whether or not metrics are
    /// able to specify their own timestamp in order to be forwarded immediately without aggregation.
    ///
    /// Defaults to `true`.
    pub fn with_timestamps(mut self, timestamps: bool) -> Self {
        self.timestamps = timestamps;
        self
    }
}

impl Default for DogstatsdCodecConfiguration {
    fn default() -> Self {
        Self {
            maximum_tag_length: usize::MAX,
            maximum_tag_count: usize::MAX,
            timestamps: true,
            permissive: false,
        }
    }
}

/// A DogStatsD packet.
pub enum DogStatsDPacket<'a> {
    /// Metric.
    Metric(MetricPacket<'a>),

    /// Event.
    Event(EventPacket<'a>),

    /// Service check.
    ServiceCheck(ServiceCheckPacket<'a>),
}

/// A [DogStatsD][dsd] codec.
///
/// This codec is used to parse the DogStatsD protocol, which is a superset of the StatsD protocol. DogStatsD adds a
/// number of additional features, such as the ability to specify tags, send histograms directly, send service checks
/// and events (DataDog-specific), and more.
///
/// [dsd]: https://docs.datadoghq.com/developers/dogstatsd/
#[derive(Clone, Debug)]
pub struct DogstatsdCodec {
    config: DogstatsdCodecConfiguration,
}

impl DogstatsdCodec {
    /// Sets the given configuration for the codec.
    ///
    /// Different aspects of the codec's behavior (such as tag length, tag count, and timestamp parsing) can be
    /// controlled through its configuration. See [`DogstatsdCodecConfiguration`] for more information.
    pub fn from_configuration(config: DogstatsdCodecConfiguration) -> Self {
        Self { config }
    }

    pub fn decode_packet<'a>(&self, data: &'a [u8]) -> Result<DogStatsDPacket<'a>, ParseError> {
        match parse_message_type(data) {
            MessageType::Event => self.decode_event(data).map(DogStatsDPacket::Event),
            MessageType::ServiceCheck => self.decode_service_check(data).map(DogStatsDPacket::ServiceCheck),
            MessageType::MetricSample => self.decode_metric(data).map(DogStatsDPacket::Metric),
        }
    }

    fn decode_metric<'a>(&self, data: &'a [u8]) -> Result<MetricPacket<'a>, ParseError> {
        // Decode the payload and get the representative parts of the metric.
        // TODO: Can probably assert remaining is empty now.
        let (_remaining, parsed_packet) = self::metric::parse_dogstatsd_metric(data, &self.config)?;
        Ok(parsed_packet)
    }

    fn decode_event<'a>(&self, data: &'a [u8]) -> Result<EventPacket<'a>, ParseError> {
        let (_remaining, event) = self::event::parse_dogstatsd_event(data, &self.config)?;
        Ok(event)
    }

    fn decode_service_check<'a>(&self, data: &'a [u8]) -> Result<ServiceCheckPacket<'a>, ParseError> {
        let (_remaining, service_check) = self::service_check::parse_dogstatsd_service_check(data, &self.config)?;
        Ok(service_check)
    }
}

#[cfg(test)]
mod tests {
    use saluki_context::{origin::OriginTagCardinality, tags::RawTags};
    use saluki_core::constants::datadog::*;

    use crate::deser::codec::dogstatsd::WellKnownTags;

    #[test]
    fn well_known_tags_empty() {
        let tags = RawTags::new("", usize::MAX, usize::MAX);
        let wkt = WellKnownTags::from_raw_tags(tags);

        assert_eq!(wkt.cardinality, None);
        assert_eq!(wkt.hostname, None);
        assert_eq!(wkt.jmx_check_name, None);
        assert_eq!(wkt.pod_uid, None);
    }

    #[test]
    fn well_known_tags_cardinality() {
        let cases = [
            // Happy path.
            ("none", Some(OriginTagCardinality::None)),
            ("low", Some(OriginTagCardinality::Low)),
            ("orch", Some(OriginTagCardinality::Orchestrator)),
            ("orchestrator", Some(OriginTagCardinality::Orchestrator)),
            ("high", Some(OriginTagCardinality::High)),
            // Invalid values: either the wrong case, or just not even real cardinality levels.
            ("Low", None),
            ("MEDIUM", None),
            ("HiGH", None),
            ("fakelevel", None),
        ];

        for (tag_value, expected) in cases {
            let card_tag = format!("{}:{}", CARDINALITY_TAG_KEY, tag_value);
            let tags = RawTags::new(&card_tag, usize::MAX, usize::MAX);
            let wkt = WellKnownTags::from_raw_tags(tags);

            assert_eq!(wkt.cardinality, expected);
        }
    }

    #[test]
    fn well_known_tags_hostname() {
        let cases = [("", Some("")), ("localhost", Some("localhost"))];

        for (tag_value, expected) in cases {
            let host_tag = format!("{}:{}", HOST_TAG_KEY, tag_value);
            let tags = RawTags::new(&host_tag, usize::MAX, usize::MAX);
            let wkt = WellKnownTags::from_raw_tags(tags);

            assert_eq!(wkt.hostname, expected);
        }
    }

    #[test]
    fn well_known_tags_jmx_check_name() {
        let cases = [("", Some("")), ("check_name", Some("check_name"))];

        for (tag_value, expected) in cases {
            let jmx_tag = format!("{}:{}", JMX_CHECK_NAME_TAG_KEY, tag_value);
            let tags = RawTags::new(&jmx_tag, usize::MAX, usize::MAX);
            let wkt = WellKnownTags::from_raw_tags(tags);

            assert_eq!(wkt.jmx_check_name, expected);
        }
    }

    #[test]
    fn well_known_tags_pod_uid() {
        let cases = [("asdasda", Some("asdasda")), (ENTITY_ID_IGNORE_VALUE, None)];

        for (tag_value, expected) in cases {
            let pod_tag = format!("{}:{}", ENTITY_ID_TAG_KEY, tag_value);
            let tags = RawTags::new(&pod_tag, usize::MAX, usize::MAX);
            let wkt = WellKnownTags::from_raw_tags(tags);

            assert_eq!(wkt.pod_uid, expected);
        }
    }
}
