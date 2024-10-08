// This code is taken from linkerd2-proxy, based on the following commit --
// https://github.com/linkerd/linkerd2-proxy/blob/c0a0114a6dfce898f5b6dd71b17f0a37b2bd2da2/linkerd/http/retry/src/replay.rs
// -- and is licensed under the Apache License, version 2.0.
//
// Includes slight modifications to fit within Saluki.

use std::{collections::VecDeque, io::IoSlice, pin::Pin, sync::Arc, task::Context, task::Poll};
use std::{sync::Mutex, task::ready};

use bytes::{Buf, BufMut, Bytes, BytesMut};
use http_body::Frame;
use http_body::{Body, SizeHint};
use tracing::trace;

/// Wraps an HTTP body type and lazily buffers data as it is read from the inner body.
///
/// When this body is dropped, if a clone exists, any buffered data is shared with its cloned. The first clone to be
/// polled will take ownership over the data until it is dropped. When *that* clone is dropped, the buffered data ---
/// including any new data read from the body by the clone, if the body has not yet completed --- will be shared with
/// any remaining clones.
///
/// The buffered data can then be used to retry the request if the original request fails.
#[derive(Debug)]
pub struct ReplayBody<B> {
    /// Buffered state owned by this body if it is actively being polled. If this body has been polled and no other body
    /// owned the state, this will be `Some`.
    state: Option<BodyState<B>>,

    /// Copy of the state shared across all clones. When the active clone is dropped, it moves its state back into the
    /// shared state to be taken by the next clone to be polled.
    shared: Arc<SharedState<B>>,

    /// Should this clone replay the buffered body from the shared state before polling the initial body?
    replay_body: bool,
}

/// Data returned by `ReplayBody`'s `http_body::Body` implementation is either `Bytes` returned by the initial body, or
/// a list of all `Bytes` chunks returned by the initial body (when replaying it).
#[derive(Debug)]
pub enum Data {
    Initial(Bytes),
    Replay(BufList),
}

/// Body data composed of multiple `Bytes` chunks.
#[derive(Clone, Debug, Default)]
pub struct BufList {
    bufs: VecDeque<Bytes>,
}

#[derive(Debug)]
struct SharedState<B> {
    body: Mutex<Option<BodyState<B>>>,
    /// Did the initial body return `true` from `is_end_stream` before it was ever polled? If so, always return `true`;
    /// the body is completely empty.
    ///
    /// We store this separately so that clones of a totally empty body can always return `true` from `is_end_stream`
    /// even when they don't own the shared state.
    was_empty: bool,

    orig_size_hint: SizeHint,
}

#[derive(Debug)]
struct BodyState<B> {
    buf: BufList,
    rest: Option<B>,
    is_completed: bool,

    /// Maximum number of bytes to buffer.
    max_bytes: usize,
}

impl<B: Body> ReplayBody<B> {
    /// Wraps an initial `Body` in a `ReplayBody`.
    ///
    /// In order to prevent unbounded buffering, this takes a maximum number of bytes to buffer as a second parameter.
    /// If more than than that number of bytes would be buffered, the buffered data is discarded and any subsequent
    /// clones of this body will fail. However, the *currently active* clone of the body is allowed to continue without
    /// erroring. It will simply stop buffering any additional data for retries.
    ///
    /// If the body has a size hint with a lower bound greater than `max_bytes`, the original body is returned in the
    /// error variant.
    pub fn try_new(body: B, max_bytes: usize) -> Result<Self, B> {
        let orig_size_hint = body.size_hint();
        trace!(body.size_hint = %orig_size_hint.lower(), %max_bytes);
        if orig_size_hint.lower() > max_bytes as u64 {
            return Err(body);
        }

        Ok(Self {
            shared: Arc::new(SharedState {
                body: Mutex::new(None),
                orig_size_hint,
                was_empty: body.is_end_stream(),
            }),
            state: Some(BodyState {
                buf: Default::default(),
                rest: Some(body),
                is_completed: false,
                max_bytes: max_bytes + 1,
            }),
            // The initial `ReplayBody` has nothing to replay
            replay_body: false,
        })
    }

    /// Mutably borrows the body state if this clone currently owns it, or else tries to acquire it from the shared
    /// state.
    ///
    /// # Panics
    ///
    /// This panics if another clone has currently acquired the state, based on the assumption that a retry body will
    /// not be polled until the previous request has been dropped.
    fn acquire_state<'a>(
        state: &'a mut Option<BodyState<B>>, shared: &Mutex<Option<BodyState<B>>>,
    ) -> &'a mut BodyState<B> {
        state.get_or_insert_with(|| {
            shared
                .lock()
                .expect("another clone has acquired the state")
                .take()
                .expect("missing body state")
        })
    }
}

impl<B> Body for ReplayBody<B>
where
    B: Body + Unpin,
{
    type Data = Data;
    type Error = B::Error;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        let state = Self::acquire_state(&mut this.state, &this.shared.body);
        // Move these out to avoid mutable borrow issues in the `map` closure when polling the inner body.
        trace!(
            replay_body = this.replay_body,
            buf.has_remaining = state.buf.has_remaining(),
            body.is_completed = state.is_completed,
            body.max_bytes_remaining = state.max_bytes,
            "ReplayBody::poll_data"
        );

        // If we haven't replayed the buffer yet, and its not empty, return the buffered data first.
        if this.replay_body && state.buf.has_remaining() {
            trace!("Replaying body");

            // Don't return the buffered data again on the next poll.
            this.replay_body = false;
            return Poll::Ready(Some(Ok(Frame::data(Data::Replay(state.buf.clone())))));
        }

        // If the inner body has previously ended, don't poll it again.
        //
        // NOTE(eliza): we would expect the inner body to just happily return `None` multiple times here, but
        // `hyper::Body::channel` (which we use in the tests) will panic if it is polled after returning `None`, so we
        // have to special-case this. :/
        if state.is_completed {
            return Poll::Ready(None);
        }

        // Poll the inner body for more data. If the body has ended, remember that so that future clones will not try
        // polling it again (as described above).
        let data: http_body::Frame<<B as Body>::Data> = {
            // Get access to the initial body. If we don't have access to the inner body, there's no more work to do.
            let rest = match state.rest.as_mut() {
                Some(rest) => rest,
                None => return Poll::Ready(None),
            };

            trace!("Polling initial body");
            match ready!(Pin::new(rest).poll_frame(cx)) {
                Some(Ok(data)) => data,
                Some(Err(e)) => return Poll::Ready(Some(Err(e))),
                None => {
                    trace!("Initial body completed");
                    state.is_completed = true;
                    return Poll::Ready(None);
                }
            }
        };

        match data.into_data() {
            Ok(data) => {
                let length = data.remaining();
                state.max_bytes = state.max_bytes.saturating_sub(length);
                let chunk = state.buf.push_chunk(data);
                Poll::Ready(Some(Ok(Frame::data(Data::Initial(chunk)))))
            }
            _ => panic!("Expected frame data but received trailers."),
        }

        // If we have buffered the maximum number of bytes, allow *this* body to continue, but don't buffer any more.
    }

    fn is_end_stream(&self) -> bool {
        // if the initial body was EOS as soon as it was wrapped, then we are empty.
        if self.shared.was_empty {
            return true;
        }

        let is_inner_eos = self
            .state
            .as_ref()
            .and_then(|state| state.rest.as_ref().map(Body::is_end_stream))
            .unwrap_or(false);

        // if this body has data remaining to play back, it is not EOS
        !self.replay_body
            // if we have replayed everything, the initial body may still have data remaining, so ask it
            && is_inner_eos
    }

    #[inline]
    fn size_hint(&self) -> SizeHint {
        // If this clone isn't holding the body, return the original size hint.
        let state = match self.state.as_ref() {
            Some(state) => state,
            None => return self.shared.orig_size_hint.clone(),
        };

        // Otherwise, if we're holding the state but have dropped the inner body, the entire body is buffered so we know
        // the exact size hint.
        let buffered = state.buf.remaining() as u64;
        let rest_hint = match state.rest.as_ref() {
            Some(rest) => rest.size_hint(),
            None => return SizeHint::with_exact(buffered),
        };

        // Otherwise, add the inner body's size hint to the amount of buffered data. An upper limit is only set if the
        // inner body has an upper limit.
        let mut hint = SizeHint::default();
        hint.set_lower(buffered + rest_hint.lower());
        if let Some(rest_upper) = rest_hint.upper() {
            hint.set_upper(buffered + rest_upper);
        }
        hint
    }
}

impl<B> Clone for ReplayBody<B> {
    fn clone(&self) -> Self {
        Self {
            state: None,
            shared: self.shared.clone(),
            // The clone should try to replay from the shared state before reading any additional data from the initial
            // body.
            replay_body: true,
        }
    }
}

impl<B> Drop for ReplayBody<B> {
    fn drop(&mut self) {
        // If this clone owned the shared state, put it back.
        if let Some(state) = self.state.take() {
            let mut guard = self.shared.body.lock().expect("Mutex is poisoned");
            *guard = Some(state);
        }
    }
}

impl Buf for Data {
    #[inline]
    fn remaining(&self) -> usize {
        match self {
            Data::Initial(buf) => buf.remaining(),
            Data::Replay(bufs) => bufs.remaining(),
        }
    }

    #[inline]
    fn chunk(&self) -> &[u8] {
        match self {
            Data::Initial(buf) => buf.chunk(),
            Data::Replay(bufs) => bufs.chunk(),
        }
    }

    #[inline]
    fn chunks_vectored<'iovs>(&'iovs self, iovs: &mut [IoSlice<'iovs>]) -> usize {
        match self {
            Data::Initial(buf) => buf.chunks_vectored(iovs),
            Data::Replay(bufs) => bufs.chunks_vectored(iovs),
        }
    }

    #[inline]
    fn advance(&mut self, amt: usize) {
        match self {
            Data::Initial(buf) => buf.advance(amt),
            Data::Replay(bufs) => bufs.advance(amt),
        }
    }

    #[inline]
    fn copy_to_bytes(&mut self, len: usize) -> Bytes {
        match self {
            Data::Initial(buf) => buf.copy_to_bytes(len),
            Data::Replay(bufs) => bufs.copy_to_bytes(len),
        }
    }
}

impl BufList {
    fn push_chunk(&mut self, mut data: impl Buf) -> Bytes {
        let len = data.remaining();
        // `data` is (almost) certainly a `Bytes`, so `copy_to_bytes` should internally be a cheap refcount bump almost
        // all of the time.  But, if it isn't, this will copy it to a `Bytes` that we can now clone.
        let bytes = data.copy_to_bytes(len);
        // Buffer a clone of the bytes read on this poll.
        self.bufs.push_back(bytes.clone());
        // Return the bytes
        bytes
    }
}

impl Buf for BufList {
    fn remaining(&self) -> usize {
        self.bufs.iter().map(Buf::remaining).sum()
    }

    fn chunk(&self) -> &[u8] {
        self.bufs.front().map(Buf::chunk).unwrap_or(&[])
    }

    fn chunks_vectored<'iovs>(&'iovs self, iovs: &mut [IoSlice<'iovs>]) -> usize {
        // Are there more than zero iovecs to write to?
        if iovs.is_empty() {
            return 0;
        }

        // Loop over the buffers in the replay buffer list, and try to fill as
        // many iovecs as we can from each buffer.
        let mut filled = 0;
        for buf in &self.bufs {
            filled += buf.chunks_vectored(&mut iovs[filled..]);
            if filled == iovs.len() {
                return filled;
            }
        }

        filled
    }

    fn advance(&mut self, mut amt: usize) {
        while amt > 0 {
            let rem = self.bufs[0].remaining();
            // If the amount to advance by is less than the first buffer in the buffer list, advance that buffer's
            // cursor by `amt`, and we're done.
            if rem > amt {
                self.bufs[0].advance(amt);
                return;
            }

            // Otherwise, advance the first buffer to its end, and continue.
            self.bufs[0].advance(rem);
            amt -= rem;

            self.bufs.pop_front();
        }
    }

    fn copy_to_bytes(&mut self, len: usize) -> Bytes {
        // If the length of the requested `Bytes` is <= the length of the front buffer, we can just use its
        // `copy_to_bytes` implementation (which is just a reference count bump).
        match self.bufs.front_mut() {
            Some(first) if len <= first.remaining() => {
                let buf = first.copy_to_bytes(len);
                // If we consumed the first buffer, also advance our "cursor" by popping it.
                if first.remaining() == 0 {
                    self.bufs.pop_front();
                }

                buf
            }
            _ => {
                assert!(len <= self.remaining(), "`len` greater than remaining");
                let mut buf = BytesMut::with_capacity(len);
                buf.put(self.take(len));
                buf.freeze()
            }
        }
    }
}
