use bytes::Buf as _;
use saluki_core::pooling::ObjectPool;
use saluki_io::{
    buf::{BytesBuffer, CollapsibleReadWriteIoBuffer as _, ReadIoBuffer as _},
    net::Stream,
};
use tracing::trace;

/// An ergonomic wrapper around fairly utilizing I/O buffers from an object pool.
///
/// DogStatsD uses a bounded elastic I/O buffer pool, which can allocate additional buffers on demand until it reaches
/// its configured cap. Once the pool reaches that cap, new connections must wait for existing connections to release
/// buffers. If each connection simply acquired a buffer and reused it until the connection closed, waiters could be
/// blocked indefinitely even when existing connections were idle.
///
/// `IoBufferManager` provides a simple, ergonomic wrapper over a basic pattern of treating the current buffer as
/// optional, which allows the wrapper to release the current buffer back to the pool, and acquire a new one, all before
/// returning a reference to the buffer. This provides fairness by ensuring that tasks which are waiting for a buffer
/// can eventually acquire one once existing tasks are able to reach a consistent point that they can release their
/// buffer.
///
/// ## Release behavior
///
/// This wrapper provides two basic behaviors:
///
/// - acquire a new buffer when the current buffer doesn't exist
/// - retain the current buffer (and collapse it) if there is remaining data, _or_ release it if there is no remaining data
///
/// When the current buffer still has remaining data, we must preserve the buffer and its data as we could otherwise be
/// throwing away a partial frame that will be fulfilled by the next socket read. We additionally handle collapsing the
/// current buffer when there is remaining data, as this ensures that all available capacity is contiguous and directly
/// follows whatever remaining data exists.
///
/// When the current buffer has no remaining data, we can safely release it back to the pool prior to immediately
/// reacquiring a new buffer.
///
/// ## Connectionless streams
///
/// For connectionless streams, the current buffer is never released and is always reused. This is because
/// connectionless streams are often higher volume in terms of the number of socket reads that occur, and are a fixed
/// number that doesn't change with the amount of traffic that's being sent. This allows us to reduce the overhead
/// related to acquiring and releasing buffers, ensuring that these inherently lossy streams aren't introducing
/// additional overhead that might cause unnecessary packet loss.
pub struct IoBufferManager<'a, O>
where
    O: ObjectPool<Item = BytesBuffer>,
{
    pool: &'a O,
    should_retain: bool,
    current: Option<BytesBuffer>,
}

impl<'a, O> IoBufferManager<'a, O>
where
    O: ObjectPool<Item = BytesBuffer>,
{
    /// Creates a new `IoBufferManager` for the given stream and object pool.
    pub fn new(pool: &'a O, stream: &Stream) -> Self {
        Self {
            pool,
            should_retain: stream.is_connectionless(),
            current: None,
        }
    }

    /// Returns a mutable reference to the current buffer.
    ///
    /// This method may or may not release the current buffer depending on if remaining data exists or not. When no
    /// buffer is available (including after intentionally releasing it), a new buffer will be acquired before
    /// returning.
    pub async fn get_buffer_mut(&mut self) -> &mut BytesBuffer {
        // Consume the current buffer, if it exists.
        let current = self.current.take().and_then(|mut buffer| {
            if buffer.has_remaining() || self.should_retain {
                // Collapse the remaining data in the buffer and continue using it.
                buffer.collapse();
                Some(buffer)
            } else {
                // Release the buffer back to the pool.
                None
            }
        });

        // Determine our replacement buffer: either we're sticking with the current buffer, or reacquiring a new one.
        let new_current = match current {
            Some(buffer) => buffer,
            None => {
                let new_buffer = self.pool.acquire().await;
                trace!(
                    remaining = new_buffer.remaining(),
                    capacity = new_buffer.capacity(),
                    "Acquired new buffer from pool."
                );

                new_buffer
            }
        };

        // Finally, store the new buffer and reference a mutable reference to it.
        self.current = Some(new_current);
        self.current.as_mut().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bytes::{Buf as _, BufMut as _};
    use saluki_core::pooling::FixedSizeObjectPool;
    use saluki_io::buf::{BytesBuffer, FixedSizeVec, ReadIoBuffer as _};
    use tokio::time::timeout;

    use super::IoBufferManager;

    const BUFFER_CAPACITY: usize = 16;

    fn test_pool(name: &'static str, count: usize) -> FixedSizeObjectPool<BytesBuffer> {
        FixedSizeObjectPool::with_builder(name, count, || FixedSizeVec::with_capacity(BUFFER_CAPACITY))
    }

    #[tokio::test]
    async fn get_buffer_mut_acquires_buffer_when_none_is_held() {
        let pool = test_pool("io-buffer-acquire", 1);
        let mut manager = IoBufferManager {
            pool: &pool,
            should_retain: false,
            current: None,
        };

        let buffer = manager.get_buffer_mut().await;
        assert_eq!(buffer.remaining(), 0, "a freshly acquired buffer has nothing to read");
        assert_eq!(buffer.capacity(), BUFFER_CAPACITY);
    }

    #[tokio::test]
    async fn get_buffer_mut_collapses_and_retains_remaining_data() {
        // When the current buffer still has unread data, get_buffer_mut must keep that buffer, collapse it so the
        // unread tail moves to the front, and expose the freed capacity for the next socket read -- never dropping a
        // partial frame.
        let pool = test_pool("io-buffer-collapse", 1);
        let mut manager = IoBufferManager {
            pool: &pool,
            should_retain: false,
            current: None,
        };

        {
            let buffer = manager.get_buffer_mut().await;
            buffer.put_slice(b"framed-data"); // 11 bytes written
            buffer.advance(6); // consume "framed", leaving "-data" as a partial frame
            assert_eq!(buffer.chunk(), b"-data");
            // Pre-collapse, only the trailing free space (capacity - written length) is writable.
            assert_eq!(buffer.remaining_mut(), BUFFER_CAPACITY - 11);
        }

        let buffer = manager.get_buffer_mut().await;
        assert_eq!(
            buffer.chunk(),
            b"-data",
            "unread data must be preserved across the call"
        );
        assert_eq!(buffer.remaining(), 5);
        // Post-collapse, the consumed prefix has been reclaimed as writable capacity.
        assert_eq!(buffer.remaining_mut(), BUFFER_CAPACITY - 5);
    }

    #[tokio::test]
    async fn get_buffer_mut_releases_drained_buffer_before_reacquiring() {
        // Fairness contract (see module docs): a connection-oriented reader that has fully drained its buffer must
        // release it back to the pool *before* acquiring a replacement. With a single-slot pool this only makes
        // progress if the release happens first -- an acquire-before-release ordering would deadlock.
        let pool = test_pool("io-buffer-release", 1);
        let mut manager = IoBufferManager {
            pool: &pool,
            should_retain: false,
            current: None,
        };

        {
            let buffer = manager.get_buffer_mut().await;
            buffer.put_slice(b"done");
            buffer.advance(4); // fully consumed: nothing remaining
            assert_eq!(buffer.remaining(), 0);
        }

        let reacquired = timeout(Duration::from_secs(5), manager.get_buffer_mut())
            .await
            .expect("draining then reacquiring on a saturated pool must not deadlock");
        assert_eq!(reacquired.remaining(), 0);
        assert_eq!(reacquired.capacity(), BUFFER_CAPACITY);
    }

    #[tokio::test]
    async fn connectionless_stream_reuses_buffer_across_reads() {
        // Connectionless streams keep their buffer (`should_retain = true`) rather than releasing it, even once
        // drained. The retain-vs-release decision for an empty buffer isn't observable from the buffer alone (both
        // yield an empty buffer), but this confirms the connectionless path preserves unread data across calls and
        // keeps returning a usable buffer once drained.
        let pool = test_pool("io-buffer-connectionless", 1);
        let mut manager = IoBufferManager {
            pool: &pool,
            should_retain: true,
            current: None,
        };

        {
            let buffer = manager.get_buffer_mut().await;
            buffer.put_slice(b"partial");
            buffer.advance(3); // leave "tial" unread
        }

        {
            let buffer = manager.get_buffer_mut().await;
            assert_eq!(
                buffer.chunk(),
                b"tial",
                "connectionless reuse must preserve unread data"
            );
            buffer.advance(4); // drain it fully
        }

        let buffer = manager.get_buffer_mut().await;
        assert_eq!(buffer.remaining(), 0);
        assert_eq!(buffer.capacity(), BUFFER_CAPACITY);
    }
}
