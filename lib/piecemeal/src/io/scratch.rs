//! Scratch buffers and writer.

use std::collections::VecDeque;

use crate::{helpers::sizeof_varint, ProtoResult};

use super::writer::Writer;

/// A scratch buffer.
///
/// Scratch buffers are used to temporarily write message fields as a message is being built, so
/// that it can be written in its entirety to an output stream later.
///
/// `ScratchBuffer` is mostly a superset of `Writer`, but it also provides methods to clear the
/// buffer and read the written bytes to it.
pub trait ScratchBuffer: Writer {
    /// Clears the buffer, removing all values.
    ///
    /// Note that this method has no effect on the allocated capacity of the buffer.
    fn clear(&mut self);

    /// Extracts a slice containing the entire buffer.
    fn as_slice(&self) -> &[u8];
}

impl ScratchBuffer for Vec<u8> {
    fn clear(&mut self) {
        Vec::clear(self);
    }

    fn as_slice(&self) -> &[u8] {
        &self[..]
    }
}

/// A scratch writer.
///
/// When encoding messages, each message needs to be able to specify its own length -- the number of
/// overall bytes in the message -- before any of the message content itself. This is hard to do
/// when building messages one field at a time, since not all fields (and their individual sizes)
/// can be known beforehand.
///
/// We compensate for this by writing into a scratch buffer, a temporary buffer where we can write
/// messages, and their individual fields, without needing to worry about their length. However,
/// this alone is not sufficient for properly writing out the final encoded message, and
/// `ScratchWriter<B>` provides the logic to calculate these lengths and then insert them at proper
/// locations in the final encoded message as it is written to an output stream.
///
/// `ScratchWriter<B>` allows callers to specify when a message is about to be written, so that a
/// marker can be generated, which allows the length of that message to be calculated and stored for
/// final assembly afterwards. When `finalize` is called, the scratch buffer is written to the
/// output stream, while the length markers are used to know when to write the length of each
/// message in between subslices of the scratch buffer.
pub struct ScratchWriter<'a, B> {
    buffer: &'a mut B,
    total_len_bytes: usize,
    len_markers: VecDeque<(usize, u64)>,
}

impl<'a, B: ScratchBuffer> ScratchWriter<'a, B> {
    /// Create a new `ScratchWriter<B>` with the given buffer.
    pub fn new(buffer: &'a mut B) -> Self {
        Self {
            buffer,
            total_len_bytes: 0,
            len_markers: VecDeque::new(),
        }
    }

    /// Tracks the given operation.
    pub fn track_message<F>(&mut self, f: F) -> ProtoResult<()>
    where
        F: FnOnce(&mut Self) -> ProtoResult<()>,
    {
        // Track the before/after size of the message in the scratch buffer.
        let start = self.buffer.as_slice().len();
        f(self)?;
        let end = self.buffer.as_slice().len();

        // Calculate the delta, which is the message's size. We also update `total_len_bytes` with
        // the varint-encoded length (in bytes) of the message size, which lets us backpropagate the
        // size of the length field itself to any parent messages.
        let delta = (end - start) + self.total_len_bytes;
        self.total_len_bytes += sizeof_varint(delta as u64);

        // Insert a length marker for this message.
        self.len_markers.push_back((start, delta as u64));
        Ok(())
    }

    /// Finalize the scratch buffer, writing it to the given writer.
    ///
    /// This will write an initial length for the entire scratch buffer, and write the necessary
    /// lengths for any submessages contained within the scratch buffer.
    ///
    /// ## Errors
    ///
    /// If there is an error writing the scratch buffer into the given writer, an error will be returned.
    pub fn finalize<W>(&mut self, writer: &mut W) -> ProtoResult<()>
    where
        W: Writer,
    {
        let mut start = 0;
        let buf = self.buffer.as_slice();

        // Write the overall length for the scratch buffer, before we iterate any length markers.
        let total_len = self.total_len_bytes + buf.len();
        writer.write_varint(total_len as u64)?;

        while let Some((offset, len)) = self.len_markers.pop_back() {
            // Write everything before the marker.
            let sub_buf = &buf[start..offset];
            writer.pb_write_all(sub_buf)?;

            // Write the length at the given marker offset.
            writer.write_varint(len)?;

            // Update our start offset so our next sub-buffer slice starts at the end of this one.
            start += sub_buf.len();
        }

        // Write final sub-buffer, if there is one.
        let final_sub_buf = &buf[start..];
        if !final_sub_buf.is_empty() {
            writer.pb_write_all(final_sub_buf)?;
        }

        self.buffer.clear();
        self.total_len_bytes = 0;

        Ok(())
    }
}

impl<'a, B: ScratchBuffer> Writer for ScratchWriter<'a, B> {
    fn pb_write_u8(&mut self, x: u8) -> ProtoResult<()> {
        self.buffer.pb_write_u8(x)
    }

    fn pb_write_u32(&mut self, x: u32) -> ProtoResult<()> {
        self.buffer.pb_write_u32(x)
    }

    fn pb_write_i32(&mut self, x: i32) -> ProtoResult<()> {
        self.buffer.pb_write_i32(x)
    }

    fn pb_write_f32(&mut self, x: f32) -> ProtoResult<()> {
        self.buffer.pb_write_f32(x)
    }

    fn pb_write_u64(&mut self, x: u64) -> ProtoResult<()> {
        self.buffer.pb_write_u64(x)
    }

    fn pb_write_i64(&mut self, x: i64) -> ProtoResult<()> {
        self.buffer.pb_write_i64(x)
    }

    fn pb_write_f64(&mut self, x: f64) -> ProtoResult<()> {
        self.buffer.pb_write_f64(x)
    }

    fn pb_write_all(&mut self, buf: &[u8]) -> ProtoResult<()> {
        self.buffer.pb_write_all(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Writer;

    fn varint_field(field_number: u32) -> u32 {
        (field_number << 3) | 0
    }

    fn msg_field(field_number: u32) -> u32 {
        (field_number << 3) | 2
    }

    #[test]
    fn test_scratch_writer() {
        let mut buf = Vec::new();
        let mut writer = ScratchWriter::new(&mut buf);

        // Our imaginary message struct looks like this:
        //
        // message A {
        //   int64 field_a = 1;
        //   int64 field_b = 2;
        //   B field_c = 3;
        // }
        //
        // message B {
        //   int64 field_a = 1;
        //   int64 field_b = 2;
        //   C field_c = 3;
        // }
        //
        // message C {
        //   int64 field_a = 1;
        //   int64 field_b = 2;
        // }

        // We'll approximate the same ordering/nesting etc.

        // Write A->a and A->b and then start writing A->c:
        writer.write_with_tag(varint_field(1), |w| w.write_uint64(42)).unwrap();
        writer.write_with_tag(varint_field(2), |w| w.write_uint64(369)).unwrap();
        writer.write_tag(msg_field(3)).unwrap();
        writer
            .track_message(|w| {
                // Write B->a and B->b and then start writing B->c:
                w.write_with_tag(varint_field(1), |w| w.write_uint64(27)).unwrap();
                w.write_with_tag(varint_field(2), |w| w.write_uint64(309)).unwrap();
                w.write_tag(msg_field(3)).unwrap();
                w.track_message(|w| {
                    // Write C->a and C->b:
                    w.write_with_tag(varint_field(1), |w| w.write_uint64(88)).unwrap();
                    w.write_with_tag(varint_field(2), |w| w.write_uint64(365))
                })
            })
            .unwrap();

        let mut actual = Vec::new();
        writer.finalize(&mut actual).unwrap();

        let expected = &[
            0x13, 0x08, 0x2a, 0x10, 0xf1, 0x02, 0x1a, 0x0c, 0x08, 0x1b, 0x10, 0xb5, 0x02, 0x1a, 0x05, 0x08, 0x58, 0x10,
            0xed, 0x02,
        ];
        assert_eq!(&expected[..], &actual[..]);
    }
}

/*
#[test]
fn test_issue_222() {
    // remember that `serialize_into_vec()` and `serialize_into_slice()` add a
    // length prefix in addition to writing the message itself; important for
    // when you look at the buffer sizes and errors thrown in this test

    struct TestMsg {}

    impl MessageWrite for TestMsg {
        fn write_message<W: Writer>(&self, w: &mut W) -> ProtoResult<()> {
            let bytes = [0x08u8, 0x96u8, 0x01u8];
            for b in bytes {
                // use `write_u8()` in loop because some other functions have
                // hidden writes (length prefixes etc.) inside them.
                w.write_u8(b)?;
            }
            Ok(())
        }

        fn get_size(&self) -> usize {
            3 // corresponding to `bytes` above
        }
    }

    let msg = TestMsg {};
    let v = serialize_into_vec(&msg).unwrap();
    // We would really like to assert that the vector `v` WITHIN
    // `serialize_into_vec()` does not get its capacity modified after
    // initializion with `with_capacity()`, but the only way to do that would be
    // to put an assert within `serialize_into_vec()` itself, which isn't a
    // pattern seen in this project.
    //
    // Instead, we do this. If this check fails, it definitely means that the
    // capacity setting in `serialize_into_vec()` is suboptimal, but passing
    // doesn't guarantee that it is optimal.
    assert_eq!(v.len(), v.capacity());

    let mut buf_len_2 = vec![0x00u8, 0x00u8];
    let mut buf_len_3 = vec![0x00u8, 0x00u8, 0x00u8];
    let mut buf_len_4 = vec![0x00u8, 0x00u8, 0x00u8, 0x00u8];

    assert!(matches!(
        serialize_into_slice(&msg, buf_len_2.as_mut_slice()),
        Err(Error::OutputBufferTooSmall)
    ));
    assert!(matches!(
        // the salient case in issue 222; before bugfix this would have been
        // Err(Error::UnexpectedEndOfBuffer)
        serialize_into_slice(&msg, buf_len_3.as_mut_slice()),
        Err(Error::OutputBufferTooSmall)
    ));
    assert!(matches!(serialize_into_slice(&msg, buf_len_4.as_mut_slice()), Ok(())));
}
*/
