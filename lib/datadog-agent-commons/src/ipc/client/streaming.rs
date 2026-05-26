use std::{
    future::Future,
    pin::Pin,
    task::{ready, Context, Poll},
};

use futures::Stream;
use pin_project_lite::pin_project;
use tonic::{Response, Status, Streaming};

pin_project! {
    #[project = InnerProj]
    enum Inner<T> {
        /// Waiting for the server to stream the first message.
        Initial { fut: Pin<Box<dyn Future<Output = Result<Response<Streaming<T>>, Status>> + Send>> },

        /// Waiting for the server to stream the next message.
        Streaming { #[pin] stream: Streaming<T> },

        /// Stream has produced an error or reached its end; further polls yield `None`.
        Terminated,
    }
}

pin_project! {
    /// A streaming gRPC response.
    ///
    /// Compared to the normal streaming response type from [`tonic`], `StreamingResponse` handles a special case where
    /// servers may not send a guaranteed initial message, which is required for `tonic` to create and return the
    /// `Streaming` object that can then be polled. This leads to an issue where calls can effectively appear to block
    /// until the first message is sent by the server, which is suboptimal.
    ///
    /// `StreamingResponse` exposes a unified [`Stream`] implementation that encompasses both the initial RPC
    /// establishment and subsequent messages sent by the server, to allow for a more seamless experience when working
    /// with streaming RPCs.
    pub struct StreamingResponse<T> {
        #[pin]
        inner: Inner<T>,
    }
}

impl<T> StreamingResponse<T> {
    pub(super) fn from_response_future<F>(fut: F) -> Self
    where
        F: Future<Output = Result<Response<Streaming<T>>, Status>> + Send + 'static,
    {
        Self {
            inner: Inner::Initial { fut: Box::pin(fut) },
        }
    }
}

impl<T> Stream for StreamingResponse<T> {
    type Item = Result<T, Status>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // Each arm picks one of three outcomes: advance to a new state and loop, yield an item
        // leaving state untouched, or fuse to `Terminated` while yielding an item. Fusing ensures
        // no now-finished resource (notably the `Initial` future) is ever polled again.
        #[allow(clippy::large_enum_variant)]
        enum Outcome<T> {
            Advance(Streaming<T>),
            Yield(Option<Result<T, Status>>),
            Terminate(Option<Result<T, Status>>),
        }

        loop {
            let mut this = self.as_mut().project();
            let outcome = match this.inner.as_mut().project() {
                InnerProj::Initial { fut } => match ready!(fut.as_mut().poll(cx)) {
                    Ok(response) => Outcome::Advance(response.into_inner()),
                    Err(status) => Outcome::Terminate(Some(Err(status))),
                },
                InnerProj::Streaming { stream } => match ready!(stream.poll_next(cx)) {
                    Some(maybe_item) => Outcome::Yield(Some(maybe_item)),
                    None => Outcome::Terminate(None),
                },
                InnerProj::Terminated => Outcome::Yield(None),
            };

            match outcome {
                Outcome::Advance(stream) => {
                    this.inner.set(Inner::Streaming { stream });
                }
                Outcome::Yield(item) => return Poll::Ready(item),
                Outcome::Terminate(item) => {
                    this.inner.set(Inner::Terminated);
                    return Poll::Ready(item);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures::{future::pending, StreamExt};
    use tokio::time::timeout;
    use tonic::{Code, Status};

    use super::StreamingResponse;

    #[tokio::test]
    async fn streaming_response_terminates_after_initial_error() {
        // Regression test: prior to fusing the `Initial` state on error, the second poll re-entered
        // the already-completed async block and panicked with "async fn resumed after completion".
        let mut stream = StreamingResponse::<()>::from_response_future(async { Err(Status::unavailable("boom")) });

        match stream.next().await {
            Some(Err(s)) => assert_eq!(s.code(), Code::Unavailable),
            other => panic!(
                "expected Some(Err(Unavailable)), got {:?}",
                other.map(|r| r.map(|_| ()))
            ),
        }

        // Subsequent polls must yield `None` and must not panic.
        assert!(stream.next().await.is_none());
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn streaming_response_pending_initial_stays_pending() {
        // Smoke test: a pending inner future leaves `poll_next` Pending without advancing state.
        let mut stream = StreamingResponse::<()>::from_response_future(async { pending::<Result<_, Status>>().await });

        assert!(
            timeout(Duration::from_millis(50), stream.next()).await.is_err(),
            "stream with pending initial future should not produce an item"
        );
    }
}
