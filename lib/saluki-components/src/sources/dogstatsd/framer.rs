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

pub fn get_framer(listen_address: &ListenAddress) -> DsdFramer {
    let newline_framer = NewlineFramer::default().required_on_eof(false);

    match listen_address {
        ListenAddress::Tcp(_) => DsdFramer::Stream(NestedFramer::new(newline_framer, LengthDelimitedFramer)),
        ListenAddress::Udp(_) => DsdFramer::NonStream(newline_framer),
        #[cfg(unix)]
        ListenAddress::Unixgram(_) => DsdFramer::NonStream(newline_framer),
        #[cfg(unix)]
        ListenAddress::Unix(_) => DsdFramer::Stream(NestedFramer::new(newline_framer, LengthDelimitedFramer)),
    }
}
