---
date: 2025-11-04
title: ADR 001 - Switching from `bytes::Bytes` to `BufferView<'a>` for improved memory efficiency in payload deserialization.
---

# ADR 001 - Switching from `bytes::Bytes` to `BufferView<'a>` for improved memory efficiency in payload deserialization.

## Context and Problem Statement

When deserializing payloads, such as DogStatsD, using `bytes::Bytes`, framers must allocate to holding the resulting frames that they extract. This is not memory efficient, as it means we always pay a penalty by allocating, even when a borrowed byte slice could otherwise be used. We want to be able to avoid allocating for every single frame that we extract from a payload, both for the obvious benefits of reduced memory usage and fragmentation, but also for the potential latency improvements by not allocating in the critical path of receiving data.

## Context

Saluki's overarching design goal is to provide efficient and deterministic memory usage. In order to achieve this, pre-allocated I/O buffers are preferred for accepting data over the network. We do this with a custom type, `BytesBuffer`, and some object pooling facilities to support reuse of buffers in an ergonomic way. On top of that, we utilize traits from the `bytes` crate, namely `Buf`, to make it easy to work with these buffers. These traits provide ergonomic support for using buffers as cursors: read a portion of the buffer, and then "advance" the cursor, so that the same data cannot be read again.

When framers extract a valid frame, we want to update the source buffer to mark those bytes as consumed. As mentioned above, we use `Buf` to achieve this, which works well as it stands: the act of carving out a certain chunk of bytes to create our "frame" also advances the source buffer. With the extracted frame, we can then use additional methods from `Buf` to constrain the frame. This is important because it allows us to remove the delimiter itself, which callers don't care about. This ensures we can both satisfy the need to consume the entire frame, while advancing the source buffer, and also return a sanitized frame that is ready to be used by callers.

The main problem with this approach is that `bytes` has its own concrete implementations -- `bytes::Bytes` and `bytes::BytesMut` -- which are not amenable to the type of object pooling we do. In order to ensure that our fixed number of I/O buffers can be reused, we need control over when _usages_ of those buffers is handed back to the caller that retrieved them from the pool in the first place. `bytes` follows an approach where a mutable buffer (`BytesMut`) is used for collecting data (such as the buffer being read into from a network socket read), and then is "frozen" to create a `Bytes` instance, which is then able to be cheaply cloned and sliced by virtue of using atomic reference counting under the hood. We can only recover the original `BytesMut` by fallibly trying to consume a `Bytes` instance, which may fail if other copies still exist. This means we cannot provide a misuse-resistant object pooling mechanism that ensures we only ever have a maximum of N buffers at any given time, which is ultimately why we created `BytesBuffer`.

The crux of the problem that this ADR aims to solve is that, when using a custom implementation of `Buf`, none of the optimizations in `bytes` are available to us. This means that the slicing/advancing operations that would otherwise be cheap atomic counter operations now become allocations for intermediate buffers and so on.

## Considered Options

We considered two possible approaches to improve the situation:

- use borrowed byte slices directly (e.g. pass around `&[u8]`)
- create our own `Bytes`-style abstraction to manage borrowed byte slices (`BufferView<'a>`)

### Use borrowed byte slices directly

## Decision Outcome
