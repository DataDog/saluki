use saluki_io::{
    buf::BytesBufferView,
    deser::framing::{Framer, FramingError, NewlineFramer},
    net::ListenAddress,
};

pub enum DsdFramer {
    NonStream(NewlineFramer),
    //Stream(NestedFramer<NewlineFramer, LengthDelimitedFramer>),
    Stream(NewlineFramer),
}

impl Framer for DsdFramer {
    type Frame<'a>
        = BytesBufferView<'a>
    where
        Self: 'a;

    fn next_frame<'a, 'buf>(
        &'a mut self, buf: &'a mut BytesBufferView<'buf>, is_eof: bool,
    ) -> Result<Option<Self::Frame<'a>>, FramingError>
    where
        'buf: 'a,
    {
        match self {
            Self::NonStream(framer) => framer.next_frame(buf, is_eof),
            Self::Stream(framer) => framer.next_frame(buf, is_eof),
        }
    }
}

pub fn get_framer(listen_address: &ListenAddress) -> DsdFramer {
    let newline_framer = NewlineFramer::default().required_on_eof(false);

    match listen_address {
        ListenAddress::Tcp(_) => unreachable!("TCP mode not support for DogStatsD source"),
        ListenAddress::Udp(_) => DsdFramer::NonStream(newline_framer),
        #[cfg(unix)]
        ListenAddress::Unixgram(_) => DsdFramer::NonStream(newline_framer),
        #[cfg(unix)]
        ListenAddress::Unix(_) => DsdFramer::Stream(newline_framer),
        //ListenAddress::Unix(_) => DsdFramer::Stream(NestedFramer::new(newline_framer, LengthDelimitedFramer)),
    }
}
