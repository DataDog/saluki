use saluki_io::{
    deser::{
        codec::DogstatsdCodec,
        framing::{LengthDelimitedFramer, NewlineFramer},
    },
    multi_framing,
    net::ListenAddress,
};

use super::interceptor::AgentLikeTagMetadataInterceptor;

multi_framing!(name => DogStatsD, codec => DogstatsdCodec<AgentLikeTagMetadataInterceptor>, {
    Newline => NewlineFramer,
    LengthDelimited => LengthDelimitedFramer,
});

pub fn get_framer(listen_address: &ListenAddress) -> DogStatsDMultiFramer {
    match listen_address {
        ListenAddress::Tcp(_) => unreachable!("TCP mode not support for Dogstatsd source"),
        ListenAddress::Udp(_) => DogStatsDMultiFramer::Newline(NewlineFramer::default().required_on_eof(false)),
        #[cfg(unix)]
        ListenAddress::Unixgram(_) => DogStatsDMultiFramer::Newline(NewlineFramer::default().required_on_eof(false)),
        #[cfg(unix)]
        ListenAddress::Unix(_) => DogStatsDMultiFramer::LengthDelimited(LengthDelimitedFramer),
    }
}
