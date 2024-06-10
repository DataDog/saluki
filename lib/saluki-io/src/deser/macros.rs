#[macro_export]
macro_rules! multi_framing {
	(name => $name:ident, codec => $codec:ty, { $($variant:ident => $framer_ty:ty,)+ }) => {
		paste::paste! {
			pub enum [<$name MultiFramer>] {
				$($variant($framer_ty)),+
			}

			#[derive(Debug)]
			pub enum [<$name MultiFraming>] {
				$($variant(<$framer_ty as $crate::deser::framing::Framer<$codec>>::Output)),+
			}

			impl $crate::deser::Decoder for [<$name MultiFraming>] {
				type Error = $crate::deser::framing::FramingError<$codec>;

				fn decode<B: $crate::buf::ReadIoBuffer>(&mut self, buf: &mut B, events: &mut saluki_core::topology::interconnect::EventBuffer) -> Result<usize, Self::Error> {
					match self {
						$(Self::$variant(inner) => inner.decode(buf, events)),+
					}
				}

				fn decode_eof<B: $crate::buf::ReadIoBuffer>(&mut self, buf: &mut B, events: &mut saluki_core::topology::interconnect::EventBuffer) -> Result<usize, Self::Error> {
					match self {
						$(Self::$variant(inner) => inner.decode_eof(buf, events)),+
					}
				}
			}

			impl $crate::deser::framing::Framer<$codec> for [<$name MultiFramer>] {
				type Output = [<$name MultiFraming>];

				fn with_decoder(self, decoder: $codec) -> Self::Output {
					match self {
						$(Self::$variant(framer) => [<$name MultiFraming>]::$variant(framer.with_decoder(decoder))),+
					}
				}
			}
		}
	}
}

pub use multi_framing;
