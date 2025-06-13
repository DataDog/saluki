# RFC 001: An efficient and misuse-resistant replacement for `Bytes` in `Framer`

## Problem

The design of `Framer` centers around being given a buffer that can be incrementally consumed/advanced as frames are
decoded from it.

In order to provide an ergonomic approach for consuming the input buffer, `Framer::next_frame` is generic over the input
buffer type, so long as it implements `bytes::Buf`. Advancing the buffer, however, requires mutable access, which
precludes us from returning an immutable borrow over the slice representing our frame.

We get around this by calling `Buf::copy_to_bytes` to give us an owned value that can be returned, and since our input
buffer is never `Bytes` in the first place, there is no special optimization under the hood: it allocates a new
`Vec<u8>` and copies the bytes into it.

We need to be able to have a framer authoritatively advance the input buffer when a frame is decoded, and also be able
to avoid allocating in order to return the frame to the caller.

## Why don't we use `Bytes` up front?

Using `Bytes` as the input buffer type would, in practice, allow us to get zero-cost slicing in framer implementations
with no additional effort. However, there are things that make trying to do this unergonomic and/or risky in the context
of our goal of only utilizing fixed-size buffers.

`Bytes` is immutable, and so to even have a `Bytes` to give to a framer implies that you started out with a `BytesMut`.
Since we want to pool these buffers, we need to pool them in their mutable form -- `BytesMut` -- so that they can
actually be used for socket reads. 

### Converting between `Bytes`/`BytesMut` and the risk of leaks

`Bytes` and `BytesMut` have corresponding methods for converting between the two types, but notably, the translation
from `Bytes` to `BytesMut` is fallible because other live instances of `Bytes` may point to the same underlying
`BytesMut`. Again, this follows the underlying design of `Bytes` looking almost identical to `Arc<Vec<u8>>`.

What this means, however, is that without careful usage, code using `Bytes` could inadvertently keep around an instance
that makes it impossible to get back the original `BytesMut`, which could quickly lead to exhausting all buffers in a
buffer pool unless replaced, or require replenishment in the form of allocating additional buffers.

This _also_ applies to `BytesMut`, which can be sliced and have two or more outstanding instances all pointing to the
same underlying buffer (albeit with non-overlapping regions).

### Difficulty in collapsing `BytesMut`

`BytesMut` is designed to be used in a way that involves roughly knowing how much data to expect so that particular
methods, such as `BytesMut::reserve` and `BytesMut::try_reclaim`, can be used to ensure there's available capacity, and
to handle trying to reclaim any regions of the buffer that were previously shared but no longer are, and so on.

We encounter this problem ourselves already, such as when a buffer is completely filled, and has a partially-written
frame at the end: we can decode all of the complete frames, but in order to allow for the partially-written frame to be
completely written, it may require the _entire_ buffer, which means we need to shift that partial write to the front of
the buffer so the remainder can be written to.

`BytesMut` does not easily support this. At best, `BytesMut::try_reclaim` can be used to be approximate this by, for
example, always setting the `additional` value to `1` to ensure we maximize our chance of triggering the behavior. We
would be restricted to using `try_reclaim`, rather than `reserve`, to ensure our buffers stayed fixed-size. Even then,
`try_reclaim` is fallible, so it may fail and leave us in a situation where we have to clear the buffer and lose data.

## Proposed solution: `BytesBufferView<'a>`

In order to solve both the issue of being unable to return immutably borrowed buffers, as well as ensuring that buffers
cannot be kept alive the point of framing and decoding, I'm proposing a new type, `BytesBufferView<'a>`, that would
interoperate directly with `BytesBuffer`.

At a high-level, `BytesBufferView<'a>` is simply a specific slice of a `BytesBuffer` (or a `BytesBufferView<'a>`, more
on that later), taken via _mutable_ borrow which ensures that when the view drops, the parent buffer is properly
advanced.

Below is a rough sketch of what `BytesBufferView<'a>` might look like:

```rust
impl BytesBuffer {
    // Other methods relevant to _writing_ into `BytesBuffer`
    // elided by brevity.

    /// Returns a new view over this buffer.
    pub fn as_view(&mut self) -> BytesBufferView<'_>;
}

enum ViewParent<'a> {
    Buffer(&'a mut BytesBuffer),
    View(&'a mut BytesBufferView<'a>),
}

/// 
pub struct BytesBufferView<'a> {
    // Parent where this view is taken from.
    parent: ViewParent<'a>,

    // The index within the _parent_ where this view starts.
    idx: usize,
    
    // The length of this view.
    len: usize,
    
    // The deferred amount to advance the parent by.
    //
    // Also used to adjust `self.idx` when dereferencing to `&[u8]` to
    // keep the view internally/externally consistent.
    idx_advance: usize,
}

impl<'a> BytesBufferView<'a> {
    /// Returns the remaining length of this view, in bytes.
    pub fn len(&self) -> usize;
    
    /// Returns a reference to the remaining data in this view.
    pub fn as_bytes(&self) -> &[u8];

    /// Creates a new view by slicing this view from the current position
    /// to the given index, relative to the view.
    ///
    /// Advances this view by the length of the new view.
    pub fn slice_to(&mut self, idx: usize) -> Self;

    /// Creates a new view by slicing this view from the given index,
    /// relative to the view, to the end of this view.
    ///
    /// Advances this view by the length of the new view, plus whatever
    /// bytes were skipped prior to the new view.
    pub fn slice_from(&mut self, idx: usize) -> Self;
}
```

### Solving lifetime issues

Our first issue to solve is that of borrowing from an input buffer while also trying to return the frame data. We
illustrate below how `Framer` currently looks and how it would change:

```rust
// Before:
pub trait Framer {
    fn next_frame<B: ReadIoBuffer>(&mut self, buf: &mut B, is_eof: bool) -> Result<Option<Bytes>, FramingError>;
}

// After:
pub trait Framer {
    fn next_frame(&mut self, buf: &mut BytesBufferView<'_>, is_eof: bool) -> Result<Option<BytesBufferView<'_>>, FramingError>;
}
```

Firstly, while not germane to solving the lifetime issues, we would drop the generic input buffer and always take
`BytesBufferView<'_>` as our input, to provide continuity from input to output, including when framers are nested. This
would semantically be equivalent to a "frozen" buffer: once the mutating operation on the buffer is over, we operate on
the buffer in a "frozen" mode until we need to read more data, etc.

As `BytesBufferView<'_>` would only advance its parent buffer/view on drop, we need longer need to do a song-and-dance
of getting the frame data first before then advancing the buffer. This means no immutable-and-then-mutable borrow
weirdness.

### Buffer advancement through RAII

A key aspect of our problem is that we want to be able to slice up the input buffer as we extract frames and decode, and
ensure that the parent buffer reflects that: if we successfully extract a frame, that frame data should be consumed from
the origin buffer, and so on.

In order to do this _without_ needing to handle mutable/immutable constraints, we defer advancing until
`BytesBufferView<'a>` is dropped. We do so by tracking the total amount to advance internally, and then advancing by
that amount on drop.

Below is a simple example of how this might look in practice, using a mock socket receive loop:

```rust
let mut io_buf = BytesBuffer::new();

loop {
    // Read some data into our buffer from a network socket:
    do_socket_read(&mut io_buf)?;

    // Now that we have some data, we want to see if we can extract
    // a newline-delimited frame from it:
    let mut newline_framer = NewlineFramer::default();
    let mut io_buf_view = io_buf.as_view();

    match newline_framer.next_frame(&mut io_buf_view) {
        Ok(Some(frame)) => {
            // We don't need to slice-and-dice the frame any further,
            // so we just pass the byte slice to our decoder function.
            let packet = try_decode_packet(frame.as_bytes())?;
            packets_out.enqueue(packet);
        },
        Ok(None) | Err(_) => {
            // We couldn't extract a frame, so the framer never advanced
            // `io_buf_view`. When `io_buf_view` goes out of scope here
            // and drops, it has no deferred advance, and so `io_buf` is
            // effectively left as-is before we created `io_buf_view`.
            continue
        },
    }
    
    // At this point, both `frame` and `io_buf_view` go out of scope.
    //
    // We drop `frame` first, which wasn't subsequently sliced: it has
    // no deferred amount to advance its parent by, so `frame` drops
    // and `io_buf_view` is left unchanged.
    // 
    // Next we drop `io_buf_view`. In order to slice out `frame`, we
    // _did_ consume those bytes in a way that requires advancing, so
    // `io_buf_view` will have a deferred advance amount equivalent to
    // the size of `frame`. We advance `io_buf` by that amount.
    //
    // At this point, the data sliced off into `frame` is now gone from
    // any subsequently created view on `io_buf`.
}
```

### Integrating with the rest of Saluki and ecosystem

In principle, what we're advocating for here is eschewing both the use of `Bytes` _and_ `bytes::Buf` in favor of
`BytesBufferView<'a>` and concrete methods on it. While I think this is good for the purposes of clarity, it does mean
we will hit some sharp edges when touching other parts of the ecosystem.

#### Libraries that require `bytes::Buf`

Some libraries, such as `http-body`, have specific generic type constraints on types implementing `bytes::Buf`, which
means we would need to also implement it for `BytesBuffer`/`BytesBufferView<'a>` in order to allow for interoperability.

This should be doable without issue, as `bytes::Buf` is fundamentally the blueprint for how `BytesBufferView<'a>` is
structured.

#### Deciding what to do about `ReadIoBuffer`, `WriteIoBuffer`, `ReadWriteIoBuffer`, `CollapsibleReadWriteIoBuffer`, and so on

It's not presently clear to me just how much we would want to merge these, or get rid of them entirely. `BytesBuffer`
has been pretty consistent and constant in the codebase since it was introduced, and so we could likely stand to
de-future-proof ourselves by dropping some of these traits and making them concrete methods on `BytesBuffer` and/or
`BytesBufferView<'a>`.

That said, the traits are sometimes useful in test code since `BytesBuffer` is inherently tied to being object pooled,
which makes constructing one far less easy than just typing `let mut buf = Vec::new();` and so on.