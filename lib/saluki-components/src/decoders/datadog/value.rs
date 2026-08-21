//! Value-level decoding for the v1.0 APM trace wire format: streaming strings, attribute maps, and
//! the recursive `AnyValue` union.

use saluki_common::collections::FastHashMap;
use saluki_core::data_model::event::trace::AttributeValue;
use stringtheory::MetaString;

use super::error::DecodeError;
use super::read;
use super::string_table::StringTable;

/// Maximum `AnyValue` nesting depth.
///
/// Without a bound, a deeply nested payload could drive the decoder into unbounded recursion and
/// overflow the stack. Matches the reference decoder's limit.
const MAX_ANY_VALUE_DEPTH: usize = 200;

/// Reads a streaming string and resolves it to an owned [`MetaString`].
///
/// A streaming string is encoded either as a literal string (appended to `strings`, its new index
/// returned implicitly) or as a `uint32` index into `strings`.
pub fn read_streaming_string(
    r: &mut &[u8], strings: &mut StringTable, context: &'static str,
) -> Result<MetaString, DecodeError> {
    if read::peek_is_str(r) {
        let s = read::read_str(r, context)?;
        let index = strings.add(s);
        strings.get(index)
    } else {
        let index = read::read_u32(r, context)?;
        strings.get(index)
    }
}

/// Skips the value of an unrecognized field, harvesting any inline strings it carries into
/// `strings`.
///
/// # Design
///
/// String references in this format are purely positional: the first occurrence of a string is
/// written inline and appended to the payload's string table, and later occurrences are written as a
/// `uint32` index into that table (see [`read_streaming_string`]). A newer producer may add a field
/// this decoder does not recognize. If that field carries a new inline string, the string still
/// occupied a table slot on the encoding side, so the decoder **MUST** add it too. Otherwise every
/// subsequent index in the known fields that follow is shifted by one, silently resolving to the
/// wrong string or failing as out of range. Skipping the value's bytes with
/// [`read::skip_value`](super::read::skip_value) alone is not enough.
///
/// Every inline string in the payload outside the string table array itself (a known field) is a
/// streaming string, so each string encountered here is added, all other scalars are skipped, and
/// arrays and maps are walked recursively. Map keys are walked as well as values, since either may
/// be or contain a streaming string.
///
/// # Errors
///
/// Returns a [`DecodeError`] on truncated or malformed input, an oversized array or map header, or
/// nesting deeper than [`MAX_ANY_VALUE_DEPTH`].
pub fn harvest_unknown_field(
    r: &mut &[u8], strings: &mut StringTable, context: &'static str,
) -> Result<(), DecodeError> {
    harvest_unknown_field_depth(r, strings, context, 0)
}

fn harvest_unknown_field_depth(
    r: &mut &[u8], strings: &mut StringTable, context: &'static str, depth: usize,
) -> Result<(), DecodeError> {
    if depth > MAX_ANY_VALUE_DEPTH {
        return Err(DecodeError::DepthExceeded {
            max: MAX_ANY_VALUE_DEPTH,
        });
    }

    if read::peek_is_str(r) {
        let s = read::read_str(r, context)?;
        strings.add(s);
    } else if read::peek_is_array(r) {
        let len = read::read_array_len(r, context)?;
        for _ in 0..len {
            harvest_unknown_field_depth(r, strings, context, depth + 1)?;
        }
    } else if read::peek_is_map(r) {
        let len = read::read_map_len(r, context)?;
        for _ in 0..len {
            harvest_unknown_field_depth(r, strings, context, depth + 1)?;
            harvest_unknown_field_depth(r, strings, context, depth + 1)?;
        }
    } else {
        read::skip_value(r, context)?;
    }

    Ok(())
}

/// Reads an attribute map, encoded as a flat array of `[key, type, value, ...]` triples.
///
/// The array length must be a multiple of three. Duplicate keys resolve to last-wins, matching the
/// reference decoder's map semantics.
pub fn read_attributes_map(
    r: &mut &[u8], strings: &mut StringTable, context: &'static str,
) -> Result<FastHashMap<MetaString, AttributeValue>, DecodeError> {
    let len = read::read_array_len(r, context)?;
    if len % 3 != 0 {
        return Err(DecodeError::InvalidAttributeArrayLen { len });
    }

    let mut map = FastHashMap::default();
    map.reserve((len / 3) as usize);
    let mut i = 0;
    while i < len {
        let key = read_streaming_string(r, strings, context)?;
        let value = read_any_value(r, strings)?;
        map.insert(key, value);
        i += 3;
    }
    Ok(map)
}

/// Reads an `AnyValue`, encoded as a `uint32` type discriminant followed by the value.
pub fn read_any_value(r: &mut &[u8], strings: &mut StringTable) -> Result<AttributeValue, DecodeError> {
    read_any_value_depth(r, strings, 0)
}

fn read_any_value_depth(r: &mut &[u8], strings: &mut StringTable, depth: usize) -> Result<AttributeValue, DecodeError> {
    if depth > MAX_ANY_VALUE_DEPTH {
        return Err(DecodeError::DepthExceeded {
            max: MAX_ANY_VALUE_DEPTH,
        });
    }

    let value_type = read::read_u32(r, "AnyValue type")?;
    let value = match value_type {
        1 => AttributeValue::String(read_streaming_string(r, strings, "AnyValue string")?),
        2 => AttributeValue::Bool(read::read_bool(r, "AnyValue bool")?),
        3 => AttributeValue::Float(read::read_f64(r, "AnyValue double")?),
        4 => AttributeValue::Int(read::read_i64(r, "AnyValue int")?),
        5 => AttributeValue::Bytes(read::read_bytes(r, "AnyValue bytes")?),
        6 => {
            // Array: flat `[type, value, ...]` with two slots per element.
            let len = read::read_array_len(r, "AnyValue array")?;
            if len % 2 != 0 {
                return Err(DecodeError::InvalidArrayValueLen { len });
            }
            let mut values = Vec::with_capacity((len / 2) as usize);
            let mut i = 0;
            while i < len {
                values.push(read_any_value_depth(r, strings, depth + 1)?);
                i += 2;
            }
            AttributeValue::Array(values)
        }
        7 => {
            // Key-value list: flat `[key, type, value, ...]` with three slots per entry.
            let len = read::read_array_len(r, "AnyValue keyValueList")?;
            if len % 3 != 0 {
                return Err(DecodeError::InvalidAttributeArrayLen { len });
            }
            let mut entries = Vec::with_capacity((len / 3) as usize);
            let mut i = 0;
            while i < len {
                let key = read_streaming_string(r, strings, "AnyValue keyValueList key")?;
                let value = read_any_value_depth(r, strings, depth + 1)?;
                entries.push((key, value));
                i += 3;
            }
            AttributeValue::KeyValueList(entries)
        }
        other => return Err(DecodeError::UnknownAnyValueType { value_type: other }),
    };
    Ok(value)
}
