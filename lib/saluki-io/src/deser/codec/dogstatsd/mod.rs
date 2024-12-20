use nom::{
    branch::alt,
    bytes::complete::{tag, take, take_while1},
    character::complete::{u32 as parse_u32, u64 as parse_u64, u8 as parse_u8},
    combinator::{all_consuming, map, map_res},
    error::{Error, ErrorKind},
    number::complete::double,
    sequence::{delimited, preceded, separated_pair, terminated},
    IResult,
};
use saluki_context::BorrowedTag;
use saluki_core::constants::datadog::{
    CARDINALITY_TAG_KEY, ENTITY_ID_IGNORE_VALUE, ENTITY_ID_TAG_KEY, JMX_CHECK_NAME_TAG_KEY,
};
use saluki_event::{
    eventd::{AlertType, EventD, Priority},
    metric::*,
    service_check::{CheckStatus, ServiceCheck},
};
use snafu::Snafu;

mod message;
pub use self::message::{parse_message_type, MessageType};

type NomParserError<'a> = nom::Err<nom::error::Error<&'a [u8]>>;

enum MetricType {
    Count,
    Gauge,
    Set,
    Timer,
    Histogram,
    Distribution,
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
        let (_remaining, parsed_packet) = parse_dogstatsd_metric(data, &self.config)?;
        Ok(parsed_packet)
    }

    fn decode_event(&self, data: &[u8]) -> Result<EventD, ParseError> {
        let (_remaining, event) = parse_dogstatsd_event(data, &self.config)?;
        Ok(event)
    }

    fn decode_service_check(&self, data: &[u8]) -> Result<ServiceCheck, ParseError> {
        let (_remaining, service_check) = parse_dogstatsd_service_check(data, &self.config)?;
        Ok(service_check)
    }
}

pub enum ParsedPacket<'a> {
    Metric(MetricPacket<'a>),
    Event(EventD),
    ServiceCheck(ServiceCheck),
}

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

fn parse_dogstatsd_metric<'a>(
    input: &'a [u8], config: &DogstatsdCodecConfiguration,
) -> IResult<&'a [u8], MetricPacket<'a>> {
    // We always parse the metric name and value(s) first, where value is both the kind (counter, gauge, etc) and the
    // actual value itself.
    let metric_name_parser = if config.permissive {
        permissive_metric_name
    } else {
        ascii_alphanum_and_seps
    };
    let (remaining, (metric_name, (metric_type, raw_metric_values))) =
        separated_pair(metric_name_parser, tag(":"), raw_metric_values)(input)?;

    // At this point, we may have some of this additional data, and if so, we also then would have a pipe separator at
    // the very front, which we'd want to consume before going further.
    //
    // After that, we simply split the remaining bytes by the pipe separator, and then try and parse each chunk to see
    // if it's any of the protocol extensions we know of.
    let mut maybe_sample_rate = None;
    let mut maybe_tags = None;
    let mut maybe_container_id = None;
    let mut maybe_timestamp = None;

    let remaining = if !remaining.is_empty() {
        let (mut remaining, _) = tag("|")(remaining)?;

        while let Some((chunk, tail)) = split_at_delimiter(remaining, b'|') {
            if chunk.is_empty() {
                break;
            }

            match chunk[0] {
                // Sample rate: indicates client-side sampling of this metric which will need to be "reinflated" at some
                // point downstream to calculate the true metric value.
                b'@' => {
                    let (_, sample_rate) =
                        all_consuming(preceded(tag("@"), map_res(double, SampleRate::try_from)))(chunk)?;
                    maybe_sample_rate = Some(sample_rate);
                }
                // Tags: additional tags to be added to the metric.
                b'#' => {
                    let (_, tags) = all_consuming(preceded(tag("#"), metric_tags(config)))(chunk)?;
                    maybe_tags = Some(tags);
                }
                // Container ID: client-provided container ID for the container that this metric originated from.
                b'c' if chunk.len() > 1 && chunk[1] == b':' => {
                    let (_, container_id) = all_consuming(preceded(tag("c:"), container_id))(chunk)?;
                    maybe_container_id = Some(container_id);
                }
                // Timestamp: client-provided timestamp for the metric, relative to the Unix epoch, in seconds.
                b'T' => {
                    if config.timestamps {
                        let (_, timestamp) = all_consuming(preceded(tag("T"), unix_timestamp))(chunk)?;
                        maybe_timestamp = Some(timestamp);
                    }
                }
                _ => {
                    // We don't know what this is, so we just skip it.
                    //
                    // TODO: Should we throw an error, warn, or be silently permissive?
                }
            }

            remaining = tail;
        }

        // TODO: Similarly to the above comment, should having any remaining data here cause us to throw an error, warn,
        // or be silently permissive?

        remaining
    } else {
        remaining
    };

    let (num_points, mut metric_values) = metric_values_from_raw(raw_metric_values, metric_type, maybe_sample_rate)?;

    // If we got a timestamp, apply it to all metric values.
    if let Some(timestamp) = maybe_timestamp {
        metric_values.set_timestamp(timestamp);
    }

    let tags = maybe_tags.unwrap_or_else(TagSplitter::empty);

    let mut pod_uid = None;
    let mut cardinality = None;
    let mut jmx_check_name = None;
    for tag in &tags {
        let tag = BorrowedTag::from(tag);
        match tag.name_and_value() {
            (ENTITY_ID_TAG_KEY, Some(entity_id)) if entity_id != ENTITY_ID_IGNORE_VALUE => {
                pod_uid = Some(entity_id);
            }
            (JMX_CHECK_NAME_TAG_KEY, Some(name)) => {
                jmx_check_name = Some(name);
            }
            (CARDINALITY_TAG_KEY, Some(value)) => {
                if let Ok(card) = OriginTagCardinality::try_from(value) {
                    cardinality = Some(card);
                }
            }
            _ => {}
        }
    }

    Ok((
        remaining,
        MetricPacket {
            metric_name,
            tags,
            values: metric_values,
            num_points,
            timestamp: maybe_timestamp,
            container_id: maybe_container_id,
            pod_uid,
            cardinality,
            jmx_check_name,
        },
    ))
}

pub struct MetricPacket<'a> {
    pub metric_name: &'a str,
    pub tags: TagSplitter<'a>,
    pub values: MetricValues,
    pub num_points: u64,
    pub timestamp: Option<u64>,
    pub container_id: Option<&'a str>,
    pub pod_uid: Option<&'a str>,
    pub cardinality: Option<OriginTagCardinality>,
    pub jmx_check_name: Option<&'a str>,
}

fn parse_dogstatsd_event<'a>(input: &'a [u8], config: &DogstatsdCodecConfiguration) -> IResult<&'a [u8], EventD> {
    // We parse the title length and text length from `_e{<TITLE_UTF8_LENGTH>,<TEXT_UTF8_LENGTH>}:`
    let (remaining, (title_len, text_len)) = delimited(
        tag(message::EVENT_PREFIX),
        separated_pair(parse_u32, tag(b","), parse_u32),
        tag(b"}:"),
    )(input)?;

    // Title and Text are the required fields of an event.
    if title_len == 0 || text_len == 0 {
        return Err(nom::Err::Error(Error::new(input, ErrorKind::Verify)));
    }

    let (remaining, (raw_title, raw_text)) = separated_pair(take(title_len), tag(b"|"), take(text_len))(remaining)?;

    let title = match simdutf8::basic::from_utf8(raw_title) {
        Ok(title) => message::clean_data(title),
        Err(_) => return Err(nom::Err::Error(Error::new(raw_title, ErrorKind::Verify))),
    };

    let text = match simdutf8::basic::from_utf8(raw_text) {
        Ok(text) => message::clean_data(text),
        Err(_) => return Err(nom::Err::Error(Error::new(raw_text, ErrorKind::Verify))),
    };

    // At this point, we may have some of this additional data, and if so, we also then would have a pipe separator at
    // the very front, which we'd want to consume before going further.
    //
    // After that, we simply split the remaining bytes by the pipe separator, and then try and parse each chunk to see
    // if it's any of the protocol extensions we know of.
    //
    // Priority and Alert Type have default values
    let mut maybe_priority = Some(saluki_event::eventd::Priority::Normal);
    let mut maybe_alert_type = Some(saluki_event::eventd::AlertType::Info);
    let mut maybe_timestamp = None;
    let mut maybe_hostname = None;
    let mut maybe_aggregation_key = None;
    let mut maybe_source_type = None;
    let mut maybe_tags = None;

    let remaining = if !remaining.is_empty() {
        let (mut remaining, _) = tag("|")(remaining)?;
        while let Some((chunk, tail)) = split_at_delimiter(remaining, b'|') {
            if chunk.len() < 2 {
                break;
            }
            match &chunk[..2] {
                // Timestamp: client-provided timestamp for the event, relative to the Unix epoch, in seconds.
                message::TIMESTAMP_PREFIX => {
                    let (_, timestamp) =
                        all_consuming(preceded(tag(message::TIMESTAMP_PREFIX), unix_timestamp))(chunk)?;
                    maybe_timestamp = Some(timestamp);
                }
                // Hostname: client-provided hostname for the host that this event originated from.
                message::HOSTNAME_PREFIX => {
                    let (_, hostname) =
                        all_consuming(preceded(tag(message::HOSTNAME_PREFIX), ascii_alphanum_and_seps))(chunk)?;
                    maybe_hostname = Some(hostname.into());
                }
                // Aggregation key: key to be used to group this event with others that have the same key.
                message::AGGREGATION_KEY_PREFIX => {
                    let (_, aggregation_key) =
                        all_consuming(preceded(tag(message::AGGREGATION_KEY_PREFIX), ascii_alphanum_and_seps))(chunk)?;
                    maybe_aggregation_key = Some(aggregation_key.into());
                }
                // Priority: client-provided priority of the event.
                message::PRIORITY_PREFIX => {
                    let (_, priority) =
                        all_consuming(preceded(tag(message::PRIORITY_PREFIX), ascii_alphanum_and_seps))(chunk)?;
                    maybe_priority = Priority::try_from_string(priority);
                }
                // Source type name: client-provided source type name of the event.
                message::SOURCE_TYPE_PREFIX => {
                    let (_, source_type) =
                        all_consuming(preceded(tag(message::SOURCE_TYPE_PREFIX), ascii_alphanum_and_seps))(chunk)?;
                    maybe_source_type = Some(source_type.into());
                }
                // Alert type: client-provided alert type of the event.
                message::ALERT_TYPE_PREFIX => {
                    let (_, alert_type) =
                        all_consuming(preceded(tag(message::ALERT_TYPE_PREFIX), ascii_alphanum_and_seps))(chunk)?;
                    maybe_alert_type = AlertType::try_from_string(alert_type);
                }
                // Tags: additional tags to be added to the event.
                _ if chunk.starts_with(message::TAGS_PREFIX) => {
                    let (_, tags) = all_consuming(preceded(tag(message::TAGS_PREFIX), metric_tags(config)))(chunk)?;
                    maybe_tags = Some(tags.into_iter().map(Into::into).collect());
                }
                _ => {
                    // We don't know what this is, so we just skip it.
                    //
                    // TODO: Should we throw an error, warn, or be silently permissive?
                }
            }
            remaining = tail;
        }
        remaining
    } else {
        remaining
    };

    let eventd = EventD::new(&title, &text)
        .with_timestamp(maybe_timestamp)
        .with_hostname(maybe_hostname)
        .with_aggregation_key(maybe_aggregation_key)
        .with_priority(maybe_priority)
        .with_source_type_name(maybe_source_type)
        .with_alert_type(maybe_alert_type)
        .with_tags(maybe_tags);

    Ok((remaining, eventd))
}

fn parse_dogstatsd_service_check<'a>(
    input: &'a [u8], config: &DogstatsdCodecConfiguration,
) -> IResult<&'a [u8], ServiceCheck> {
    let (remaining, (name, raw_check_status)) = preceded(
        tag(message::SERVICE_CHECK_PREFIX),
        separated_pair(ascii_alphanum_and_seps, tag(b"|"), parse_u8),
    )(input)?;

    let check_status =
        CheckStatus::try_from(raw_check_status).map_err(|_| nom::Err::Error(Error::new(input, ErrorKind::Verify)))?;

    let mut maybe_timestamp = None;
    let mut maybe_hostname = None;
    let mut maybe_tags = None;
    let mut maybe_message = None;
    let mut seen_message = false;

    let remaining = if !remaining.is_empty() {
        let (mut remaining, _) = tag("|")(remaining)?;
        while let Some((chunk, tail)) = split_at_delimiter(remaining, b'|') {
            if chunk.len() < 2 {
                break;
            }

            // Message field must be positioned last among the metadata fields but it was already seen
            if seen_message {
                return Err(nom::Err::Error(Error::new(input, ErrorKind::Verify)));
            }
            match &chunk[..2] {
                // Timestamp: client-provided timestamp for the event, relative to the Unix epoch, in seconds.
                message::TIMESTAMP_PREFIX => {
                    let (_, timestamp) =
                        all_consuming(preceded(tag(message::TIMESTAMP_PREFIX), unix_timestamp))(chunk)?;
                    maybe_timestamp = Some(timestamp);
                }
                // Hostname: client-provided hostname for the host that this service check originated from.
                message::HOSTNAME_PREFIX => {
                    let (_, hostname) =
                        all_consuming(preceded(tag(message::HOSTNAME_PREFIX), ascii_alphanum_and_seps))(chunk)?;
                    maybe_hostname = Some(hostname.into());
                }
                // Tags: additional tags to be added to the service check.
                _ if chunk.starts_with(message::TAGS_PREFIX) => {
                    let (_, tags) = all_consuming(preceded(tag(message::TAGS_PREFIX), metric_tags(config)))(chunk)?;
                    maybe_tags = Some(tags.into_iter().map(Into::into).collect());
                }
                // Message: A message describing the current state of the service check.
                message::SERVICE_CHECK_MESSAGE_PREFIX => {
                    let (_, message) = all_consuming(preceded(
                        tag(message::SERVICE_CHECK_MESSAGE_PREFIX),
                        ascii_alphanum_and_seps,
                    ))(chunk)?;
                    maybe_message = Some(message.into());

                    // This field must be positioned last among the metadata fields
                    seen_message = true;
                }
                _ => {
                    // We don't know what this is, so we just skip it.
                    //
                    // TODO: Should we throw an error, warn, or be silently permissive?
                }
            }
            remaining = tail;
        }
        remaining
    } else {
        remaining
    };
    let service_check = saluki_event::service_check::ServiceCheck::new(name, check_status)
        .with_timestamp(maybe_timestamp)
        .with_hostname(maybe_hostname)
        .with_tags(maybe_tags)
        .with_message(maybe_message);
    Ok((remaining, service_check))
}

#[inline]
fn split_at_delimiter(input: &[u8], delimiter: u8) -> Option<(&[u8], &[u8])> {
    match memchr::memchr(delimiter, input) {
        Some(index) => Some((&input[0..index], &input[index + 1..input.len()])),
        None => {
            if input.is_empty() {
                None
            } else {
                Some((input, &[]))
            }
        }
    }
}

#[inline]
fn split_at_delimiter_inclusive(input: &[u8], delimiter: u8) -> Option<(&[u8], &[u8])> {
    match memchr::memchr(delimiter, input) {
        Some(index) => Some((&input[0..index], &input[index..input.len()])),
        None => {
            if input.is_empty() {
                None
            } else {
                Some((input, &[]))
            }
        }
    }
}

#[inline]
fn ascii_alphanum_and_seps(input: &[u8]) -> IResult<&[u8], &str> {
    let valid_char = |c: u8| c.is_ascii_alphanumeric() || c == b' ' || c == b'_' || c == b'-' || c == b'.';
    map(take_while1(valid_char), |b| {
        // SAFETY: We know the bytes in `b` can only be comprised of ASCII characters, which ensures that it's valid to
        // interpret the bytes directly as UTF-8.
        unsafe { std::str::from_utf8_unchecked(b) }
    })(input)
}

#[inline]
fn permissive_metric_name(input: &[u8]) -> IResult<&[u8], &str> {
    // Essentially, any ASCII character that is printable and isn't `:` is allowed here.
    let valid_char = |c: u8| c > 31 && c < 128 && c != b':';
    map(take_while1(valid_char), |b| {
        // SAFETY: We know the bytes in `b` can only be comprised of ASCII characters, which ensures that it's valid to
        // interpret the bytes directly as UTF-8.
        unsafe { std::str::from_utf8_unchecked(b) }
    })(input)
}

#[inline]
fn raw_metric_values(input: &[u8]) -> IResult<&[u8], (MetricType, &[u8])> {
    let (remaining, raw_values) = terminated(take_while1(|b| b != b'|'), tag("|"))(input)?;
    let (remaining, raw_kind) = alt((tag(b"g"), tag(b"c"), tag(b"ms"), tag(b"h"), tag(b"s"), tag(b"d")))(remaining)?;

    // Make sure the raw value(s) are valid UTF-8 before we use them later on.
    if raw_values.is_empty() || simdutf8::basic::from_utf8(raw_values).is_err() {
        return Err(nom::Err::Error(Error::new(raw_values, ErrorKind::Verify)));
    }

    let metric_type = match raw_kind {
        b"c" => MetricType::Count,
        b"g" => MetricType::Gauge,
        b"s" => MetricType::Set,
        b"ms" => MetricType::Timer,
        b"h" => MetricType::Histogram,
        b"d" => MetricType::Distribution,
        _ => unreachable!("should be constrained by alt parser"),
    };

    Ok((remaining, (metric_type, raw_values)))
}

#[inline]
fn metric_values_from_raw(
    input: &[u8], metric_type: MetricType, sample_rate: Option<SampleRate>,
) -> Result<(u64, MetricValues), NomParserError<'_>> {
    let mut num_points = 0;
    let floats = FloatIter::new(input).inspect(|_| num_points += 1);

    let values = match metric_type {
        MetricType::Count => MetricValues::counter_sampled_fallible(floats, sample_rate)?,
        MetricType::Gauge => MetricValues::gauge_fallible(floats)?,
        MetricType::Set => {
            num_points = 1;

            // SAFETY: We've already checked above that `input` is valid UTF-8.
            let value = unsafe { std::str::from_utf8_unchecked(input) };
            MetricValues::set(value.to_string())
        }
        MetricType::Timer | MetricType::Histogram => MetricValues::histogram_sampled_fallible(floats, sample_rate)?,
        MetricType::Distribution => MetricValues::distribution_sampled_fallible(floats, sample_rate)?,
    };

    Ok((num_points, values))
}

#[inline]
fn metric_tags(config: &DogstatsdCodecConfiguration) -> impl Fn(&[u8]) -> IResult<&[u8], TagSplitter<'_>> {
    let max_tag_count = config.maximum_tag_count;
    let max_tag_len = config.maximum_tag_length;

    move |input: &[u8]| {
        // Make sure the raw value(s) are valid UTF-8 before we use them later on.
        if simdutf8::basic::from_utf8(input).is_err() {
            return Err(nom::Err::Error(Error::new(input, ErrorKind::Verify)));
        }

        match split_at_delimiter_inclusive(input, b'|') {
            Some((tags, remaining)) => Ok((remaining, TagSplitter::new(tags, max_tag_count, max_tag_len))),
            None => Err(nom::Err::Error(Error::new(input, ErrorKind::TakeWhile1))),
        }
    }
}

#[inline]
fn unix_timestamp(input: &[u8]) -> IResult<&[u8], u64> {
    parse_u64(input)
}

#[inline]
fn container_id(input: &[u8]) -> IResult<&[u8], &str> {
    // We generally only expect container IDs to be either long hexadecimal strings (like 64 characters), or in special
    // cases, the inode number of the cgroup controller that contains the container sending the metrics, where the value
    // will look like `in-<integer value>`.
    let valid_char = |c: u8| c.is_ascii_alphanumeric() || c == b'-';
    map(take_while1(valid_char), |b| {
        // SAFETY: We know the bytes in `b` can only be comprised of ASCII characters, which ensures that it's valid to
        // interpret the bytes directly as UTF-8.
        unsafe { std::str::from_utf8_unchecked(b) }
    })(input)
}

#[inline]
fn limit_str_to_len(s: &str, limit: usize) -> &str {
    if limit >= s.len() {
        s
    } else {
        let sb = s.as_bytes();

        // Search through the last four bytes of the string, ending at the index `limit`, and look for the byte that
        // defines the boundary of a full UTF-8 character.
        let start = limit.saturating_sub(3);
        let new_index = sb[start..=limit]
            .iter()
            // Bit twiddling magic for checking if `b` is < 128 or >= 192.
            .rposition(|b| (*b as i8) >= -0x40);

        // SAFETY: UTF-8 characters are a maximum of four bytes, so we know we will have found a valid character
        // boundary by searching over four bytes, regardless of where the slice started.
        //
        // Similarly we know that taking everything from index 0 to the detected character boundary index will be a
        // valid UTF-8 string.
        unsafe {
            let safe_end = start + new_index.unwrap_unchecked();
            std::str::from_utf8_unchecked(&sb[..safe_end])
        }
    }
}

/// An iterator for splitting tags out of an input byte slice.
///
/// Extracts individual tags from the input byte slice by splitting on the comma (`,`, 0x2C) character.
///
/// ## Cloning
///
/// `TagSplitter` can be cloned to create a new iterator with its own iteration state. The same underlying input byte
/// slice is retained.
#[derive(Clone)]
pub struct TagSplitter<'a> {
    raw_tags: &'a [u8],
    max_tag_count: usize,
    max_tag_len: usize,
}

impl<'a> TagSplitter<'a> {
    /// Creates a new `TagSplitter` from the given input byte slice.
    ///
    /// The maximum tag count and maximum tag length control how many tags are returned from the iterator and their
    /// length. If the iterator encounters more tags than the maximum count, it will simply stop returning tags. If the
    /// iterator encounters any tag that is longer than the maximum length, it will truncate the tag to configured
    /// length, or to a smaller length, whichever is closer to a valid UTF-8 character boundary.
    const fn new(raw_tags: &'a [u8], max_tag_count: usize, max_tag_len: usize) -> Self {
        Self {
            raw_tags,
            max_tag_count,
            max_tag_len,
        }
    }

    /// Creates an empty `TagSplitter`.
    const fn empty() -> Self {
        Self {
            raw_tags: &[],
            max_tag_count: 0,
            max_tag_len: 0,
        }
    }
}

impl<'a> IntoIterator for &TagSplitter<'a> {
    type Item = &'a str;
    type IntoIter = TagIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        TagIter {
            raw_tags: self.raw_tags,
            parsed_tags: 0,
            max_tag_len: self.max_tag_len,
            max_tag_count: self.max_tag_count,
        }
    }
}

pub struct TagIter<'a> {
    raw_tags: &'a [u8],
    parsed_tags: usize,
    max_tag_len: usize,
    max_tag_count: usize,
}

impl<'a> Iterator for TagIter<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        let (raw_tag, tail) = split_at_delimiter(self.raw_tags, b',')?;
        self.raw_tags = tail;

        if self.parsed_tags >= self.max_tag_count {
            // We've reached the maximum number of tags, so we just skip the rest.
            return None;
        }

        // SAFETY: The caller that creates `TagSplitter` is responsible for ensuring that the entire byte slice is
        // valid UTF-8, which means we should also have valid UTF-8 here since only `TagSplitter` creates `TagIter`.
        let tag = unsafe { std::str::from_utf8_unchecked(raw_tag) };
        let tag = limit_str_to_len(tag, self.max_tag_len);

        self.parsed_tags += 1;

        Some(tag)
    }
}

struct FloatIter<'a> {
    raw_values: &'a [u8],
}

impl<'a> FloatIter<'a> {
    fn new(raw_values: &'a [u8]) -> Self {
        Self { raw_values }
    }
}

impl<'a> Iterator for FloatIter<'a> {
    type Item = Result<f64, nom::Err<nom::error::Error<&'a [u8]>>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.raw_values.is_empty() {
            return None;
        }

        let (raw_value, tail) = split_at_delimiter(self.raw_values, b':')?;
        self.raw_values = tail;

        // SAFETY: The caller that creates `ValueIter` is responsible for ensuring that the entire byte slice is valid
        // UTF-8.
        let value_s = unsafe { std::str::from_utf8_unchecked(raw_value) };
        match value_s.parse::<f64>() {
            Ok(value) => Some(Ok(value)),
            Err(_) => Some(Err(nom::Err::Error(Error::new(raw_value, ErrorKind::Float)))),
        }
    }
}

#[cfg(test)]
mod tests {
    use nom::IResult;
    use proptest::{collection::vec as arb_vec, prelude::*};
    use saluki_context::{ContextResolver, ContextResolverBuilder};
    use saluki_event::{
        eventd::{AlertType, EventD, Priority},
        metric::*,
        service_check::{CheckStatus, ServiceCheck},
    };
    use stringtheory::MetaString;

    use super::{
        parse_dogstatsd_event, parse_dogstatsd_metric, parse_dogstatsd_service_check, DogstatsdCodecConfiguration,
    };

    type NomResult<'input, T> = Result<T, nom::Err<nom::error::Error<&'input [u8]>>>;
    type OptionalNomResult<'input, T> = Result<Option<T>, nom::Err<nom::error::Error<&'input [u8]>>>;

    fn parse_dsd_metric(input: &[u8]) -> OptionalNomResult<'_, Metric> {
        let default_config = DogstatsdCodecConfiguration::default();
        parse_dsd_metric_with_conf(input, &default_config)
    }

    fn parse_dsd_metric_with_conf<'input>(
        input: &'input [u8], config: &DogstatsdCodecConfiguration,
    ) -> OptionalNomResult<'input, Metric> {
        let mut context_resolver = ContextResolverBuilder::for_tests();
        let (remaining, result) = parse_dsd_metric_direct(input, config, &mut context_resolver)?;
        assert!(remaining.is_empty());

        Ok(result)
    }

    fn parse_dsd_metric_direct<'input>(
        input: &'input [u8], config: &DogstatsdCodecConfiguration, context_resolver: &mut ContextResolver,
    ) -> IResult<&'input [u8], Option<Metric>> {
        let (remaining, packet) = parse_dogstatsd_metric(input, config)?;
        // TODO: this is duplicative with `handle_metric_packet`, but it's not clear where we want the responsibility for metadata to live
        let metadata = MetricMetadata {
            hostname: None,
            origin_entity: OriginEntity {
                process_id: None,
                container_id: MetaString::from(packet.container_id.unwrap_or("")),
                pod_uid: MetaString::from(packet.pod_uid.unwrap_or("")),
                cardinality: packet.cardinality,
            },
            origin: Some(
                packet
                    .jmx_check_name
                    .map(MetricOrigin::jmx_check)
                    .unwrap_or_else(MetricOrigin::dogstatsd),
            ),
        };

        let context_ref = context_resolver.create_context_ref(packet.metric_name, &packet.tags);
        let context = match context_resolver.resolve(context_ref) {
            Some(context) => context,
            None => return Ok((remaining, None)),
        };

        Ok((remaining, Some(Metric::from_parts(context, packet.values, metadata))))
    }

    fn parse_dsd_eventd(input: &[u8]) -> NomResult<'_, EventD> {
        let default_config = DogstatsdCodecConfiguration::default();
        parse_dsd_eventd_with_conf(input, &default_config)
    }

    fn parse_dsd_eventd_with_conf<'input>(
        input: &'input [u8], config: &DogstatsdCodecConfiguration,
    ) -> NomResult<'input, EventD> {
        let (remaining, eventd) = parse_dsd_eventd_direct(input, config)?;
        assert!(remaining.is_empty());

        Ok(eventd)
    }

    fn parse_dsd_eventd_direct<'input>(
        input: &'input [u8], config: &DogstatsdCodecConfiguration,
    ) -> IResult<&'input [u8], EventD> {
        parse_dogstatsd_event(input, config)
    }

    fn parse_dsd_service_check(input: &[u8]) -> NomResult<'_, ServiceCheck> {
        let default_config = DogstatsdCodecConfiguration::default();
        parse_dsd_service_check_with_conf(input, &default_config)
    }

    fn parse_dsd_service_check_with_conf<'input>(
        input: &'input [u8], config: &DogstatsdCodecConfiguration,
    ) -> NomResult<'input, ServiceCheck> {
        let (remaining, service_check) = parse_dsd_service_check_direct(input, config)?;
        assert!(remaining.is_empty());

        Ok(service_check)
    }

    fn parse_dsd_service_check_direct<'input>(
        input: &'input [u8], config: &DogstatsdCodecConfiguration,
    ) -> IResult<&'input [u8], ServiceCheck> {
        parse_dogstatsd_service_check(input, config)
    }

    #[track_caller]
    fn check_basic_metric_eq(mut expected: Metric, actual: Option<Metric>) -> Metric {
        let actual = actual.expect("event should not have been None");

        // We set this manually because the DSD codec is always going to set this on the actual metric, so we want our
        // expected metric to also match... without each unit test having to set it as boilerplate.
        expected.metadata_mut().set_origin(MetricOrigin::dogstatsd());

        assert_eq!(expected.context(), actual.context());
        assert_eq!(expected.values(), actual.values());
        assert_eq!(expected.metadata(), actual.metadata());
        actual
    }

    #[track_caller]
    fn check_basic_eventd_eq(expected: EventD, actual: EventD) {
        assert_eq!(expected.title(), actual.title());
        assert_eq!(expected.text(), actual.text());
        assert_eq!(expected.timestamp(), actual.timestamp());
        assert_eq!(expected.hostname(), actual.hostname());
        assert_eq!(expected.aggregation_key(), actual.aggregation_key());
        assert_eq!(expected.priority(), actual.priority());
        assert_eq!(expected.source_type_name(), actual.source_type_name());
        assert_eq!(expected.alert_type(), actual.alert_type());
        assert_eq!(expected.tags(), actual.tags());
    }

    #[track_caller]
    fn check_basic_service_check_eq(expected: ServiceCheck, actual: ServiceCheck) {
        assert_eq!(expected.name(), actual.name());
        assert_eq!(expected.status(), actual.status());
        assert_eq!(expected.timestamp(), actual.timestamp());
        assert_eq!(expected.hostname(), actual.hostname());
        assert_eq!(expected.tags(), actual.tags());
    }

    #[test]
    fn basic_metric() {
        let name = "my.counter";
        let value = 1.0;
        let raw = format!("{}:{}|c", name, value);
        let expected = Metric::counter(name, value);
        let actual = parse_dsd_metric(raw.as_bytes()).expect("should not fail to parse");
        check_basic_metric_eq(expected, actual);

        let name = "my.gauge";
        let value = 2.0;
        let raw = format!("{}:{}|g", name, value);
        let expected = Metric::gauge(name, value);
        let actual = parse_dsd_metric(raw.as_bytes()).expect("should not fail to parse");
        check_basic_metric_eq(expected, actual);

        // Special case where we check this for both timers and histograms since we treat them both the same when
        // parsing.
        let name = "my.timer_or_histogram";
        let value = 3.0;
        for kind in &["ms", "h"] {
            let raw = format!("{}:{}|{}", name, value, kind);
            let expected = Metric::histogram(name, value);
            let actual = parse_dsd_metric(raw.as_bytes()).expect("should not fail to parse");
            check_basic_metric_eq(expected, actual);
        }

        let distribution_name = "my.distribution";
        let distribution_value = 3.0;
        let distribution_raw = format!("{}:{}|d", distribution_name, distribution_value);
        let distribution_expected = Metric::distribution(distribution_name, distribution_value);
        let distribution_actual = parse_dsd_metric(distribution_raw.as_bytes()).expect("should not fail to parse");
        check_basic_metric_eq(distribution_expected, distribution_actual);

        let set_name = "my.set";
        let set_value = "value";
        let set_raw = format!("{}:{}|s", set_name, set_value);
        let set_expected = Metric::set(set_name, set_value);
        let set_actual = parse_dsd_metric(set_raw.as_bytes()).expect("should not fail to parse");
        check_basic_metric_eq(set_expected, set_actual);
    }

    #[test]
    fn metric_tags() {
        let name = "my.counter";
        let value = 1.0;
        let tags = ["tag1", "tag2"];
        let raw = format!("{}:{}|c|#{}", name, value, tags.join(","));
        let expected = Metric::counter((name, &tags[..]), value);

        let actual = parse_dsd_metric(raw.as_bytes()).expect("should not fail to parse");
        check_basic_metric_eq(expected, actual);
    }

    #[test]
    fn metric_sample_rate() {
        let name = "my.counter";
        let value = 1.0;
        let sample_rate = 0.5;
        let raw = format!("{}:{}|c|@{}", name, value, sample_rate);

        let value_sample_rate_adjusted = value * (1.0 / sample_rate);
        let expected = Metric::counter(name, value_sample_rate_adjusted);

        let actual = parse_dsd_metric(raw.as_bytes()).expect("should not fail to parse");
        let actual = check_basic_metric_eq(expected, actual);
        let values = match actual.values() {
            MetricValues::Counter(values) => values
                .into_iter()
                .map(|(ts, v)| (ts.map(|v| v.get()).unwrap_or(0), v))
                .collect::<Vec<_>>(),
            _ => panic!("expected counter values"),
        };

        assert_eq!(values.len(), 1);
        assert_eq!(values[0], (0, value_sample_rate_adjusted));
    }

    #[test]
    fn metric_container_id() {
        let name = "my.counter";
        let value = 1.0;
        let container_id = "abcdef123456";
        let raw = format!("{}:{}|c|c:{}", name, value, container_id);
        let mut expected = Metric::counter(name, value);
        let origin_entity = expected.metadata_mut().origin_entity_mut();
        origin_entity.container_id = MetaString::from(container_id);

        let actual = parse_dsd_metric(raw.as_bytes()).expect("should not fail to parse");
        check_basic_metric_eq(expected, actual);
    }

    #[test]
    fn metric_unix_timestamp() {
        let name = "my.counter";
        let value = 1.0;
        let timestamp = 1234567890;
        let raw = format!("{}:{}|c|T{}", name, value, timestamp);
        let mut expected = Metric::counter(name, value);
        expected.values_mut().set_timestamp(timestamp);

        let actual = parse_dsd_metric(raw.as_bytes()).expect("should not fail to parse");
        check_basic_metric_eq(expected, actual);
    }

    #[test]
    fn metric_multiple_extensions() {
        let name = "my.counter";
        let value = 1.0;
        let sample_rate = 0.5;
        let tags = ["tag1", "tag2"];
        let container_id = "abcdef123456";
        let timestamp = 1234567890;
        let raw = format!(
            "{}:{}|c|#{}|@{}|c:{}|T{}",
            name,
            value,
            tags.join(","),
            sample_rate,
            container_id,
            timestamp
        );

        let value_sample_rate_adjusted = value * (1.0 / sample_rate);
        let mut expected = Metric::counter((name, &tags[..]), value_sample_rate_adjusted);
        let origin_entity = expected.metadata_mut().origin_entity_mut();
        origin_entity.container_id = MetaString::from(container_id);
        expected.values_mut().set_timestamp(timestamp);

        let actual = parse_dsd_metric(raw.as_bytes()).expect("should not fail to parse");
        let actual = check_basic_metric_eq(expected, actual);
        let values = match actual.values() {
            MetricValues::Counter(values) => values
                .into_iter()
                .map(|(ts, v)| (ts.map(|v| v.get()).unwrap_or(0), v))
                .collect::<Vec<_>>(),
            _ => panic!("expected counter values"),
        };

        assert_eq!(values.len(), 1);
        assert_eq!(values[0], (timestamp, value_sample_rate_adjusted));
    }

    #[test]
    fn multivalue_metrics() {
        let name = "my.counter";
        let values = [1.0, 2.0, 3.0];
        let values_stringified = values.iter().map(|v| v.to_string()).collect::<Vec<_>>();
        let raw = format!("{}:{}|c", name, values_stringified.join(":"));
        let expected = Metric::counter(name, values);
        let actual = parse_dsd_metric(raw.as_bytes()).expect("should not fail to parse");
        check_basic_metric_eq(expected, actual);

        let name = "my.gauge";
        let values = [42.0, 5.0, -18.0];
        let values_stringified = values.iter().map(|v| v.to_string()).collect::<Vec<_>>();
        let raw = format!("{}:{}|g", name, values_stringified.join(":"));
        let expected = Metric::gauge(name, values);
        let actual = parse_dsd_metric(raw.as_bytes()).expect("should not fail to parse");
        check_basic_metric_eq(expected, actual);

        // Special case where we check this for both timers and histograms since we treat them both the same when
        // parsing.
        //
        // Additionally, we have an optimization to return a single distribution metric from multi-value payloads, so we
        // also check here that only one metric is generated for multi-value timers/histograms/distributions.
        let name = "my.timer_or_histogram";
        let values = [27.5, 4.20, 80.085];
        let values_stringified = values.iter().map(|v| v.to_string()).collect::<Vec<_>>();
        for kind in &["ms", "h"] {
            let raw = format!("{}:{}|{}", name, values_stringified.join(":"), kind);
            let expected = Metric::histogram(name, values);
            let actual = parse_dsd_metric(raw.as_bytes()).expect("should not fail to parse");
            check_basic_metric_eq(expected, actual);
        }

        let name = "my.distribution";
        let raw = format!("{}:{}|d", name, values_stringified.join(":"));
        let expected = Metric::distribution(name, values);
        let actual = parse_dsd_metric(raw.as_bytes()).expect("should not fail to parse");
        check_basic_metric_eq(expected, actual);
    }

    #[test]
    fn respects_maximum_tag_count() {
        let input = b"foo:1|c|#tag1:value1,tag2:value2,tag3:value3";

        let cases = [3, 2, 1];
        for max_tag_count in cases {
            let config = DogstatsdCodecConfiguration::default().with_maximum_tag_count(max_tag_count);

            let metric = parse_dsd_metric_with_conf(input, &config)
                .expect("should not fail to parse")
                .expect("should not fail to intern");
            assert_eq!(metric.context().tags().len(), max_tag_count);
        }
    }

    #[test]
    fn respects_maximum_tag_length() {
        let input = b"foo:1|c|#tag1:short,tag2:medium,tag3:longlong";

        let cases = [6, 5, 4];
        for max_tag_length in cases {
            let config = DogstatsdCodecConfiguration::default().with_maximum_tag_length(max_tag_length);

            let metric = parse_dsd_metric_with_conf(input, &config)
                .expect("should not fail to parse")
                .expect("should not fail to intern");
            for tag in metric.context().tags().into_iter() {
                assert!(tag.len() <= max_tag_length);
            }
        }
    }

    #[test]
    fn respects_read_timestamps() {
        let input = b"foo:1|c|T1234567890";

        let config = DogstatsdCodecConfiguration::default().with_timestamps(false);

        let metric = parse_dsd_metric_with_conf(input, &config)
            .expect("should not fail to parse")
            .expect("should not fail to intern");

        let value_timestamps = match metric.values() {
            MetricValues::Counter(values) => values
                .into_iter()
                .map(|(ts, _)| ts.map(|v| v.get()).unwrap_or(0))
                .collect::<Vec<_>>(),
            _ => panic!("expected counter values"),
        };

        assert_eq!(value_timestamps.len(), 1);
        assert_eq!(value_timestamps[0], 0);
    }

    #[test]
    fn no_metrics_when_interner_full_allocations_disallowed() {
        // We're specifically testing here that when we don't allow outside allocations, we should not be able to
        // resolve a context if the interner is full. A no-op interner has the smallest possible size, so that's going
        // to assure we can't intern anything... but we also need a string (name or one of the tags) that can't be
        // _inlined_ either, since that will get around the interner being full.
        //
        // We set our metric name to be longer than 23 bytes (the inlining limit) to ensure this.

        let config = DogstatsdCodecConfiguration::default();
        let mut context_resolver = ContextResolverBuilder::for_tests().with_heap_allocations(false);

        let metric_name = "big_metric_name_that_cant_possibly_be_inlined";
        assert!(MetaString::try_inline(metric_name).is_none());

        let input = format!("{}:1|c|#tag1:value1,tag2:value2,tag3:value3", metric_name);

        let (remaining, result) = parse_dsd_metric_direct(input.as_bytes(), &config, &mut context_resolver)
            .expect("should not fail to parse");
        assert!(remaining.is_empty());
        assert!(result.is_none());
    }

    #[test]
    fn basic_eventd() {
        let event_title = "my event";
        let event_text = "text";
        let raw = format!(
            "_e{{{},{}}}:{}|{}",
            event_title.len(),
            event_text.len(),
            event_title,
            event_text
        );

        let actual = parse_dsd_eventd(raw.as_bytes()).unwrap();
        let expected = EventD::new(event_title, event_text);
        check_basic_eventd_eq(expected, actual);
    }

    #[test]
    fn eventd_tags() {
        let event_title = "my event";
        let event_text = "text";
        let tags = vec!["tag1".into(), "tag2".into()];
        let raw = format!(
            "_e{{{},{}}}:{}|{}|#{}",
            event_title.len(),
            event_text.len(),
            event_title,
            event_text,
            tags.join(","),
        );

        let expected = EventD::new(event_title, event_text).with_tags(tags);
        let actual = parse_dsd_eventd(raw.as_bytes()).unwrap();
        check_basic_eventd_eq(expected, actual);
    }

    #[test]
    fn eventd_priority() {
        let event_title = "my event";
        let event_text = "text";
        let event_priority = Priority::Low;
        let raw = format!(
            "_e{{{},{}}}:{}|{}|p:{}",
            event_title.len(),
            event_text.len(),
            event_title,
            event_text,
            event_priority
        );

        let expected = EventD::new(event_title, event_text).with_priority(event_priority);
        let actual = parse_dsd_eventd(raw.as_bytes()).unwrap();
        check_basic_eventd_eq(expected, actual);
    }

    #[test]
    fn eventd_alert_type() {
        let event_title = "my event";
        let event_text = "text";
        let event_alert_type = AlertType::Warning;
        let raw = format!(
            "_e{{{},{}}}:{}|{}|t:{}",
            event_title.len(),
            event_text.len(),
            event_title,
            event_text,
            event_alert_type
        );

        let expected = EventD::new(event_title, event_text).with_alert_type(event_alert_type);
        let actual = parse_dsd_eventd(raw.as_bytes()).unwrap();
        check_basic_eventd_eq(expected, actual);
    }

    #[test]
    fn eventd_multiple_extensions() {
        let event_title = "my event";
        let event_text = "text";
        let event_hostname = MetaString::from("testhost");
        let event_aggregation_key = MetaString::from("testkey");
        let event_priority = Priority::Low;
        let event_source_type = MetaString::from("testsource");
        let event_alert_type = AlertType::Success;
        let event_timestamp = 1234567890;
        let tags = vec!["tag1".into(), "tag2".into()];
        let raw = format!(
            "_e{{{},{}}}:{}|{}|h:{}|k:{}|p:{}|s:{}|t:{}|d:{}|#{}",
            event_title.len(),
            event_text.len(),
            event_title,
            event_text,
            event_hostname,
            event_aggregation_key,
            event_priority,
            event_source_type,
            event_alert_type,
            event_timestamp,
            tags.join(","),
        );
        let actual = parse_dsd_eventd(raw.as_bytes()).unwrap();
        let expected = EventD::new(event_title, event_text)
            .with_hostname(event_hostname)
            .with_aggregation_key(event_aggregation_key)
            .with_priority(event_priority)
            .with_source_type_name(event_source_type)
            .with_alert_type(event_alert_type)
            .with_timestamp(event_timestamp)
            .with_tags(tags);
        check_basic_eventd_eq(expected, actual);
    }

    #[test]
    fn basic_service_checks() {
        let name = "testsvc";
        let sc_status = CheckStatus::Warning;
        let raw = format!("_sc|{}|{}", name, sc_status.as_u8());
        let actual = parse_dsd_service_check(raw.as_bytes()).unwrap();
        let expected = ServiceCheck::new(name, sc_status);
        check_basic_service_check_eq(expected, actual);
    }

    #[test]
    fn service_check_timestamp() {
        let name = "testsvc";
        let sc_status = CheckStatus::Warning;
        let sc_timestamp = 1234567890;
        let raw = format!("_sc|{}|{}|d:{}", name, sc_status.as_u8(), sc_timestamp);
        let actual = parse_dsd_service_check(raw.as_bytes()).unwrap();
        let expected = ServiceCheck::new(name, sc_status).with_timestamp(sc_timestamp);
        check_basic_service_check_eq(expected, actual);
    }

    #[test]
    fn service_check_tags() {
        let name = "testsvc";
        let sc_status = CheckStatus::Warning;
        let tags = vec!["tag1".into(), "tag2".into()];
        let raw = format!("_sc|{}|{}|#{}", name, sc_status.as_u8(), tags.join(","));
        let actual = parse_dsd_service_check(raw.as_bytes()).unwrap();
        let expected = ServiceCheck::new(name, sc_status).with_tags(tags);
        check_basic_service_check_eq(expected, actual);
    }

    #[test]
    fn service_check_message() {
        let name = "testsvc";
        let sc_status = CheckStatus::Ok;
        let sc_message = MetaString::from("service running properly");
        let raw = format!("_sc|{}|{}|m:{}", name, sc_status.as_u8(), sc_message);
        let actual = parse_dsd_service_check(raw.as_bytes()).unwrap();
        let expected = ServiceCheck::new(name, sc_status).with_message(sc_message);
        check_basic_service_check_eq(expected, actual);
    }

    #[test]
    fn service_check_multiple_extensions() {
        let name = "testsvc";
        let sc_status = CheckStatus::Unknown;
        let sc_timestamp = 1234567890;
        let sc_hostname = MetaString::from("myhost");
        let tags = vec!["tag1".into(), "tag2".into()];
        let sc_message = MetaString::from("service status unknown");
        let raw = format!(
            "_sc|{}|{}|d:{}|h:{}|#{}|m:{}",
            name,
            sc_status.as_u8(),
            sc_timestamp,
            sc_hostname,
            tags.join(","),
            sc_message
        );
        let actual = parse_dsd_service_check(raw.as_bytes()).unwrap();
        let expected = ServiceCheck::new(name, sc_status)
            .with_timestamp(sc_timestamp)
            .with_hostname(sc_hostname)
            .with_tags(tags)
            .with_message(sc_message);
        check_basic_service_check_eq(expected, actual);
    }

    #[test]
    fn permissive_mode() {
        let payload = b"codeheap 'non-nmethods'.usage:0.3054|g|#env:dev,service:foobar,datacenter:localhost.dev";

        let config = DogstatsdCodecConfiguration::default().with_permissive_mode(true);
        match parse_dsd_metric_with_conf(payload, &config) {
            Ok(result) => assert!(result.is_some(), "should not fail to materialize metric after decoding"),
            Err(e) => panic!("should not have errored: {:?}", e),
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(1000))]
        #[test]
        fn property_test_malicious_input_non_exhaustive(input in arb_vec(0..255u8, 0..1000)) {
            // We're testing that the parser is resilient to malicious input, which means that it should not panic or
            // crash when given input that's not well-formed.
            //
            // As this is a property test, it is _not_ exhaustive but generally should catch simple issues that manage
            // to escape the unit tests. This is left here for the sole reason of incrementally running this every time
            // all tests are run, in the hopes of potentially catching an issue that might have been missed.
            //
            // TODO: True exhaustive-style testing a la afl/honggfuzz.
            let _ = parse_dsd_metric(&input);
        }
    }
}
