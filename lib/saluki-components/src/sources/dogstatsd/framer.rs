//! DogStatsD framing over a read cursor.
//!
//! Frames are borrowed straight from the I/O buffer. [`ReadCursor`] lends the buffer's bytes and its read index, and a
//! frame always comes from [`ReadCursor::take`], so handing out a frame and advancing the buffer past it are one
//! operation.
//!
//! On stream listeners, each complete length-prefixed outer frame is taken whole, and its messages are cut from the
//! taken payload. Between drains, the front of the buffer is therefore always at an outer frame boundary, and the
//! framer carries no state from one drain to the next.

use std::iter::FusedIterator;

use saluki_io::{buf::ReadCursor, deser::framing::FramingError, net::ListenAddress};
use tracing::trace;

/// How DogStatsD frames are laid out on the wire.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    /// Newline-delimited messages.
    NonStream,
    /// Length-prefixed outer frames, each holding newline-delimited messages.
    Stream,
}

/// Frames DogStatsD messages for one listener.
///
/// The framer holds only configuration, so it can't fall out of step with the buffer it frames.
#[derive(Clone, Copy, Debug)]
pub struct DsdFramer {
    mode: Mode,
    eol_required: bool,
}

impl DsdFramer {
    /// Returns the frames at the front of the cursor's buffer.
    ///
    /// If `capture_outer_frames` is set, each outer frame's payload is yielded as [`Frame::Outer`] before the messages
    /// it holds. It has no effect on non-stream listeners, which have no outer frames.
    pub fn frames<'a>(&self, cursor: ReadCursor<'a>, is_eof: bool, capture_outer_frames: bool) -> Frames<'a> {
        Frames {
            cursor,
            framer: *self,
            is_eof,
            capture_outer_frames,
            outer_payload: None,
            completed_outer_frames: 0,
            failed: false,
        }
    }
}

/// Something found at the front of the buffer.
#[derive(Debug, Eq, PartialEq)]
pub enum Frame<'a> {
    /// A DogStatsD message.
    Message(&'a [u8]),

    /// The payload of an outer frame, yielded before the messages it holds when outer frame capture is on.
    Outer(&'a [u8]),
}

/// The frames at the front of a buffer, for one drain.
///
/// Frames borrow the buffer, not `Frames`, so `Frames` can be used freely while they're held.
///
/// Dropping `Frames` partway through an outer frame skips that frame's remaining messages, since the buffer has already
/// been advanced past them. After an error, `Frames` yields nothing more. A new drain starts afresh, since the framer
/// keeps no state.
pub struct Frames<'a> {
    cursor: ReadCursor<'a>,
    framer: DsdFramer,
    is_eof: bool,
    capture_outer_frames: bool,
    /// What's left of the current outer frame's payload, already taken from the buffer.
    outer_payload: Option<&'a [u8]>,
    completed_outer_frames: usize,
    failed: bool,
}

impl<'a> Frames<'a> {
    /// Returns how many outer frames were fully consumed since the last call.
    pub fn take_completed_outer_frames(&mut self) -> usize {
        std::mem::take(&mut self.completed_outer_frames)
    }

    /// Fuses `Frames` and returns the error.
    #[cold]
    fn fail<T>(&mut self, error: FramingError) -> Option<Result<T, FramingError>> {
        self.failed = true;
        Some(Err(error))
    }

    fn next_line(&mut self) -> Option<Result<&'a [u8], FramingError>> {
        let unread = self.cursor.unread();
        if unread.is_empty() {
            return None;
        }

        let len = match memchr::memchr(b'\n', unread) {
            Some(idx) => return Some(Ok(&self.cursor.take(idx + 1)[..idx])),
            None if !self.is_eof => return None,
            None if self.framer.eol_required => return self.fail(missing_newline(unread.len())),
            None => unread.len(),
        };
        Some(Ok(self.cursor.take(len)))
    }

    fn next_nested(&mut self) -> Option<Result<Frame<'a>, FramingError>> {
        let payload = loop {
            if let Some(payload) = self.outer_payload {
                break payload;
            }

            let payload_len = match outer_frame_len(self.cursor.unread(), self.cursor.capacity(), self.is_eof) {
                Ok(Some(len)) => len,
                Ok(None) => return None,
                Err(e) => return self.fail(e),
            };
            let payload = &self.cursor.take(4 + payload_len)[4..];
            trace!(payload_len = payload.len(), payload = %payload.escape_ascii(), "Extracted outer frame.");

            if payload.is_empty() {
                self.completed_outer_frames += 1;
            } else {
                self.outer_payload = Some(payload);
            }
            if self.capture_outer_frames {
                return Some(Ok(Frame::Outer(payload)));
            }
        };

        let (message, rest) = match memchr::memchr(b'\n', payload) {
            Some(idx) => (&payload[..idx], &payload[idx + 1..]),
            None if self.framer.eol_required => return self.fail(missing_newline(payload.len())),
            None => (payload, &payload[payload.len()..]),
        };

        if rest.is_empty() {
            self.outer_payload = None;
            self.completed_outer_frames += 1;
        } else {
            self.outer_payload = Some(rest);
        }
        Some(Ok(Frame::Message(message)))
    }
}

impl<'a> Iterator for Frames<'a> {
    type Item = Result<Frame<'a>, FramingError>;

    /// Returns the next frame, or `None` if no complete frame remains.
    ///
    /// Incomplete frames stay in the buffer. After an error, any part of the current outer frame that was already taken
    /// is dropped.
    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        match self.framer.mode {
            Mode::NonStream => self.next_line().map(|result| result.map(Frame::Message)),
            Mode::Stream => self.next_nested(),
        }
    }
}

impl FusedIterator for Frames<'_> {}

/// Returns the payload length of the complete length-prefixed frame at the front of `unread`.
///
/// The length prefix is a little-endian `u32`. Returns `Ok(None)` if the frame isn't complete yet. A frame that
/// couldn't fit in the buffer, length prefix included, is invalid.
fn outer_frame_len(unread: &[u8], capacity: usize, is_eof: bool) -> Result<Option<usize>, FramingError> {
    if unread.is_empty() {
        return Ok(None);
    }

    let partial = |needed| {
        if is_eof {
            Err(FramingError::PartialFrame {
                needed,
                remaining: unread.len(),
            })
        } else {
            Ok(None)
        }
    };

    let Some(header) = unread.first_chunk::<4>() else {
        return partial(4);
    };
    let payload_len = u32::from_le_bytes(*header) as usize;
    let frame_len = payload_len.saturating_add(4);
    if frame_len > capacity {
        return Err(FramingError::InvalidFrame {
            frame_len: payload_len,
            reason: "frame length exceeds buffer capacity",
        });
    }
    if unread.len() < frame_len {
        return partial(frame_len);
    }
    Ok(Some(payload_len))
}

const fn missing_newline(frame_len: usize) -> FramingError {
    FramingError::InvalidFrame {
        frame_len,
        reason: "reached EOF without finding newline delimiter",
    }
}

/// Returns the framer for the given listener.
///
/// Stream listeners (TCP, Unix stream sockets) use length-prefixed outer frames holding newline-delimited messages.
/// Other listeners use newline-delimited messages. With `eol_required`, a final message without a trailing newline is
/// invalid.
pub fn get_framer(listen_address: &ListenAddress, eol_required: bool) -> DsdFramer {
    let mode = match listen_address {
        ListenAddress::Tcp(_) | ListenAddress::Unix(_) => Mode::Stream,
        ListenAddress::Udp(_) | ListenAddress::Unixgram(_) | ListenAddress::NamedPipe { .. } => Mode::NonStream,
    };
    DsdFramer { mode, eol_required }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

    use bytes::{Buf as _, BufMut as _};
    use saluki_core::pooling::helpers::get_pooled_object_via_builder;
    use saluki_io::{
        buf::{BytesBuffer, FixedSizeVec},
        deser::framing::FramingError,
        net::ListenAddress,
    };

    use super::{get_framer, DsdFramer, Frame, Mode};

    const CAPACITY: usize = 8196;

    #[derive(Debug, PartialEq)]
    enum Got {
        Message(Vec<u8>),
        Outer(Vec<u8>),
        Completed(usize),
        Error(FramingError),
    }

    fn buffer() -> BytesBuffer {
        get_pooled_object_via_builder::<_, BytesBuffer>(|| FixedSizeVec::with_capacity(CAPACITY))
    }

    fn bytes(data: &[u8]) -> BytesBuffer {
        let mut buf = buffer();
        buf.put_slice(data);
        buf
    }

    fn stream(payloads: &[&[u8]]) -> BytesBuffer {
        let mut buf = buffer();
        for payload in payloads {
            buf.put_u32_le(payload.len() as u32);
            buf.put_slice(payload);
        }
        buf
    }

    const fn nested(eol_required: bool) -> DsdFramer {
        DsdFramer {
            mode: Mode::Stream,
            eol_required,
        }
    }

    const fn direct(eol_required: bool) -> DsdFramer {
        DsdFramer {
            mode: Mode::NonStream,
            eol_required,
        }
    }

    /// Drains up to `limit` frames, recording completed counts as the DogStatsD decode loop sees them.
    ///
    /// Stops at the first error, after checking that `Frames` is fused.
    fn drain(framer: DsdFramer, buf: &mut BytesBuffer, is_eof: bool, capture: bool, limit: usize) -> Vec<Got> {
        let mut got = Vec::new();
        let mut frames = framer.frames(buf.cursor(), is_eof, capture);
        for _ in 0..limit {
            let frame = frames.next();
            let completed = frames.take_completed_outer_frames();
            if completed > 0 {
                got.push(Got::Completed(completed));
            }
            match frame {
                Some(Ok(Frame::Message(message))) => got.push(Got::Message(message.to_vec())),
                Some(Ok(Frame::Outer(outer))) => got.push(Got::Outer(outer.to_vec())),
                Some(Err(e)) => {
                    got.push(Got::Error(e));
                    assert_eq!(frames.next(), None, "fused after an error");
                    assert_eq!(frames.take_completed_outer_frames(), 0);
                    break;
                }
                None => break,
            }
        }
        got
    }

    fn drain_all(framer: DsdFramer, buf: &mut BytesBuffer, is_eof: bool, capture: bool) -> Vec<Got> {
        drain(framer, buf, is_eof, capture, usize::MAX)
    }

    fn message(data: &[u8]) -> Got {
        Got::Message(data.to_vec())
    }

    fn outer(data: &[u8]) -> Got {
        Got::Outer(data.to_vec())
    }

    const fn missing_newline(frame_len: usize) -> Got {
        Got::Error(super::missing_newline(frame_len))
    }

    #[test]
    fn stream_single_outer_frame_multiple_messages() {
        let mut buf = stream(&[b"frame1\nframe2\nframe3\n"]);
        assert_eq!(
            drain_all(nested(false), &mut buf, false, false),
            [
                message(b"frame1"),
                message(b"frame2"),
                Got::Completed(1),
                message(b"frame3")
            ],
            "completion waits for the last message"
        );
        assert!(buf.chunk().is_empty());
    }

    #[test]
    fn stream_multiple_outer_frames_single_message() {
        let mut buf = stream(&[b"frame1\n", b"frame2\n", b"frame3\n"]);
        assert_eq!(
            drain_all(nested(false), &mut buf, false, false),
            [
                Got::Completed(1),
                message(b"frame1"),
                Got::Completed(1),
                message(b"frame2"),
                Got::Completed(1),
                message(b"frame3")
            ]
        );
        assert!(buf.chunk().is_empty());
    }

    #[test]
    fn stream_multiple_outer_frames_multiple_messages() {
        let mut buf = stream(&[b"frame1\nframe2\n", b"frame3\nframe4\n", b"frame5\nframe6\n"]);
        assert_eq!(
            drain_all(nested(false), &mut buf, false, false),
            [
                message(b"frame1"),
                Got::Completed(1),
                message(b"frame2"),
                message(b"frame3"),
                Got::Completed(1),
                message(b"frame4"),
                message(b"frame5"),
                Got::Completed(1),
                message(b"frame6")
            ]
        );
        assert!(buf.chunk().is_empty());
    }

    #[test]
    fn stream_outer_frames_reported_only_when_capture_is_on() {
        let mut buf = stream(&[b"frame1\nframe2\n", b"frame3\n"]);
        assert_eq!(
            drain_all(nested(false), &mut buf, false, false),
            [
                message(b"frame1"),
                Got::Completed(1),
                message(b"frame2"),
                Got::Completed(1),
                message(b"frame3")
            ]
        );

        let mut buf = stream(&[b"frame1\nframe2\n", b"frame3\n"]);
        assert_eq!(
            drain_all(nested(false), &mut buf, false, true),
            [
                outer(b"frame1\nframe2\n"),
                message(b"frame1"),
                Got::Completed(1),
                message(b"frame2"),
                outer(b"frame3\n"),
                Got::Completed(1),
                message(b"frame3")
            ],
            "outer frames come before their messages"
        );
    }

    #[test]
    fn stream_capture_is_chosen_per_drain() {
        let mut buf = stream(&[b"a\nb\n", b"c\n"]);
        assert_eq!(
            drain(nested(false), &mut buf, false, false, 2),
            [message(b"a"), Got::Completed(1), message(b"b")]
        );
        assert_eq!(
            drain_all(nested(false), &mut buf, false, true),
            [outer(b"c\n"), Got::Completed(1), message(b"c")]
        );
    }

    #[test]
    fn stream_zero_length_outer_frames() {
        let mut buf = stream(&[b"", b"a\n"]);
        assert_eq!(
            drain_all(nested(false), &mut buf, false, true),
            [
                Got::Completed(1),
                outer(b""),
                outer(b"a\n"),
                Got::Completed(1),
                message(b"a")
            ],
            "with capture on, a zero-length outer frame reports its completion, then itself"
        );

        let mut buf = stream(&[b"", b"", b"a\n"]);
        assert_eq!(
            drain_all(nested(false), &mut buf, false, false),
            [Got::Completed(3), message(b"a")]
        );
    }

    #[test]
    fn stream_early_stop_skips_rest_of_outer_frame() {
        let mut buf = stream(&[b"aa\nbb\n", b"", b"cc\ndd"]);
        buf.put_slice(&[4, 0, 0, 0, b'e']);

        let held = {
            let mut frames = nested(false).frames(buf.cursor(), false, true);
            assert_eq!(frames.next(), Some(Ok(Frame::Outer(b"aa\nbb\n"))));
            frames.next()
        };
        assert_eq!(held, Some(Ok(Frame::Message(b"aa"))), "frames outlive `Frames`");
        assert_eq!(buf.chunk()[..4], [0, 0, 0, 0], "front is at the next outer frame");

        assert_eq!(
            drain_all(nested(false), &mut buf, false, false),
            [Got::Completed(1), message(b"cc"), Got::Completed(1), message(b"dd")]
        );
        assert_eq!(
            buf.chunk(),
            [4, 0, 0, 0, b'e'],
            "partial outer frame left in the buffer"
        );
    }

    #[test]
    fn stream_partial_outer_frame_waits_for_more_data() {
        let mut buf = stream(&[b"a\n"]);
        buf.put_slice(&[6, 0, 0, 0, b'b']);
        assert_eq!(
            drain_all(nested(false), &mut buf, false, false),
            [Got::Completed(1), message(b"a")]
        );
        assert_eq!(buf.chunk(), [6, 0, 0, 0, b'b']);

        assert_eq!(
            drain_all(nested(false), &mut buf, true, false),
            [Got::Error(FramingError::PartialFrame {
                needed: 10,
                remaining: 5
            })],
            "a partial outer frame at EOF is an error"
        );
        assert_eq!(buf.chunk(), [6, 0, 0, 0, b'b']);
    }

    #[test]
    fn stream_outer_frame_size_limit() {
        // The largest outer frame that fits the buffer is accepted.
        let mut payload = vec![b'a'; CAPACITY - 5];
        payload.push(b'\n');
        let mut buf = stream(&[&payload]);
        let got = drain_all(nested(false), &mut buf, false, false);
        assert_eq!(got, [Got::Completed(1), message(&payload[..payload.len() - 1])]);

        // One byte more can never fit, so it's invalid, and it's reported on every drain.
        let mut buf = stream(&[b"a\n"]);
        buf.put_u32_le((CAPACITY - 3) as u32);
        let oversized = Got::Error(FramingError::InvalidFrame {
            frame_len: CAPACITY - 3,
            reason: "frame length exceeds buffer capacity",
        });
        assert_eq!(
            drain_all(nested(false), &mut buf, false, false),
            [Got::Completed(1), message(b"a"), oversized]
        );
        for _ in 0..2 {
            let oversized = Got::Error(FramingError::InvalidFrame {
                frame_len: CAPACITY - 3,
                reason: "frame length exceeds buffer capacity",
            });
            assert_eq!(drain_all(nested(false), &mut buf, false, false), [oversized]);
            assert_eq!(buf.remaining(), 4, "the oversized length prefix stays in the buffer");
        }
    }

    #[test]
    fn stream_partial_length_prefix_waits_for_more_data() {
        let mut buf = bytes(&[1, 0, 0]);
        assert_eq!(drain_all(nested(false), &mut buf, false, false), []);
        assert_eq!(buf.chunk(), [1, 0, 0]);

        assert_eq!(
            drain_all(nested(false), &mut buf, true, false),
            [Got::Error(FramingError::PartialFrame {
                needed: 4,
                remaining: 3
            })]
        );
        assert_eq!(buf.chunk(), [1, 0, 0]);
    }

    #[test]
    fn stream_error_drops_rest_of_outer_frame() {
        let mut buf = stream(&[b"aa\nbb", b"cc\n"]);
        assert_eq!(
            drain_all(nested(true), &mut buf, false, false),
            [message(b"aa"), missing_newline(2)]
        );
        assert_eq!(
            drain_all(nested(true), &mut buf, false, false),
            [Got::Completed(1), message(b"cc")],
            "a new drain continues at the next outer frame"
        );
    }

    #[test]
    fn non_stream_partial_line_waits_for_more_data() {
        let mut buf = bytes(b"a:1|c\nb:1|c\nc:1");
        assert_eq!(
            drain_all(direct(false), &mut buf, false, false),
            [message(b"a:1|c"), message(b"b:1|c")]
        );
        assert_eq!(buf.chunk(), b"c:1", "the partial line stays in the buffer");
    }

    #[test]
    fn non_stream_error_consumes_nothing() {
        let mut buf = bytes(b"a:1|c");
        assert_eq!(drain_all(direct(true), &mut buf, true, false), [missing_newline(5)]);
        assert_eq!(buf.chunk(), b"a:1|c");
    }

    #[test]
    fn non_stream_keeps_carriage_returns() {
        let mut buf = bytes(b"a:1|c\r\nb:1|c\r\n");
        assert_eq!(
            drain_all(direct(false), &mut buf, false, false),
            [message(b"a:1|c\r"), message(b"b:1|c\r")]
        );
    }

    #[test]
    fn non_stream_empty_lines_are_empty_messages() {
        let mut buf = bytes(b"\na:1|c\n\n");
        assert_eq!(
            drain_all(direct(false), &mut buf, true, false),
            [message(b""), message(b"a:1|c"), message(b"")]
        );
    }

    #[test]
    fn listener_framing_modes() {
        let local = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8125));
        let cases = [
            (ListenAddress::Tcp(local), Mode::Stream),
            (ListenAddress::Unix("/tmp/dsd-stream.sock".into()), Mode::Stream),
            (ListenAddress::Udp(local), Mode::NonStream),
            (ListenAddress::Unixgram("/tmp/dsd.sock".into()), Mode::NonStream),
            (
                ListenAddress::named_pipe("datadog-dogstatsd", "D:AI(A;;GA;;;WD)"),
                Mode::NonStream,
            ),
        ];
        for (listen_address, mode) in cases {
            assert_eq!(get_framer(&listen_address, false).mode, mode, "{listen_address:?}");
        }
    }

    fn udp_address() -> ListenAddress {
        ListenAddress::Udp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8125)))
    }

    #[test]
    fn udp_missing_newline_is_accepted_by_default() {
        let mut buf = bytes(b"test.metric:1|c");
        assert_eq!(
            drain_all(get_framer(&udp_address(), false), &mut buf, true, false),
            [message(b"test.metric:1|c")]
        );
        assert!(buf.chunk().is_empty());
    }

    #[test]
    fn udp_missing_newline_is_rejected_when_required() {
        let mut buf = bytes(b"test.metric:1|c");
        assert_eq!(
            drain_all(get_framer(&udp_address(), true), &mut buf, true, false),
            [missing_newline(15)]
        );
    }

    #[cfg(unix)]
    #[test]
    fn uds_stream_missing_newline_is_rejected_when_required() {
        let mut buf = stream(&[b"test.metric:1|c"]);
        let framer = get_framer(&ListenAddress::Unix("/tmp/dsd-stream.sock".into()), true);
        assert_eq!(drain_all(framer, &mut buf, true, false), [missing_newline(15)]);
        assert!(buf.chunk().is_empty(), "the outer frame was taken whole");
    }

    #[test]
    fn named_pipe_uses_raw_newline_framing() {
        let mut buf = bytes(b"test.metric:1|c\n");
        let framer = get_framer(
            &ListenAddress::named_pipe("datadog-dogstatsd", "D:AI(A;;GA;;;WD)"),
            true,
        );
        assert_eq!(drain_all(framer, &mut buf, false, false), [message(b"test.metric:1|c")]);
        assert!(buf.chunk().is_empty());
    }

    #[test]
    fn named_pipe_waits_for_newline_before_eof() {
        let mut buf = bytes(b"test.metric:1|c");
        let framer = get_framer(
            &ListenAddress::named_pipe("datadog-dogstatsd", "D:AI(A;;GA;;;WD)"),
            true,
        );
        assert_eq!(drain_all(framer, &mut buf, false, false), []);
        assert_eq!(buf.chunk(), b"test.metric:1|c");
    }
}
