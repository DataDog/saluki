/// Encodes a `usize` as a variable-length integer (LEB128) into `buf`.
pub fn encode_varint(mut value: usize, buf: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        buf.push(byte);
        if value == 0 {
            break;
        }
    }
}

/// Encodes a `u128` as a variable-length integer (LEB128) into `buf`.
pub fn encode_varint_u128(mut value: u128, buf: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        buf.push(byte);
        if value == 0 {
            break;
        }
    }
}

/// Decodes a variable-length integer (LEB128) from `buf` starting at `idx`.
///
/// Returns `(value, bytes_consumed)`, or `None` if the buffer is too short or the varint is
/// malformed.
pub fn decode_varint(buf: &[u8], mut idx: usize) -> Option<(usize, usize)> {
    let start = idx;
    let mut value: usize = 0;
    let mut shift = 0;
    loop {
        if idx >= buf.len() {
            return None;
        }
        let byte = buf[idx];
        idx += 1;
        value |= ((byte & 0x7F) as usize) << shift;
        if byte & 0x80 == 0 {
            return Some((value, idx - start));
        }
        shift += 7;
        if shift >= usize::BITS {
            return None;
        }
    }
}

/// Decodes a `u128` variable-length integer (LEB128) from `buf` starting at `idx`.
pub fn decode_varint_u128(buf: &[u8], mut idx: usize) -> Option<(u128, usize)> {
    let start = idx;
    let mut value: u128 = 0;
    let mut shift = 0;
    loop {
        if idx >= buf.len() {
            return None;
        }
        let byte = buf[idx];
        idx += 1;
        value |= ((byte & 0x7F) as u128) << shift;
        if byte & 0x80 == 0 {
            return Some((value, idx - start));
        }
        shift += 7;
        if shift >= 128 {
            return None;
        }
    }
}

/// Encodes a log level as a single byte.
pub fn encode_level(level: &str) -> u8 {
    match level {
        "TRACE" => 0,
        "DEBUG" => 1,
        "INFO" => 2,
        "WARN" => 3,
        "ERROR" => 4,
        _ => 5,
    }
}

/// Decodes a log level byte back to a static string.
pub fn decode_level(byte: u8) -> &'static str {
    match byte {
        0 => "TRACE",
        1 => "DEBUG",
        2 => "INFO",
        3 => "WARN",
        4 => "ERROR",
        _ => "UNKNOWN",
    }
}

/// Writes a varint-length-prefixed byte slice into `buf`.
pub fn write_length_prefixed(buf: &mut Vec<u8>, data: &[u8]) {
    encode_varint(data.len(), buf);
    buf.extend_from_slice(data);
}

/// RLE-encodes a column of small integers: varint(num_runs) [varint(run_len) varint(value) ...].
pub fn rle_encode(values: &[usize], buf: &mut Vec<u8>) {
    if values.is_empty() {
        encode_varint(0, buf);
        return;
    }

    // Count runs first.
    let mut runs: Vec<(usize, usize)> = Vec::new(); // (count, value)
    let mut cur_val = values[0];
    let mut cur_count = 1;
    for &v in &values[1..] {
        if v == cur_val {
            cur_count += 1;
        } else {
            runs.push((cur_count, cur_val));
            cur_val = v;
            cur_count = 1;
        }
    }
    runs.push((cur_count, cur_val));

    encode_varint(runs.len(), buf);
    for (count, value) in runs {
        encode_varint(count, buf);
        encode_varint(value, buf);
    }
}

/// Decodes an RLE-encoded column into a Vec of values.
pub fn rle_decode(buf: &[u8], idx: &mut usize) -> Option<Vec<usize>> {
    let (num_runs, consumed) = decode_varint(buf, *idx)?;
    *idx += consumed;

    let mut values = Vec::new();
    for _ in 0..num_runs {
        let (count, consumed) = decode_varint(buf, *idx)?;
        *idx += consumed;
        let (value, consumed) = decode_varint(buf, *idx)?;
        *idx += consumed;
        values.extend(std::iter::repeat_n(value, count));
    }
    Some(values)
}
