//! Payload primitives shared by every FIT payload in this crate.
//!
//! The helpers are protocol-neutral: the error wording names FIT, not any one
//! application protocol, because both the Checks records and the anomaly event records
//! use them.

use std::io;

use saluki_fit::MAX_RING_CAPACITY;

// The ring reserves an eight-byte record header and an eight-byte gap.
pub(crate) const MAX_PAYLOAD: usize = MAX_RING_CAPACITY - 16;

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub(crate) fn add_size(total: usize, extra: usize) -> io::Result<usize> {
    let size = total
        .checked_add(extra)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "payload size overflow"))?;
    if size > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "payload exceeds the FIT maximum",
        ));
    }
    Ok(size)
}

pub(crate) fn string_size(value: &str) -> io::Result<usize> {
    u32::try_from(value.len()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "string exceeds u32 length"))?;
    add_size(4, value.len())
}

pub(crate) fn strings_size(values: &[String]) -> io::Result<usize> {
    u32::try_from(values.len()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "string count exceeds u32"))?;
    values
        .iter()
        .try_fold(4, |total, value| add_size(total, string_size(value)?))
}

pub(crate) fn payload_size(parts: &[usize]) -> io::Result<usize> {
    parts.iter().try_fold(0, |total, part| add_size(total, *part))
}

pub(crate) struct Writer(Vec<u8>);

impl Writer {
    pub(crate) fn new(size: usize) -> io::Result<Self> {
        if size > MAX_PAYLOAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "payload exceeds the FIT maximum",
            ));
        }
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(size).map_err(io::Error::other)?;
        Ok(Self(bytes))
    }

    pub(crate) fn i32(&mut self, value: i32) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }
    pub(crate) fn u32(&mut self, value: u32) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }
    pub(crate) fn u64(&mut self, value: u64) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }
    pub(crate) fn f64(&mut self, value: f64) {
        self.u64(value.to_bits());
    }
    pub(crate) fn string(&mut self, value: &str) {
        self.u32(value.len() as u32);
        self.0.extend_from_slice(value.as_bytes());
    }
    pub(crate) fn strings(&mut self, values: &[String]) {
        self.u32(values.len() as u32);
        for value in values {
            self.string(value);
        }
    }
    pub(crate) fn finish(self, expected: usize) -> Vec<u8> {
        debug_assert_eq!(self.0.len(), expected);
        self.0
    }
}

pub(crate) struct Reader<'a> {
    remaining: &'a [u8],
}

impl<'a> Reader<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> io::Result<Self> {
        if bytes.len() > MAX_PAYLOAD {
            return Err(invalid("payload exceeds the FIT maximum"));
        }
        Ok(Self { remaining: bytes })
    }
    fn take(&mut self, count: usize) -> io::Result<&'a [u8]> {
        if count > self.remaining.len() {
            return Err(invalid("truncated payload"));
        }
        let (value, remaining) = self.remaining.split_at(count);
        self.remaining = remaining;
        Ok(value)
    }
    pub(crate) fn i32(&mut self) -> io::Result<i32> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub(crate) fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub(crate) fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub(crate) fn f64(&mut self) -> io::Result<f64> {
        Ok(f64::from_bits(self.u64()?))
    }
    pub(crate) fn string(&mut self) -> io::Result<String> {
        let len = self.u32()? as usize;
        let value = simdutf8::basic::from_utf8(self.take(len)?).map_err(|_| invalid("invalid UTF-8 payload"))?;
        Ok(value.to_owned())
    }
    pub(crate) fn strings(&mut self) -> io::Result<Vec<String>> {
        let count = self.u32()? as usize;
        if count > self.remaining.len() / 4 {
            return Err(invalid("string count exceeds remaining bytes"));
        }
        let mut values = Vec::new();
        values.try_reserve_exact(count).map_err(io::Error::other)?;
        for _ in 0..count {
            values.push(self.string()?);
        }
        Ok(values)
    }
    pub(crate) fn finish(self) -> io::Result<()> {
        if self.remaining.is_empty() {
            Ok(())
        } else {
            Err(invalid("trailing payload bytes"))
        }
    }
}
