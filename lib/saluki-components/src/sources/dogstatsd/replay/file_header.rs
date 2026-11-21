//! Provides parsing and writing of Datadog capture file headers.

use std::io::{self, Write};

use saluki_error::{generic_error, GenericError};

/// Datadog capture file format version.
pub(super) const DATADOG_FILE_VERSION: u8 = 3;

/// Minimum file version that carries a tagger state trailer.
pub(super) const MIN_STATE_VERSION: u8 = 2;

/// Minimum file version that records timestamps in nanoseconds (earlier versions use seconds).
pub(super) const MIN_NANO_VERSION: u8 = 3;

pub(super) const VERSION_INDEX: usize = 4;
pub(super) const DATADOG_HEADER: [u8; 8] = [0xD4, 0x74, 0xD0, 0x60, 0xF0, 0xFF, 0x00, 0x00];

/// Returns whether the buffer begins with a valid Datadog capture header.
pub(super) fn valid_header(buf: &[u8]) -> bool {
    let Some(header) = buf.first_chunk::<{ DATADOG_HEADER.len() }>() else {
        return false;
    };

    // The version byte only has to carry the header's bits; the file version is OR'd into it.
    let expected_version_bits = DATADOG_HEADER[VERSION_INDEX];
    header[..VERSION_INDEX] == DATADOG_HEADER[..VERSION_INDEX]
        && header[VERSION_INDEX] & expected_version_bits == expected_version_bits
        && header[VERSION_INDEX + 1..] == DATADOG_HEADER[VERSION_INDEX + 1..]
}

/// Parses the Datadog capture file version from the given buffer.
///
/// The caller must first check the buffer with [`valid_header`].
pub(super) fn file_version(buf: &[u8]) -> Result<u8, GenericError> {
    debug_assert!(valid_header(buf), "file_version requires a valid capture header");

    let version = 0xF0 ^ buf[VERSION_INDEX];
    if version > DATADOG_FILE_VERSION {
        return Err(generic_error!("Unsupported capture file version: {}.", version));
    }

    Ok(version)
}

/// Writes the Datadog capture file header.
pub(super) fn write_header<W: Write>(writer: &mut W) -> io::Result<()> {
    let mut header = DATADOG_HEADER;
    header[VERSION_INDEX] |= DATADOG_FILE_VERSION;
    writer.write_all(&header)
}

#[cfg(test)]
mod tests {
    use std::io::{self, ErrorKind};

    use super::{file_version, valid_header, write_header, DATADOG_FILE_VERSION, DATADOG_HEADER, VERSION_INDEX};

    #[test]
    fn test_header_format() {
        let mut bytes = Vec::new();
        write_header(&mut bytes).expect("header should write");

        assert!(valid_header(&bytes));
        assert_eq!(
            file_version(&bytes).expect("version should parse"),
            DATADOG_FILE_VERSION
        );

        for (index, expected) in DATADOG_HEADER.iter().enumerate() {
            if index == VERSION_INDEX {
                assert_eq!(bytes[index], expected | DATADOG_FILE_VERSION);
            } else {
                assert_eq!(bytes[index], *expected);
            }
        }
    }

    #[test]
    fn test_header_format_error() {
        let test_cases = [
            ("No error but no bytes written", 0, None, ErrorKind::WriteZero),
            (
                "Error and less bytes written than datadogHeader",
                1,
                Some(io::Error::new(ErrorKind::InvalidInput, "invalid")),
                ErrorKind::InvalidInput,
            ),
            (
                "Error and more bytes written than datadogHeader",
                500,
                Some(io::Error::new(ErrorKind::InvalidInput, "invalid")),
                ErrorKind::InvalidInput,
            ),
        ];

        for (name, bytes_to_write, error, expected_kind) in test_cases {
            let error = match write_header(&mut ErrorWriter { bytes_to_write, error }) {
                Ok(_) => panic!("{} should fail", name),
                Err(error) => error,
            };
            assert_eq!(error.kind(), expected_kind, "{}", name);
        }
    }

    #[test]
    fn write_header_retries_short_writes() {
        let mut writer = OneByteWriter(Vec::new());
        write_header(&mut writer).expect("short writes should be retried");

        let mut expected = Vec::new();
        write_header(&mut expected).expect("header should write");
        assert_eq!(writer.0, expected);
    }

    #[test]
    fn test_format_matcher() {
        assert!(valid_header(&DATADOG_HEADER));

        let bad_datadog_header = [0xD4, 0x74, 0xD0, 0x66, 0xF0, 0xFF, 0x00, 0x00];
        assert!(!valid_header(&bad_datadog_header));
    }

    struct ErrorWriter {
        bytes_to_write: usize,
        error: Option<io::Error>,
    }

    impl io::Write for ErrorWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            match self.error.take() {
                Some(error) => Err(error),
                None => Ok(self.bytes_to_write),
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct OneByteWriter(Vec<u8>);

    impl io::Write for OneByteWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let Some(&byte) = buf.first() else {
                return Ok(0);
            };
            self.0.push(byte);
            Ok(1)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}
