---
date: 2025-11-04
title: ADR 001 - Switching from `Bytes` to `BufferView<'a>` for improved memory efficiency in payload deserialization.
---

# ADR 001 - Switching from `Bytes` to `BufferView<'a>` for improved memory efficiency in payload deserialization.

## Context and Problem Statement

When deserializing payloads, such as DogStatsD, framers must allocate to hold the resulting frames that they extract. This is not memory efficient, as it means we always pay a penalty by allocating, even when a borrowed byte slice could otherwise be used. We want to be able to avoid allocating for every single frame that we extract from a payload, both for the obvious benefits of reduced memory usage and fragmentation, but also for the potential latency improvements by not allocating in the critical path of receiving data.

## Context

Saluki's overarching design goal is to provide efficient and deterministic memory usage. In order to achieve this, pre-allocated I/O buffers are preferred for accepting data over the network. We do this with a custom type, `BytesBuffer`, and some object pooling facilities to support reuse of buffers in an ergonomic way. On top of that, we utilize traits from the [`bytes`][bytes] crate, namely [`Buf`][bytes_Buf], to make it easy to work with these buffers. These traits provide ergonomic support for using buffers as cursors: read a portion of the buffer, and then "advance" the cursor, so that the same data cannot be read again.

When framers extract a valid frame, we want to update the source buffer to mark those bytes as consumed. As mentioned above, we use [`Buf`][bytes_Buf] to achieve this, which works well as it stands: the act of carving out a certain chunk of bytes to create our "frame" also advances the source buffer. With the extracted frame, we can then use additional methods from [`Buf`][bytes_Buf] to constrain the frame. This is important because it allows us to remove the delimiter itself, which callers don't care about. This ensures we can both satisfy the need to consume the entire frame, while advancing the source buffer, and also return a sanitized frame that is ready to be used by callers.

The main problem with this approach is that [`bytes`][bytes] has its own concrete implementations -- [`Bytes`][bytes_Bytes] and [`BytesMut`][bytes_BytesMut], which are not amenable to the type of object pooling we do. In order to ensure that our fixed number of I/O buffers can be reused, we need control over when _usages_ of those buffers is handed back to the caller that retrieved them from the pool in the first place. [`bytes`][bytes] follows an approach where a mutable buffer ([`BytesMut`][bytes_BytesMut]) is used for collecting data (such as the buffer being read into from a network socket read), and then is "frozen" to create a [`Bytes`][bytes_Bytes] instance, which is then able to be cheaply cloned and sliced by virtue of using atomic reference counting under the hood. We can only recover the original [`BytesMut`][bytes_BytesMut] by fallibly trying to consume a [`Bytes`][bytes_Bytes] instance, which may fail if other copies still exist. This means we cannot provide a misuse-resistant object pooling mechanism that ensures we only ever have a maximum of N buffers at any given time, which is ultimately why we created `BytesBuffer`.

The crux of the problem that this ADR aims to solve is that, when using a custom implementation of [`Buf`][bytes_Buf], none of the optimizations in [`bytes`][bytes] are available to us. This means that the slicing/advancing operations that would otherwise be cheap atomic counter operations now become allocations for intermediate buffers and so on. We want to be able to use our custom buffer type (`BytesBuffer`) in a way that provides the ergonomics and safety of interacting with [`Bytes`][bytes_Bytes] through its [`Buf`][bytes_Buf] implementation, while also still supporting the ability to pool the buffers.

## Considered Options

We considered two possible approaches to improve the situation:

- use borrowed byte slices directly (e.g. pass around `&[u8]`)
- create our own [`Bytes`][bytes_Bytes]-style abstraction to manage borrowed byte slices (`BufferView<'a>`)

### Use borrowed byte slices directly

Using borrowed byte slices meets our constraint of avoiding additional allocations for intermediate frames, as framers would simply be returning subslices of the input buffer they were given. However, borrowed byte slices are not ergonomic to work with in terms of ensuring they're advanced.

In order to accept a byte slice as the input and be able to manipulate it such that we could ensure it was "advanced" past an extracted frame, a mutable borrow would be required: we have to be able to modify the original borrow passed in to the framer. However, since we would also need to take an immutable borrow to capture our "frame", this turns into a classic mutable/immutable borrowing violation.

### Create our own `Bytes`-style abstraction to manage borrowed byte slices (`BufferView<'a>`)

This option is an incremental addition on top of borrowed byte slices, which uses immutable borrows and index offsets to provide a "view" over a byte slice, generating the subslice on demand by holding the start and end indices of the slice. Concretely, this maintains the benefit of avoiding intermediate allocations while providing the necessary ergonomics to slice up a source buffer during the course of extracting frames.

We would create a new type, `BufferView<'a>`, that holds an immutable borrow to an underlying byte slice -- the complete frame, in this case -- and then generates the "view" representing the cleaned up frame by tracking index offsets -- how far from the start, how far from the end -- which can then be used to exclude frame delimiters. Since `BufferView<'a>` holds a borrow of the full frame slice, it can also be used to determine the amount by which the input buffer needs to be advanced. This is the core part of the solution: we maintain immutable borrows while extracting frames, and carry enough information in `BufferView<'a>` to handle the advancing outside of the framer.

This means that in order to meet our goal of ensuring the the source buffer is properly advanced, we need to handle that outside of the framer, which we've abstracted away in a new `Framed` type: this type provides an iterator-like interface over a source buffer and framer implementation, handling advancing the source buffer as frames are extracted. Finally, an additional helper is required to ensure that input buffers given to a framer are consumed from left to right: while we can theoretically represent any byte slice with `BufferView<'a>`, we don't want to mistakenly allow a frame to come from the middle of a buffer, as we can't "advance" in that way. This helper, `RawBuffer`, is a wrapper around a byte slice that provides the only methods for creating a `BufferView<'a>`. These methods ensure that views are created from the start of the buffer.

With these helpers, we can now ensure that framers are only able to extract frames from the start of a buffer, that the frame they extract carries the entire frame length necessary for properly advancing the source buffer, and that the logic for advancing the source buffer exists in a single place that can be well tested.

## Decision Outcome

We will implement the "Create our own `Bytes`-style abstraction to manage borrowed byte slices (`BufferView<'a>`)" option.

This option allows to meet our two main goals: avoiding intermediate allocations as well as providing the necessary ergonomics to ensure source buffers are automatically advanced/consumed as frames are extracted. It achieves this with otherwise straightforward helper types, although one main downside is that it requires a level of upfront documentation to explain the invariants, which are not able to be entirely upheld purely through the type system.

[bytes]: https://docs.rs/bytes/latest/bytes/
[bytes_Buf]: https://docs.rs/bytes/latest/bytes/buf/trait.Buf.html
[bytes_Bytes]: https://docs.rs/bytes/latest/bytes/struct.Bytes.html
[bytes_BytesMut]: https://docs.rs/bytes/latest/bytes/struct.BytesMut.html
