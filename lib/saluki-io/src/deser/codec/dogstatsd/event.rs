use nom::{
    error::{Error, ErrorKind},
    IResult,
};
use saluki_event::eventd::EventD;

// const EVENT_TIMESTAMP_PREFIX: &[u8] = b"d:";
// const EVENT_HOSTNAME_PREFIX: &[u8] = b"h:";
// const EVENT_AGGREGATION_KEY_PREFIX: &[u8] = b"k:";
// const EVENT_PRIORITY_PREFIX: &[u8] = b"p:";
// const EVENT_SOURCE_TYPE_PREFIX: &[u8] = b"s:";
// const EVENT_ALERT_TYPE_PREFIX: &[u8] = b"t:";
// const EVENT_TAGS_PREFIX: &[u8] = b"#";

// const EVENT_PRIORITY_LOW: &[u8] = b"low";
// const EVENT_PRIORITY_NORMAL: &[u8] = b"normal";

// const EVENT_ALERT_TYPE_ERROR: &[u8] = b"error";
// const EVENT_ALERT_TYPE_WARNING: &[u8] = b"warning";
// const EVENT_ALERT_TYPE_INFO: &[u8] = b"info";
// const EVENT_ALERT_TYPE_SUCCESS: &[u8] = b"success";

/// The header for an event.
pub struct Header {
    title_len: usize,

    text_len: usize,
}

impl Header {
    /// Creates an `Eventd` from the given parts.
    pub fn from_parts(title_len: usize, text_len: usize) -> Self {
        Self { title_len, text_len }
    }
}

pub fn parse_dogstatsd_event_header<'a>(
    raw_event_header: &'a str, raw_event_data: &'a str,
) -> IResult<&'a [u8], Header> {
    if raw_event_header.len() < 7 {
        return Err(nom::Err::Error(Error::new(
            raw_event_header.as_bytes(),
            ErrorKind::Verify,
        )));
    }

    // Extract the raw lengths substring containing the lengths of the title and text
    let raw_lengths = &raw_event_header[3..raw_event_header.len() - 1];
    let sep_index = raw_lengths
        .find(',')
        .ok_or_else(|| nom::Err::Error(Error::new(raw_event_header.as_bytes(), ErrorKind::Verify)))?;

    // Split at the comma and trim the remaining part
    let (title_len_str, text_len_str) = raw_lengths.split_at(sep_index);
    let text_len_str = &text_len_str[1..]; // Remove the comma

    // Convert from str to usize to perform basic validation
    let title_length = title_len_str
        .parse::<usize>()
        .map_err(|_| nom::Err::Error(Error::new(raw_event_header.as_bytes(), ErrorKind::Verify)))?;
    let text_length = text_len_str
        .parse::<usize>()
        .map_err(|_| nom::Err::Error(Error::new(raw_event_header.as_bytes(), ErrorKind::Verify)))?;

    // Title and Text are required
    if title_length == 0 || text_length == 0 {
        return Err(nom::Err::Error(Error::new(
            raw_event_header.as_bytes(),
            ErrorKind::Verify,
        )));
    }

    let header = Header::from_parts(title_length, text_length);

    if raw_event_data.len() < header.title_len + header.text_len + 1 {
        return Err(nom::Err::Error(Error::new(
            raw_event_header.as_bytes(),
            ErrorKind::Verify,
        )));
    }

    Ok((&[], header))
}

pub fn parse_dogstatsd_event_data(header: &Header, raw_event_data: &str) -> (String, String) {
    let title = clean_data(&raw_event_data[..header.title_len]);
    let text = clean_data(&raw_event_data[header.title_len + 1..header.title_len + 1 + header.text_len]);
    (title, text)
}

fn clean_data(s: &str) -> String {
    s.replace("\\n", "\n")
}

fn next_field(message: &str) -> Option<(&str, &str)> {
    message.split_once('|')
}

#[allow(unused)]
fn apply_optional_field<'a>(event: &'a EventD, field: &'a str) -> IResult<&'a [u8], ()> {
    Ok((b"", ()))
}

/// Parse any optional fields from the remaining raw event string.
#[allow(dead_code)]
pub fn parse_optional_fields<'a>(event: &'a EventD, optional_fields: Option<&'a str>) -> IResult<&'a [u8], ()> {
    let mut fields = optional_fields;
    while let Some(field) = fields {
        if let Some((optional_field, fields_rest)) = next_field(field) {
            fields = Some(fields_rest);
            apply_optional_field(event, optional_field)
                .map_err(|_| nom::Err::Error(Error::new(field.as_bytes(), ErrorKind::Verify)))?;
        } else {
            break;
        }
    }
    Ok((b"", ()))
}
