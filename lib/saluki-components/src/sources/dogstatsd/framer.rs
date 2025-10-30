use saluki_io::{
    buf::ReadIoBuffer,
    deser::framing::{Framed, LengthDelimitedFramer, NewlineFramer},
    net::ListenAddress,
};

pub enum DsdFramer {
    NonStream(NewlineFramer),
    Stream(LengthDelimitedFramer, NewlineFramer),
}

impl DsdFramer {
    pub fn framed<'buf, B>(&self, buf: &'buf mut B, is_eof: bool) -> Framed<'buf, '_, B>
    where
        B: ReadIoBuffer,
    {
        match self {
            Self::NonStream(framer) => Framed::direct(framer, buf, is_eof),
            Self::Stream(outer, inner) => Framed::nested(outer, inner, buf, is_eof),
        }
    }
}

pub fn get_framer(listen_address: &ListenAddress, max_frame_size: usize) -> DsdFramer {
    let length_delimited_framer = LengthDelimitedFramer::default().with_max_frame_size(max_frame_size);
    let newline_framer = NewlineFramer::default().required_on_eof(false);

    match listen_address {
        ListenAddress::Tcp(_) => DsdFramer::Stream(length_delimited_framer, newline_framer),
        ListenAddress::Udp(_) => DsdFramer::NonStream(newline_framer),
        #[cfg(unix)]
        ListenAddress::Unixgram(_) => DsdFramer::NonStream(newline_framer),
        #[cfg(unix)]
        ListenAddress::Unix(_) => DsdFramer::Stream(length_delimited_framer, newline_framer),
    }
}
