use std::{
    convert::Infallible,
    pin::Pin,
    task::{Context, Poll},
};

use bytes::{Buf, Bytes};
use http_body::{Body, Frame};

use crate::buf::ReadIoBuffer;

/// A fixed-sized HTTP body based on a single input buffer.
#[derive(Clone)]
pub struct FixedBody {
    data: Option<Bytes>,
}

impl FixedBody {
    /// Create a new `FixedBody` from the given data.
    pub fn new<D: Into<Bytes>>(data: D) -> Self {
        Self {
            data: Some(data.into()),
        }
    }
}

impl Buf for FixedBody {
    fn remaining(&self) -> usize {
        self.data.as_ref().map_or(0, |data| data.remaining())
    }

    fn chunk(&self) -> &[u8] {
        self.data.as_ref().map_or(&[], |data| data.chunk())
    }

    fn advance(&mut self, cnt: usize) {
        match self.data.as_mut() {
            Some(data) => data.advance(cnt),
            None => panic!("attempted to advance a consumed body"),
        }
    }
}

impl ReadIoBuffer for FixedBody {
    fn capacity(&self) -> usize {
        self.data.as_ref().map_or(0, |data| data.capacity())
    }
}

impl Body for FixedBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        Poll::Ready(self.get_mut().data.take().map(|data| Ok(Frame::data(data))))
    }
}
