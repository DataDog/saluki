//! The subset of the [`byteorder`][upstream] 1.x API that our dependencies use.
//!
//! The `[patch.crates-io]` section of the workspace manifest replaces crates.io `byteorder` with this crate. It keeps
//! upstream's names, signatures, and behavior, but only for the items that our dependencies use. When a dependency
//! needs an item that's missing here, the build fails instead of behaving differently, and the item can be added then.
//!
//! The API and behavior are derived from `byteorder` 1.5.0 by Andrew Gallant, licensed under `Unlicense OR MIT`.
//!
//! [upstream]: https://github.com/BurntSushi/byteorder

#![deny(missing_docs)]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(feature = "std")]
use std::io;

/// Reads and writes numbers in a specific byte order.
pub trait ByteOrder {
    /// Reads an unsigned 32-bit integer from the first four bytes of `buf`.
    ///
    /// # Panics
    ///
    /// Panics when `buf.len() < 4`.
    fn read_u32(buf: &[u8]) -> u32;

    /// Writes an unsigned 32-bit integer to the first four bytes of `buf`.
    ///
    /// # Panics
    ///
    /// Panics when `buf.len() < 4`.
    fn write_u32(buf: &mut [u8], n: u32);

    /// Writes an unsigned 64-bit integer to the first eight bytes of `buf`.
    ///
    /// # Panics
    ///
    /// Panics when `buf.len() < 8`.
    fn write_u64(buf: &mut [u8], n: u64);
}

/// Little-endian byte order.
///
/// This type has no values: use it as a type parameter.
pub enum LittleEndian {}

/// An alias for [`LittleEndian`].
pub type LE = LittleEndian;

impl ByteOrder for LittleEndian {
    #[inline]
    fn read_u32(buf: &[u8]) -> u32 {
        u32::from_le_bytes(buf[..4].try_into().unwrap())
    }

    #[inline]
    fn write_u32(buf: &mut [u8], n: u32) {
        buf[..4].copy_from_slice(&n.to_le_bytes());
    }

    #[inline]
    fn write_u64(buf: &mut [u8], n: u64) {
        buf[..8].copy_from_slice(&n.to_le_bytes());
    }
}

/// Extends [`io::Write`] with methods for writing numbers.
///
/// Each method writes the whole value with [`io::Write::write_all`], and returns its error if it fails.
#[cfg(feature = "std")]
pub trait WriteBytesExt: io::Write {
    /// Writes an unsigned 8-bit integer.
    #[inline]
    fn write_u8(&mut self, n: u8) -> io::Result<()> {
        self.write_all(&[n])
    }

    /// Writes an unsigned 32-bit integer in the byte order `T`.
    #[inline]
    fn write_u32<T>(&mut self, n: u32) -> io::Result<()>
    where
        T: ByteOrder,
    {
        let mut buf = [0; 4];
        T::write_u32(&mut buf, n);
        self.write_all(&buf)
    }

    /// Writes a signed 32-bit integer in the byte order `T`.
    #[inline]
    fn write_i32<T>(&mut self, n: i32) -> io::Result<()>
    where
        T: ByteOrder,
    {
        self.write_u32::<T>(n as u32)
    }

    /// Writes an unsigned 64-bit integer in the byte order `T`.
    #[inline]
    fn write_u64<T>(&mut self, n: u64) -> io::Result<()>
    where
        T: ByteOrder,
    {
        let mut buf = [0; 8];
        T::write_u64(&mut buf, n);
        self.write_all(&buf)
    }

    /// Writes a signed 64-bit integer in the byte order `T`.
    #[inline]
    fn write_i64<T>(&mut self, n: i64) -> io::Result<()>
    where
        T: ByteOrder,
    {
        self.write_u64::<T>(n as u64)
    }

    /// Writes an IEEE 754 single-precision floating point number in the byte order `T`.
    #[inline]
    fn write_f32<T>(&mut self, n: f32) -> io::Result<()>
    where
        T: ByteOrder,
    {
        self.write_u32::<T>(n.to_bits())
    }

    /// Writes an IEEE 754 double-precision floating point number in the byte order `T`.
    #[inline]
    fn write_f64<T>(&mut self, n: f64) -> io::Result<()>
    where
        T: ByteOrder,
    {
        self.write_u64::<T>(n.to_bits())
    }
}

#[cfg(feature = "std")]
impl<W> WriteBytesExt for W where W: io::Write + ?Sized {}

#[cfg(all(test, feature = "std"))]
mod tests {
    use std::io::{self, Write};

    use super::*;

    type WriteFn = fn(&mut dyn Write) -> io::Result<()>;
    type WriteCase = (&'static str, WriteFn, &'static [u8]);
    type FullFn = fn() -> io::Result<usize>;

    /// Every `WriteBytesExt` method, with a value whose encoding shows its byte order, and that encoding.
    const WRITE_CASES: &[WriteCase] = &[
        ("write_u8", |w| w.write_u8(0xAB), &[0xAB]),
        (
            "write_u32",
            |w| w.write_u32::<LE>(0x0102_0304),
            &[0x04, 0x03, 0x02, 0x01],
        ),
        ("write_i32", |w| w.write_i32::<LE>(-2), &[0xFE, 0xFF, 0xFF, 0xFF]),
        (
            "write_u64",
            |w| w.write_u64::<LE>(0x0102_0304_0506_0708),
            &[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01],
        ),
        (
            "write_i64",
            |w| w.write_i64::<LE>(-2),
            &[0xFE, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
        ),
        ("write_f32", |w| w.write_f32::<LE>(-1.5), &[0x00, 0x00, 0xC0, 0xBF]),
        // A NaN with a payload, to check that the bits are written unchanged.
        (
            "write_f32",
            |w| w.write_f32::<LE>(f32::from_bits(0x7FC0_0001)),
            &[0x01, 0x00, 0xC0, 0x7F],
        ),
        (
            "write_f64",
            |w| w.write_f64::<LE>(-1.5),
            &[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xF8, 0xBF],
        ),
        (
            "write_f64",
            |w| w.write_f64::<LE>(-0.0),
            &[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80],
        ),
    ];

    /// Accepts at most one byte per call, and fails every other call with `ErrorKind::Interrupted`.
    struct TricklingWriter {
        written: Vec<u8>,
        interrupt: bool,
    }

    impl Write for TricklingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.interrupt = !self.interrupt;
            if self.interrupt {
                return Err(io::ErrorKind::Interrupted.into());
            }
            self.written.extend_from_slice(&buf[..1]);
            Ok(1)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Accepts `capacity` bytes, then answers every call with `full`.
    struct FullWriter {
        capacity: usize,
        full: FullFn,
    }

    impl Write for FullWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.capacity == 0 {
                return (self.full)();
            }
            let n = buf.len().min(self.capacity);
            self.capacity -= n;
            Ok(n)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn little_endian_uses_only_the_leading_bytes_of_the_buffer() {
        assert_eq!(LE::read_u32(&[0x04, 0x03, 0x02, 0x01, 0xFF]), 0x0102_0304);

        let mut buf = [0xEE; 9];
        LE::write_u32(&mut buf, 0x0102_0304);
        assert_eq!(buf, [0x04, 0x03, 0x02, 0x01, 0xEE, 0xEE, 0xEE, 0xEE, 0xEE]);

        let mut buf = [0xEE; 9];
        LE::write_u64(&mut buf, 0x0102_0304_0506_0708);
        assert_eq!(buf, [0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, 0xEE]);
    }

    #[test]
    #[should_panic]
    fn read_u32_panics_on_a_short_buffer() {
        LE::read_u32(&[0; 3]);
    }

    #[test]
    #[should_panic]
    fn write_u32_panics_on_a_short_buffer() {
        LE::write_u32(&mut [0; 3], 0);
    }

    #[test]
    #[should_panic]
    fn write_u64_panics_on_a_short_buffer() {
        LE::write_u64(&mut [0; 7], 0);
    }

    #[test]
    fn write_bytes_ext_writes_little_endian() {
        for (name, write, expected) in WRITE_CASES {
            let mut written = Vec::new();
            write(&mut written).unwrap();
            assert_eq!(written, *expected, "{name}");
        }
    }

    #[test]
    fn write_bytes_ext_completes_partial_and_interrupted_writes() {
        for (name, write, expected) in WRITE_CASES {
            let mut writer = TricklingWriter {
                written: Vec::new(),
                interrupt: false,
            };
            write(&mut writer).unwrap();
            assert_eq!(writer.written, *expected, "{name}");
        }
    }

    #[test]
    fn write_bytes_ext_returns_write_errors() {
        let cases: [(FullFn, io::ErrorKind); 2] = [
            // `write_all` reports a writer that stops accepting bytes as `WriteZero`.
            (|| Ok(0), io::ErrorKind::WriteZero),
            (|| Err(io::ErrorKind::BrokenPipe.into()), io::ErrorKind::BrokenPipe),
        ];
        for (name, write, expected) in WRITE_CASES {
            for (full, kind) in cases {
                let mut writer = FullWriter {
                    capacity: expected.len() - 1,
                    full,
                };
                assert_eq!(write(&mut writer).unwrap_err().kind(), kind, "{name}");
            }
        }
    }
}
