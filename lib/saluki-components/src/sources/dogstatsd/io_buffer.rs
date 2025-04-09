use bytes::Buf as _;
use saluki_core::pooling::ObjectPool;
use saluki_io::buf::{BytesBuffer, CollapsibleReadWriteIoBuffer as _, ReadIoBuffer as _};
use tracing::trace;

/// An ergonomic wrapper around fairly utilizing I/O buffers from an object pool.
///
/// As I/O buffer pools are fixed-size for the DogStatsD source, this presents an issue when looking to handle more
/// connections than there are I/O buffers. If each new connection was simply to acquire a buffer from the pool and then
/// reuse it until the connection closed, all new connections made after the buffer pool was exhausted would be blocked
/// on acquiring an I/O buffer, potentially indefinitely. This would occur even if the existing connections were idle.
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
/// - acquire a new buffer when the current buffer does not exist
/// - retain the current buffer (and collapse it) if there is remaining data, _or_ release it if there is no remaining data
///
/// When the current buffer still has remaining data, we must preserve the buffer and its data as we could otherwise be
/// throwing away a partial frame that will be fulfilled by the next socket read. We additionally handle collapsing the
/// current buffer when there is remaining data, as this ensures that all available capacity is contiguous and directly
/// follows whatever remaining data exists.
///
/// When the current buffer has no remaining data, we can safely release it back to the pool prior to immediately
/// reacquiring a new buffer.
pub struct IoBufferManager<'a, O>
where
    O: ObjectPool<Item = BytesBuffer>,
{
    pool: &'a O,
    current: Option<BytesBuffer>,
}

impl<'a, O> IoBufferManager<'a, O>
where
    O: ObjectPool<Item = BytesBuffer>,
{
    /// Creates a new `IoBufferManager` with the given object pool.
    pub fn new(pool: &'a O) -> Self {
        Self { pool, current: None }
    }

    /// Returns a mutable reference to the current buffer.
    ///
    /// This method may or may not release the current buffer depending on if remaining data exists or not. When no
    /// buffer is available (including after intentionally releasing it), a new buffer will be acquired before
    /// returning.
    pub async fn get_buffer_mut(&mut self) -> &mut BytesBuffer {
        // Consume the current buffer, potentially keeping it around if it has remaining data.
        let current = match self.current.take() {
            Some(mut buffer) => {
                if buffer.has_remaining() {
                    // Buffer still has data, so collapse it and re-use it.
                    buffer.collapse();
                    Some(buffer)
                } else {
                    // Buffer has no more data, so release it.
                    None
                }
            }
            None => None,
        };

        // Acquire a new buffer if we didn't keep the current buffer around, or if we had no current one.
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
