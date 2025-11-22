use std::fmt;

mod event;
pub use self::event::EventPacket;

mod helpers;
pub use self::helpers::{parse_message_type, MessageType};

mod metric;
pub use self::metric::MetricPacket;

mod service_check;
pub use self::service_check::ServiceCheckPacket;

type NomParserError<'a> = nom::Err<nom::error::Error<&'a [u8]>>;

// This is the lowest sample rate that we consider to be "safe" with typical DogStatsD default settings.
//
// Our logic here is:
// - DogStatsD payloads are limited to 8KiB by default
// - a valid distribution metric could have a multi-value payload with ~4093 values (value of `1`, when factoring for protocol overhead)
// - to avoid overflow in resulting sketch, total count of all values must be less than or equal to 2^32
// - 2^32 / 4093 = 1,049,344... or 1,000,000 to be safe
// - 1 / 1,000,000 = 0.000001
const MINIMUM_SAFE_DEFAULT_SAMPLE_RATE: f64 = 0.000001;

/// Parser error.
#[derive(Debug)]
pub struct ParseError {
    kind: nom::error::ErrorKind,
    data: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "encountered error '{:?}' while processing message '{}'",
            self.kind, self.data
        )
    }
}

impl std::error::Error for ParseError {}

impl<'a> From<NomParserError<'a>> for ParseError {
    fn from(err: NomParserError<'a>) -> Self {
        match err {
            nom::Err::Error(e) | nom::Err::Failure(e) => Self {
                kind: e.code,
                data: String::from_utf8_lossy(e.input).to_string(),
            },
            nom::Err::Incomplete(_) => unreachable!("dogstatsd codec only supports complete payloads"),
        }
    }
}

/// A DogStatsD packet.
pub enum ParsedPacket<'a> {
    /// Metric.
    Metric(MetricPacket<'a>),

    /// Event.
    Event(EventPacket<'a>),

    /// Service check.
    ServiceCheck(ServiceCheckPacket<'a>),
}

/// DogStatsD codec configuration.
#[derive(Clone, Debug)]
pub struct DogstatsdCodecConfiguration {
    permissive: bool,
    maximum_tag_length: usize,
    maximum_tag_count: usize,
    timestamps: bool,
    minimum_sample_rate: f64,
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

    /// Sets the minimum sample rate.
    ///
    /// This is the minimum sample rate that is allowed for a metric payload. If the sample rate is less than this limit,
    /// the sample rate is clamped to this value and a log message is emitted.
    ///
    /// Defaults to `0.000001`.
    pub fn with_minimum_sample_rate(mut self, minimum_sample_rate: f64) -> Self {
        self.minimum_sample_rate = minimum_sample_rate;
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
            minimum_sample_rate: MINIMUM_SAFE_DEFAULT_SAMPLE_RATE,
        }
    }
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

    /// Decodes a DogStatsD packet from the given raw data.
    ///
    /// # Errors
    ///
    /// If the raw data is not a valid DogStatsD packet, an error is returned.
    pub fn decode_packet<'a>(&self, data: &'a [u8]) -> Result<ParsedPacket<'a>, ParseError> {
        match parse_message_type(data) {
            MessageType::Event => self.decode_event(data).map(ParsedPacket::Event),
            MessageType::ServiceCheck => self.decode_service_check(data).map(ParsedPacket::ServiceCheck),
            MessageType::MetricSample => self.decode_metric(data).map(ParsedPacket::Metric),
        }
    }

    fn decode_metric<'a>(&self, data: &'a [u8]) -> Result<MetricPacket<'a>, ParseError> {
        // Decode the payload and get the representative parts of the metric.
        // TODO: Can probably assert remaining is empty now.
        let (_remaining, metric) = self::metric::parse_dogstatsd_metric(data, &self.config)?;
        Ok(metric)
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
