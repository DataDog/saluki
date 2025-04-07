use std::future::Future;

use saluki_io::{
    buf::BytesBufferView,
    deser::framing::{Framer, FramingError, NewlineFramer},
    net::ListenAddress,
};

pub enum DsdFramer {
    NonStream(NewlineFramer),
    Stream(NestedFramer<NewlineFramer, LengthDelimitedFramer>),
}

impl Framer for DsdFramer {
     fn extract_frames<'buf, B>(
        &mut self,
        buf: &'buf mut B,
        is_eof: bool,
        frames: Remit<'_, Result<Option<BytesBufferView<'buf>>, FramingError>>,
    ) -> impl Future<Output = ()>
    where
        B: BufferView
    {
        async move {
            match self {
                Self::NonStream(framer) => framer.extract_frames(buf, is_eof, frames).await,
                Self::Stream(framer) => framer.extract_frames(buf, is_eof, frames).await,
            }
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
        ListenAddress::Unix(_) => DsdFramer::Stream(NestedFramer::new(newline_framer, LengthDelimitedFramer)),
    }
}
