use saluki_io::{
    deser::framing::{Framer, FramingError, LengthDelimitedFramer, NewlineFramer},
    net::ListenAddress,
};

pub enum DsdFramer {
    Newline(NewlineFramer),
    LengthDelimited(LengthDelimitedFramer),
}

impl Framer for DsdFramer {
    fn next_frame<'a, B: saluki_io::buf::ReadIoBuffer>(
        &mut self, buf: &'a B, is_eof: bool,
    ) -> Result<Option<(&'a [u8], usize)>, FramingError> {
        match self {
            DsdFramer::Newline(inner) => inner.next_frame(buf, is_eof),
            DsdFramer::LengthDelimited(inner) => inner.next_frame(buf, is_eof),
        }
    }
}

pub fn get_framer(listen_address: &ListenAddress) -> DsdFramer {
    match listen_address {
        ListenAddress::Tcp(_) => unreachable!("TCP mode not support for Dogstatsd source"),
        ListenAddress::Udp(_) => DsdFramer::Newline(NewlineFramer::default().required_on_eof(false)),
        #[cfg(unix)]
        ListenAddress::Unixgram(_) => DsdFramer::Newline(NewlineFramer::default().required_on_eof(false)),
        #[cfg(unix)]
        ListenAddress::Unix(_) => DsdFramer::LengthDelimited(LengthDelimitedFramer::default()),
    }
}
