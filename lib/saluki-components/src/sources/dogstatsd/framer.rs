use bytes::Bytes;
use saluki_io::{
    buf::ReadIoBuffer,
    deser::framing::{Framer, FramingError, LengthDelimitedFramer, NestedFramer, NewlineFramer},
    net::ListenAddress,
};

pub enum DsdFramer {
    NonStream(NewlineFramer),
    Stream(NestedFramer<NewlineFramer, LengthDelimitedFramer>),
}

impl Framer for DsdFramer {
    fn next_frame<B: ReadIoBuffer>(&mut self, buf: &mut B, is_eof: bool) -> Result<Option<Bytes>, FramingError> {
        match self {
            Self::NonStream(framer) => framer.next_frame(buf, is_eof),
            Self::Stream(framer) => framer.next_frame(buf, is_eof),
        }
    }
}

pub fn get_framer(listen_address: &ListenAddress, eol_required: bool) -> DsdFramer {
    let newline_framer = NewlineFramer::default().required_on_eof(eol_required);

    match listen_address {
        ListenAddress::Tcp(_) => DsdFramer::Stream(NestedFramer::new(newline_framer, LengthDelimitedFramer)),
        ListenAddress::Udp(_) => DsdFramer::NonStream(newline_framer),
        #[cfg(unix)]
        ListenAddress::Unixgram(_) => DsdFramer::NonStream(newline_framer),
        #[cfg(unix)]
        ListenAddress::Unix(_) => DsdFramer::Stream(NestedFramer::new(newline_framer, LengthDelimitedFramer)),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    };

    use saluki_io::{
        deser::framing::{Framer, FramingError},
        net::ListenAddress,
    };

    use super::get_framer;

    fn udp_address() -> ListenAddress {
        ListenAddress::Udp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8125)))
    }

    fn missing_delimiter_err(len: usize) -> FramingError {
        FramingError::InvalidFrame {
            frame_len: len,
            reason: "reached EOF (end of UDP frame) without finding newline delimiter, which is required by dogstatsd_eol_required.",
        }
    }

    #[test]
    fn udp_missing_newline_is_accepted_by_default() {
        let payload = b"test.metric:1|c";
        let mut buf = VecDeque::from(payload.to_vec());
        let mut framer = get_framer(&udp_address(), false);

        let frame = framer
            .next_frame(&mut buf, true)
            .expect("framing should not fail")
            .expect("frame should be available at EOF");

        assert_eq!(&frame[..], payload);
    }

    #[test]
    fn udp_missing_newline_is_rejected_when_required() {
        let payload = b"test.metric:1|c";
        let mut buf = VecDeque::from(payload.to_vec());
        let mut framer = get_framer(&udp_address(), true);

        assert_eq!(
            framer.next_frame(&mut buf, true),
            Err(missing_delimiter_err(payload.len()))
        );
    }

    #[cfg(unix)]
    #[test]
    fn uds_stream_missing_newline_is_rejected_when_required() {
        let payload = b"test.metric:1|c";
        let mut buf = VecDeque::new();
        buf.extend((payload.len() as u32).to_le_bytes());
        buf.extend(payload);
        let mut framer = get_framer(&ListenAddress::Unix("/tmp/dsd-stream.sock".into()), true);

        assert_eq!(
            framer.next_frame(&mut buf, true),
            Err(missing_delimiter_err(payload.len()))
        );
    }
}
