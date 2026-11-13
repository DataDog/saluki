//! Reader for Datadog DogStatsD capture files.
//!
//! Decodes a `.dog` or `.dog.zstd` capture file into the sequence of `UnixDogstatsdMsg` records it
//! contains, plus the optional `TaggerState` trailer.

use std::{
    fs,
    io::{BufRead as _, BufReader, Read},
    path::Path,
};

use datadog_protos::agent::{TaggerState, UnixDogstatsdMsg};
use prost::Message;
use saluki_error::{generic_error, GenericError};

use super::file_header::{file_version, valid_header, DATADOG_HEADER, MIN_NANO_VERSION, MIN_STATE_VERSION};

const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
const LENGTH_PREFIX_SIZE: usize = 4;

/// Timestamp resolution recorded in a capture file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimestampResolution {
    /// Timestamps are recorded in whole seconds (file version < 3).
    Seconds,
    /// Timestamps are recorded in nanoseconds (file version >= 3).
    Nanoseconds,
}

/// Reads back a DogStatsD traffic capture file.
#[derive(Debug)]
pub struct TrafficCaptureReader {
    contents: Vec<u8>,
    version: u8,
    offset: usize,
}

impl TrafficCaptureReader {
    /// Opens a capture file at the given path.
    ///
    /// Detects zstd-compressed inputs by magic bytes and decompresses transparently. Validates the
    /// Datadog capture header and parses the file version.
    pub fn from_path(path: &Path) -> Result<Self, GenericError> {
        let file = fs::File::open(path)
            .map_err(|e| generic_error!("Failed to read capture file '{}': {}", path.display(), e))?;
        Self::read_capture(file, &format!("capture file '{}'", path.display()))
    }

    /// Reads a capture from an already-open source, such as a file descriptor.
    ///
    /// Detects zstd-compressed inputs by magic bytes and decompresses transparently. Validates the
    /// Datadog capture header and parses the file version.
    ///
    /// Compressed input is decoded as it is read, so a failing reader also stops decompression.
    pub fn from_reader(reader: impl Read) -> Result<Self, GenericError> {
        Self::read_capture(reader, "capture file")
    }

    fn read_capture(reader: impl Read, source: &str) -> Result<Self, GenericError> {
        let mut input = BufReader::new(reader);
        let is_compressed = input
            .fill_buf()
            .map_err(|e| generic_error!("Failed to read {source}: {e}"))?
            .starts_with(&ZSTD_MAGIC);

        let mut contents = Vec::new();
        if is_compressed {
            zstd::stream::read::Decoder::with_buffer(input)
                .and_then(|mut decoder| decoder.read_to_end(&mut contents))
                .map_err(|e| generic_error!("Failed to decompress {source}: {e}"))?;
        } else {
            input
                .read_to_end(&mut contents)
                .map_err(|e| generic_error!("Failed to read {source}: {e}"))?;
        }

        if !valid_header(&contents) {
            return Err(generic_error!(
                "{source} does not begin with a valid Datadog capture header."
            ));
        }

        let version = file_version(&contents)?;

        Ok(Self {
            contents,
            version,
            offset: DATADOG_HEADER.len(),
        })
    }

    /// Returns the capture file version.
    pub fn version(&self) -> u8 {
        self.version
    }

    /// Returns the timestamp resolution implied by the file version.
    pub fn timestamp_resolution(&self) -> TimestampResolution {
        if self.version < MIN_NANO_VERSION {
            TimestampResolution::Seconds
        } else {
            TimestampResolution::Nanoseconds
        }
    }

    /// Rewinds the record sequence to the first captured DogStatsD record.
    pub fn rewind(&mut self) {
        self.offset = DATADOG_HEADER.len();
    }

    /// Reads the next captured DogStatsD record from the file.
    ///
    /// Returns `Ok(None)` when the stream of records is exhausted: at EOF, at the four-byte state
    /// separator that introduces the tagger trailer, or at a truncated record boundary.
    pub fn read_next(&mut self) -> Result<Option<UnixDogstatsdMsg>, GenericError> {
        if self.offset + LENGTH_PREFIX_SIZE > self.contents.len() {
            return Ok(None);
        }

        let size_bytes = &self.contents[self.offset..self.offset + LENGTH_PREFIX_SIZE];
        let size = u32::from_le_bytes(size_bytes.try_into().expect("length prefix is 4 bytes")) as usize;
        self.offset += LENGTH_PREFIX_SIZE;

        // The writer emits a zero-length prefix to mark the start of the tagger state trailer; treat
        // that (and any size that would overrun the buffer) as the end of the record stream.
        if size == 0 || self.offset + size > self.contents.len() {
            // A zero-length prefix is the legitimate trailer marker. A non-zero `size` that overruns the buffer is a
            // corrupt/oversized length prefix being silently read as clean EOF, which drops every following
            // well-formed record. Surface the corrupt case as distinct from a real trailer.
            saluki_antithesis::always_or_unreachable!(
                size == 0,
                "replay read_next stopped at the real trailer, not on a corrupt length prefix",
                { "size": size, "offset": self.offset, "len": self.contents.len() }
            );

            return Ok(None);
        }

        let msg = UnixDogstatsdMsg::decode(&self.contents[self.offset..self.offset + size])
            .map_err(|e| generic_error!("Failed to decode captured DogStatsD record: {}", e))?;
        self.offset += size;

        Ok(Some(msg))
    }

    /// After `read_next` returns `None`, returns whether it stopped at the zero-length state separator.
    #[cfg(test)]
    pub(super) fn stopped_at_state_separator(&self) -> bool {
        self.offset >= DATADOG_HEADER.len() + LENGTH_PREFIX_SIZE
            && self.contents[self.offset - LENGTH_PREFIX_SIZE..self.offset] == [0; LENGTH_PREFIX_SIZE]
    }

    /// Reads the tagger state trailer from the end of the capture file.
    ///
    /// Returns `Ok(None)` if the file version predates state support, or if the trailer is empty.
    /// Does not modify the read offset, so it can be called independently of `read_next`.
    pub fn read_state(&self) -> Result<Option<TaggerState>, GenericError> {
        if self.version < MIN_STATE_VERSION {
            return Ok(None);
        }

        let len = self.contents.len();
        if len < LENGTH_PREFIX_SIZE {
            return Ok(None);
        }

        let size_bytes = &self.contents[len - LENGTH_PREFIX_SIZE..len];
        let size = u32::from_le_bytes(size_bytes.try_into().expect("length suffix is 4 bytes")) as usize;
        if size == 0 {
            return Ok(None);
        }

        if size + LENGTH_PREFIX_SIZE > len {
            return Err(generic_error!(
                "Tagger state trailer size ({}) exceeds capture file length ({}).",
                size,
                len
            ));
        }

        let state_start = len - LENGTH_PREFIX_SIZE - size;
        let state = TaggerState::decode(&self.contents[state_start..len - LENGTH_PREFIX_SIZE])
            .map_err(|e| generic_error!("Failed to decode tagger state trailer: {}", e))?;
        Ok(Some(state))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::super::test_support::{run_capture, temp_dir};
    use super::*;
    use crate::sources::dogstatsd::replay::writer::{CaptureRecord, TrafficCaptureWriter};

    #[test]
    fn plain_capture_round_trip() {
        let (_dir, path) = run_capture(
            &TrafficCaptureWriter::new(1),
            false,
            [sample_record(100, b"metric.a:1|c", 11)],
        );

        let mut reader = TrafficCaptureReader::from_path(&path).expect("reader should open");
        assert_eq!(reader.version(), 3);
        assert_eq!(reader.timestamp_resolution(), TimestampResolution::Nanoseconds);

        let msg = reader.read_next().expect("read should succeed").expect("first record");
        assert_eq!(msg.timestamp, 100);
        assert_eq!(msg.payload, b"metric.a:1|c");
        assert_eq!(msg.payload_size, msg.payload.len() as i32);
        assert_eq!(msg.pid, 11);

        assert!(reader.read_next().expect("read should succeed").is_none());
    }

    #[test]
    fn compressed_capture_round_trip() {
        let (_dir, path) = run_capture(
            &TrafficCaptureWriter::new(1),
            true,
            [
                sample_record(1, b"metric.a:1|c", 1),
                sample_record(2, b"metric.b:2|c", 1),
                sample_record(3, b"metric.c:3|c", 1),
            ],
        );

        let mut reader = TrafficCaptureReader::from_path(&path).expect("reader should open");

        for expected_ts in [1, 2, 3] {
            let msg = reader.read_next().expect("read should succeed").expect("record");
            assert_eq!(msg.timestamp, expected_ts);
        }

        assert!(reader.read_next().expect("read should succeed").is_none());

        reader.rewind();
        let msg = reader
            .read_next()
            .expect("read should succeed")
            .expect("first record after rewind");
        assert_eq!(msg.timestamp, 1);
    }

    #[test]
    fn from_reader_reads_the_opened_capture_after_path_changes() {
        let (_dir, path) = run_capture(
            &TrafficCaptureWriter::new(1),
            false,
            [sample_record(100, b"metric.a:1|c", 11)],
        );
        let file = fs::File::open(&path).expect("capture should open");
        let replacement = path.with_extension("replacement");
        fs::write(&replacement, b"not a capture file").expect("replacement capture should be written");
        fs::rename(&replacement, &path).expect("capture path should be replaced");

        let mut reader = TrafficCaptureReader::from_reader(file).expect("reader should use the opened capture");
        let msg = reader.read_next().expect("read should succeed").expect("record");

        assert_eq!(msg.payload, b"metric.a:1|c");
    }

    #[test]
    fn from_reader_surfaces_input_errors_during_decompression() {
        let (_dir, path) = run_capture(
            &TrafficCaptureWriter::new(1),
            true,
            [sample_record(1, b"metric.a:1|c", 1)],
        );
        let compressed = fs::read(&path).expect("capture readable");
        let reader = std::io::Read::chain(
            &compressed[..ZSTD_MAGIC.len() + 1],
            FailingReader("input failed during decompression"),
        );

        let error = TrafficCaptureReader::from_reader(reader).expect_err("input failure should stop the read");

        assert!(error.to_string().contains("Failed to decompress"), "{error:#}");
        assert!(
            error.to_string().contains("input failed during decompression"),
            "{error:#}"
        );
    }

    #[test]
    fn read_next_stops_at_state_separator() {
        let (_dir, path) = run_capture(&TrafficCaptureWriter::new(1), false, [sample_record(7, b"x:1|c", 5)]);

        let mut reader = TrafficCaptureReader::from_path(&path).expect("reader should open");
        assert!(reader.read_next().expect("first record").is_some());

        // Subsequent calls must yield None rather than attempting to decode the trailer as a record.
        assert!(reader.read_next().expect("trailer boundary").is_none());
        assert!(reader.read_next().expect("idempotent EOF").is_none());
    }

    #[test]
    fn read_next_reads_records_then_stops_cleanly_when_trailer_is_truncated() {
        // The previous name ("truncated_record_returns_none") mischaracterized this test: dropping the final bytes
        // truncates the tagger-state *trailer*, not a record. The single record stays intact, so `read_next` must
        // still return it and then terminate cleanly at the zero-length separator, rather than erroring on the
        // damaged trailer.
        let (_dir, path) = run_capture(
            &TrafficCaptureWriter::new(1),
            false,
            [sample_record(1, b"metric.a:1|c", 7)],
        );

        let bytes = fs::read(&path).expect("capture readable");
        let truncated_path = path.with_extension("truncated");
        fs::write(&truncated_path, &bytes[..bytes.len().saturating_sub(8)]).expect("write truncated");

        let mut reader = TrafficCaptureReader::from_path(&truncated_path).expect("reader should open");
        let record = reader
            .read_next()
            .expect("record read should succeed")
            .expect("intact record should still be recovered");
        assert_eq!(record.payload, b"metric.a:1|c");
        assert_eq!(record.pid, 7);
        assert!(reader.read_next().expect("clean EOF after truncated trailer").is_none());
    }

    #[test]
    fn read_next_treats_corrupt_length_prefix_as_end_of_stream() {
        // Regression coverage for read_next's documented corrupt/oversized-length branch: a *non-zero* length prefix
        // that overruns the buffer must be treated as a clean end-of-stream (`Ok(None)`) rather than decoding past the
        // end of the buffer. This is distinct from the legitimate zero-length trailer marker (covered by
        // `read_next_stops_at_state_separator`).
        let (_dir, path) = run_capture(
            &TrafficCaptureWriter::new(1),
            false,
            [sample_record(1, b"metric.a:1|c", 1)],
        );

        let mut bytes = fs::read(&path).expect("capture readable");
        // Overwrite the first record's 4-byte little-endian length prefix (immediately after the 8-byte header) with a
        // value far larger than the remaining file.
        let prefix_start = DATADOG_HEADER.len();
        bytes[prefix_start..prefix_start + LENGTH_PREFIX_SIZE].copy_from_slice(&u32::MAX.to_le_bytes());
        let corrupt_path = path.with_extension("corrupt");
        fs::write(&corrupt_path, &bytes).expect("write corrupt capture");

        let mut reader = TrafficCaptureReader::from_path(&corrupt_path).expect("reader should open");
        assert!(
            reader
                .read_next()
                .expect("corrupt length prefix should read as clean EOF")
                .is_none(),
            "an oversized length prefix must terminate the record stream, not decode past the buffer"
        );
    }

    #[test]
    fn bad_header_is_rejected() {
        let dir = temp_dir();
        let tmp = dir.path().join("bad-header");
        fs::write(&tmp, b"this is not a capture file").expect("write garbage");

        let err = TrafficCaptureReader::from_path(&tmp).expect_err("bad header should fail");
        assert!(err.to_string().contains("Datadog capture header"));
    }

    fn sample_record(timestamp_ns: i64, payload: &[u8], pid: i32) -> CaptureRecord {
        CaptureRecord {
            timestamp_ns,
            payload: payload.to_vec(),
            pid: Some(pid),
            ancillary: Vec::new(),
            container_id: Some(format!("container_id://container-{}", pid)),
        }
    }

    struct FailingReader(&'static str);

    impl Read for FailingReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other(self.0))
        }
    }
}
