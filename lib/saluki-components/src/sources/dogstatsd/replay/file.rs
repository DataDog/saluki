#![allow(dead_code)]

use std::io::{self, Write};

use saluki_error::{generic_error, GenericError};

/// Datadog capture file format version.
pub(super) const DATADOG_FILE_VERSION: u8 = 3;

/// Minimum file format version that includes trailing replay state.
pub(super) const MIN_STATE_VERSION: u8 = 2;

/// Minimum file format version that stores timestamps in nanoseconds.
pub(super) const MIN_NANO_VERSION: u8 = 3;

pub(super) const VERSION_INDEX: usize = 4;
pub(super) const DATADOG_HEADER: [u8; 8] = [0xD4, 0x74, 0xD0, 0x60, 0xF0, 0xFF, 0x00, 0x00];
const ERR_HEADER_WRITE: &str = "capture file header could not be fully written to buffer";

/// Returns whether the buffer begins with a valid Datadog capture header.
pub(super) fn datadog_matcher(buf: &[u8]) -> bool {
    if buf.len() < DATADOG_HEADER.len() {
        return false;
    }

    for (index, expected) in DATADOG_HEADER.iter().enumerate() {
        let actual = buf[index];
        if index == VERSION_INDEX {
            if actual & expected != *expected {
                return false;
            }
        } else if actual != *expected {
            return false;
        }
    }

    true
}

/// Parses the Datadog capture file version from the given buffer.
pub(super) fn file_version(buf: &[u8]) -> Result<u8, GenericError> {
    if !datadog_matcher(buf) {
        return Err(generic_error!("Cannot verify file version from invalid capture header."));
    }

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
    let written = writer.write(&header)?;
    if written < DATADOG_HEADER.len() {
        return Err(io::Error::new(io::ErrorKind::WriteZero, ERR_HEADER_WRITE));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{self, ErrorKind};

    use super::{DATADOG_FILE_VERSION, DATADOG_HEADER, VERSION_INDEX, datadog_matcher, file_version, write_header};

    #[test]
    fn test_header_format() {
        let mut bytes = Vec::new();
        write_header(&mut bytes).expect("header should write");

        assert!(datadog_matcher(&bytes));
        assert_eq!(file_version(&bytes).expect("version should parse"), DATADOG_FILE_VERSION);

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
            ("No error but less bytes written than datadogHeader", 1, None, ErrorKind::WriteZero),
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
    fn test_format_matcher() {
        assert!(datadog_matcher(&DATADOG_HEADER));

        let bad_datadog_header = [0xD4, 0x74, 0xD0, 0x66, 0xF0, 0xFF, 0x00, 0x00];
        assert!(!datadog_matcher(&bad_datadog_header));
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
}
