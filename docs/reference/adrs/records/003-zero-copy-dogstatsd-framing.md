---
date: 2026-11-06
title: ADR 003 - Zero-copy DogStatsD framing over pooled I/O buffers
---

# ADR 003 - Zero-copy DogStatsD framing over pooled I/O buffers

## Problem statement

The DogStatsD source allocates memory for every packet it receives. Each frame is copied out of the pooled I/O buffer
before the codec reads it, even though the codec only needs to borrow the bytes. Allocation is also expensive in Agent
Data Plane (ADP): it runs jemalloc without a thread cache, so every allocation and free goes to the shared arena.

How do we hand frames to the codec without copying them, while keeping pooled I/O buffers and the way we read into them?

## Context

### How framing works today

DogStatsD reads from the socket into a `BytesBuffer` from a bounded object pool, then cuts frames off the front of that
buffer until no complete frame remains. Connected streams collapse the leftover bytes and read again. There are two
framings:

- **Direct:** newline-delimited messages, for UDP, Unix datagram sockets, and named pipes.
- **Nested:** length-delimited outer frames holding newline-delimited messages, for Unix stream sockets and TCP.

Framers return frames as `Bytes`, using `Buf::copy_to_bytes`. On `BytesBuffer` that's the default implementation: allocate
and copy. `NestedFramer` holds each outer frame as `Bytes`, and splitting a message out of a `Bytes` that came from a
`Vec` allocates a shared header on the first split. Allocations per message:

| Listener | Allocations |
|---|---|
| UDP, Unix datagram, named pipe | 1 per message |
| Unix stream or TCP, one message per outer frame | 2 per message |
| Unix stream or TCP, many messages per outer frame | 2 per outer frame |

Under ADP's jemalloc configuration, this framing is 33% to 49% of the combined time to frame and parse a message.

### Why not `Bytes` and `BytesMut`

The [`bytes`](https://docs.rs/bytes) crate makes slicing cheap by reference counting, but a frozen `Bytes` can only be
turned back into a `BytesMut` fallibly, so `bytes` can't guarantee that at most N I/O buffers exist. We need that
guarantee for deterministic memory use, which is why `BytesBuffer` and its pool exist, and why `copy_to_bytes` on it
degrades to a copy.

### A previous attempt

[#983](https://github.com/DataDog/saluki/pull/983) switched framers to borrowed slices. To hand out slices of the buffer
while still advancing it afterwards, its `Framed` type converted `&mut` to a raw pointer and advanced the buffer when
dropped. That was sound, but fragile: the safety argument lived in comments, nested framing was a hard-coded mode
rather than a framer, every frame went through dynamic dispatch, and framers lost access to the buffer's capacity.

### Requirements

- No allocation or copy per frame when the consumer only borrows it.
- Keep the pooled `BytesBuffer`, and the read, frame, collapse, read cycle.
- A frame must never be handed out without the buffer having been advanced past it by exactly the right amount. This
  should hold by construction, not because a drop guard or deferred step runs.
- Frames come off the front of the buffer only. Partial frames stay in the buffer for the next read.
- Nested framing must perform as well as direct framing.
- Today's EOF, error, and size-limit behavior is unchanged, as are the hooks the source uses: completed outer frame
  counts and outer frame capture.
- A frame can be held across `.await` in the decode loop.
- No `unsafe` code.

## Considered options

Each option was built as a prototype and measured in a shared benchmark harness with DogStatsD workloads modeled on the
SMP regression cases. Before any timing, each prototype had to pass a conformance suite that compared it against
today's framing, using direct and nested layouts, randomized read sizes, and edge cases.

- **PR #983's design**: framers over `&mut &[u8]`, with the buffer advanced on drop through a raw pointer.
- **Span framers with deferred advancement**: framers describe a frame's position without touching the buffer, and a
  lending iterator advances past each frame when the next one is requested, or on drop.
- **Today's framer shape with a consume primitive**: framers advance the buffer through a primitive that returns the
  bytes it advanced past.
- **Span framers driven by the consume primitive**: the two previous options combined, so advancement happens at once.
- **Internal iteration**: frames are passed to a closure, and the buffer is advanced once afterwards.
- **Reference-counted slices**: the pooled buffer is frozen behind `Bytes::from_owner`, and frames are `Bytes` slices
  of it. Leftover bytes are copied to a new buffer.
- **A single DogStatsD framer over a split-borrow read cursor**: the buffer lends a shared borrow of its bytes and an
  exclusive borrow of its read index. Taking a frame moves the read index, and in stream mode each outer frame is taken
  whole.

## Decision outcome

Chosen option: **A single DogStatsD framer over a split-borrow read cursor**

The buffer's bytes and its read index are separate fields, so the buffer can lend both at once: a shared borrow of the
bytes, and an exclusive borrow of the read index. The cursor's `take(n)` moves the read index past `n` bytes, then
returns them, borrowed for as long as the buffer is borrowed. That is the only way to get bytes that outlive the
cursor, so every frame comes from bytes the buffer has already been advanced past, with no raw pointers and no drop step.
The compiler stops the buffer from being collapsed, cleared, or written while any frame is alive.

In stream mode, the framer takes each complete outer frame in a single `take`, then cuts its messages from the slice it
took. Between reads, the front of the buffer is therefore always at a frame boundary, and the framer holds no state from
one read to the next. Partial outer frames are never taken.

The other options each failed a requirement, or were slower:

- PR #983's design was fragile, and among the slowest of the borrowing designs. The chosen option is 7% to 73% faster
  on framing alone.
- Deferred advancement meets every requirement with no unsafe code, but it ties a frame's lifetime to the iterator, so
  the source can't record hooks until each frame is done. The chosen option is also 8% to 45% faster on framing alone.
- The two consume-primitive options advance a nested frame piece by piece, so the framer has to remember where it is
  within an outer frame between reads, and that state becomes invalid if the buffer is cleared or replaced mid-frame.
  The chosen option is also 1% to 25% faster on framing alone.
- Internal iteration was the fastest, up to 8% ahead on framing alone and level once parsing is included. But the
  closure can't `.await`, so it doesn't fit the decode loop.
- Reference-counted slices avoid the copy, but cost atomic operations per frame and a pool round trip per buffer. They
  also stop buffers being collapsed in place, and let a single held frame pin a whole buffer. They were the slowest
  zero-copy option: the chosen option is 1.5x to 2.9x faster on framing alone.

### Results

Millions of frames per second, measured in one run with the `optimized-release` profile, which is how ADP releases are
built. Small and medium messages follow the shapes of typical DogStatsD traffic; the SMP shape follows the SMP
regression cases.

| Scenario | Today, system allocator | Chosen, system allocator | Today, ADP jemalloc | Chosen, ADP jemalloc |
|---|---|---|---|---|
| Datagrams, small messages | 15.5 | 40.5 (2.6x) | 6.3 | 41.0 (6.5x) |
| Datagrams, SMP shape | 2.9 | 5.3 (1.9x) | 2.5 | 5.6 (2.3x) |
| Stream, one message per frame, small | 7.2 | 32.9 (4.6x) | 3.0 | 33.3 (11.0x) |
| Stream, many messages per frame, small | 20.1 | 42.1 (2.1x) | 20.3 | 41.5 (2.0x) |
| Datagrams, medium, framing and parsing | 2.2 | 2.4 (1.1x) | 1.8 | 2.4 (1.4x) |
| Stream, one message per frame, medium, framing and parsing | 1.8 | 2.4 (1.3x) | 1.3 | 2.3 (1.8x) |

The chosen option makes no allocations per frame in any layout, and framing drops to about 12% of the combined time to
frame and parse a message.

Beyond the conformance suite, the chosen option was checked with:

- 1.35 million random cases of differential fuzzing against today's framers and an independent model, with no
  mismatches;
- compile-fail tests for each borrow guarantee;
- mutation tests of the conformance checks themselves.

### Consequences

- Good, because the DogStatsD source no longer allocates per frame. That's the biggest gain where it allocated most:
  streams with one message per frame, and anywhere under ADP's jemalloc configuration.
- Good, because a frame implies the buffer has been advanced past it, by construction, with no `unsafe` code.
- Good, because the framer holds no state between reads, so its state can't fall out of step with the buffer.
- Good, because traffic capture gets each outer frame as a borrowed slice, with no extra copy.
- Bad, because consumers that need an owned frame must now copy it. Before, nested framing could hand them a
  reference-counted slice of an outer frame it had already copied.
- Bad, because the generic `Framer` trait and the framers in `saluki-io` go away. Their only user is the DogStatsD source,
  so any future protocol needs its own framing.
- Bad, because a caller that stops partway through an outer frame skips that frame's remaining messages. The decode loop
  never stops early, so this can't happen today.

## References

- [#983](https://github.com/DataDog/saluki/pull/983): the previous attempt.
