//! A self-balancing work queue.
//!
//! A work queue distributes items from one or more producers across a fixed set of workers. Each worker has its own
//! bounded queue, so workers never contend with each other on a shared receiver.
//!
//! Items keep going to the same worker until it has a backlog of a few queued items. A moderately loaded queue therefore
//! keeps as few workers busy as it can, which avoids waking up idle workers for every item. Once that worker falls
//! behind, items go to whichever worker has the fewest queued items, so no worker sits idle while another has a backlog.
//!
//! Each sender balances independently, based on how many items are currently queued for each worker. Items already
//! queued for a worker stay with that worker, so a worker that stops making progress holds on to its backlog. Workers
//! that have stopped, by dropping their receiver, are skipped, and items queued for them are dropped.

use std::{fmt, num::NonZeroUsize, sync::Arc};

use tokio::sync::mpsc;

/// Number of queued items at which a sender moves on from its current worker.
const BACKLOG_THRESHOLD: usize = 4;

/// Creates a bounded work queue with `workers` workers.
///
/// Returns a sender, which can be cloned to create additional producers, along with one receiver per worker.
/// `capacity` is the total number of items that can be queued, and is split evenly across the workers, with each worker
/// able to queue at least one item.
pub fn bounded<T>(workers: NonZeroUsize, capacity: usize) -> (WorkQueueSender<T>, Vec<WorkQueueReceiver<T>>) {
    let per_worker_capacity = (capacity / workers.get()).max(1);
    let (senders, receivers): (Vec<_>, Vec<_>) = (0..workers.get())
        .map(|_| {
            let (tx, rx) = mpsc::channel(per_worker_capacity);
            (tx, WorkQueueReceiver { inner: rx })
        })
        .unzip();

    let sender = WorkQueueSender {
        senders: senders.into(),
        current: 0,
    };
    (sender, receivers)
}

/// The sending side of a work queue.
///
/// Cloning a sender creates another producer for the same work queue. Each clone balances independently.
pub struct WorkQueueSender<T> {
    senders: Arc<[mpsc::Sender<T>]>,
    current: usize,
}

impl<T> WorkQueueSender<T> {
    /// Sends an item to one of the workers.
    ///
    /// Workers that have stopped are skipped. If every running worker's queue is full, waits for room in a running
    /// worker's queue.
    ///
    /// # Errors
    ///
    /// If every worker has stopped, an error is returned with the item.
    pub async fn send(&mut self, mut item: T) -> Result<(), SendError<T>> {
        if self.is_stopped(self.current) || self.queued(self.current) >= BACKLOG_THRESHOLD {
            self.current = self.least_loaded();
        }

        let workers = self.senders.len();
        for offset in 0..workers {
            let index = (self.current + offset) % workers;
            match self.senders[index].try_send(item) {
                Ok(()) => return Ok(()),
                Err(mpsc::error::TrySendError::Full(returned) | mpsc::error::TrySendError::Closed(returned)) => {
                    item = returned
                }
            }
        }

        // Every running worker's queue is full. Wait for room, moving on to the next worker if one stops while waiting.
        for offset in 0..workers {
            let index = (self.current + offset) % workers;
            match self.senders[index].send(item).await {
                Ok(()) => return Ok(()),
                Err(mpsc::error::SendError(returned)) => item = returned,
            }
        }

        Err(SendError(item))
    }

    /// Returns `true` if the given worker has stopped.
    fn is_stopped(&self, index: usize) -> bool {
        self.senders[index].is_closed()
    }

    /// Returns the number of items queued for the given worker.
    fn queued(&self, index: usize) -> usize {
        let sender = &self.senders[index];
        sender.max_capacity() - sender.capacity()
    }

    /// Returns the running worker with the fewest queued items, preferring the workers after the current one on ties.
    ///
    /// If every worker has stopped, returns the current worker.
    fn least_loaded(&self) -> usize {
        let workers = self.senders.len();
        (1..=workers)
            .map(|offset| (self.current + offset) % workers)
            .filter(|&index| !self.is_stopped(index))
            .min_by_key(|&index| self.queued(index))
            .unwrap_or(self.current)
    }
}

impl<T> Clone for WorkQueueSender<T> {
    fn clone(&self) -> Self {
        Self {
            senders: Arc::clone(&self.senders),
            current: self.current,
        }
    }
}

impl<T> fmt::Debug for WorkQueueSender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkQueueSender")
            .field("workers", &self.senders.len())
            .field("current", &self.current)
            .finish()
    }
}

/// The receiving side of a work queue, for a single worker.
#[derive(Debug)]
pub struct WorkQueueReceiver<T> {
    inner: mpsc::Receiver<T>,
}

impl<T> WorkQueueReceiver<T> {
    /// Receives the next item queued for this worker.
    ///
    /// Returns `None` once every sender has been dropped and no items remain queued for this worker.
    ///
    /// This method is cancel safe: if it's used in `select!` and another branch completes first, no item is lost.
    pub async fn recv(&mut self) -> Option<T> {
        self.inner.recv().await
    }

    /// Receives the next item queued for this worker, if one is available, without waiting.
    pub fn try_recv(&mut self) -> Option<T> {
        self.inner.try_recv().ok()
    }

    /// Returns the number of items queued for this worker.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns `true` if no items are queued for this worker.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

/// An error returned when sending to a work queue whose workers have all stopped.
///
/// The item that couldn't be sent is returned in the error.
pub struct SendError<T>(pub T);

impl<T> fmt::Debug for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SendError(..)")
    }
}

impl<T> fmt::Display for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("all workers in the work queue have stopped")
    }
}

impl<T> std::error::Error for SendError<T> {}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::timeout;

    use super::*;

    fn workers(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

    fn drain<T>(receiver: &mut WorkQueueReceiver<T>) -> Vec<T> {
        std::iter::from_fn(|| receiver.try_recv()).collect()
    }

    #[tokio::test]
    async fn stays_on_a_worker_that_keeps_up() {
        let (mut sender, mut receivers) = bounded(workers(2), 4);

        // The worker drains each item before the next arrives, so every item goes to the same worker.
        for item in 0..3 {
            sender.send(item).await.unwrap();
            assert_eq!(receivers[0].recv().await, Some(item));
        }
        assert!(receivers[1].try_recv().is_none());
    }

    #[tokio::test]
    async fn spreads_items_across_workers_that_fall_behind() {
        let worker_count = 3;
        let (mut sender, mut receivers) = bounded(workers(worker_count), 64);

        // Nothing is draining, so each worker takes items until it has a backlog, and then the next one does.
        for item in 0..worker_count * BACKLOG_THRESHOLD {
            sender.send(item).await.unwrap();
        }
        assert!(receivers.iter().all(|receiver| receiver.len() == BACKLOG_THRESHOLD));
        for (worker, receiver) in receivers.iter_mut().enumerate() {
            let start = worker * BACKLOG_THRESHOLD;
            assert_eq!(drain(receiver), (start..start + BACKLOG_THRESHOLD).collect::<Vec<_>>());
        }
    }

    #[tokio::test]
    async fn moves_to_the_least_loaded_worker() {
        let (mut sender, mut receivers) = bounded(workers(3), 64);

        // Give every worker a backlog, which leaves the sender on the third worker.
        for item in 0..BACKLOG_THRESHOLD * 3 {
            sender.send(item).await.unwrap();
        }

        // Partly drain the first worker and fully drain the second, so the second is the least loaded even though the
        // first comes next in order.
        for _ in 0..BACKLOG_THRESHOLD / 2 {
            receivers[0].recv().await.unwrap();
        }
        drain(&mut receivers[1]);

        sender.send(100).await.unwrap();
        assert_eq!(drain(&mut receivers[1]), vec![100]);
    }

    #[tokio::test]
    async fn skips_full_worker_queues() {
        let (mut sender, mut receivers) = bounded(workers(2), 2);

        // Fill the first worker's queue. The next item has to go to the second worker.
        sender.send(1).await.unwrap();
        timeout(Duration::from_secs(1), sender.send(2))
            .await
            .expect("send should not wait while another worker has room")
            .unwrap();
        assert_eq!(receivers[0].recv().await, Some(1));
        assert_eq!(receivers[1].recv().await, Some(2));
    }

    #[tokio::test]
    async fn waits_for_room_when_every_queue_is_full() {
        let (mut sender, mut receivers) = bounded(workers(2), 2);
        sender.send(1).await.unwrap();
        sender.send(2).await.unwrap();

        let blocked = tokio::spawn(async move { sender.send(3).await });
        tokio::task::yield_now().await;
        assert!(!blocked.is_finished(), "send should wait while every queue is full");

        // Freeing room in either queue lets the send complete.
        let first = receivers[0].recv().await;
        let second = receivers[1].recv().await;
        blocked.await.unwrap().unwrap();
        assert_eq!((first, second), (Some(1), Some(2)));
        let rest: Vec<_> = receivers.iter_mut().flat_map(drain).collect();
        assert_eq!(rest, vec![3]);
    }

    #[tokio::test]
    async fn waits_for_a_running_worker_when_the_current_worker_has_stopped() {
        let (mut sender, mut receivers) = bounded(workers(2), 2);

        // Stop the first worker, which is the sender's current worker, and fill the second worker's queue.
        drop(receivers.remove(0));
        sender.send(1).await.unwrap();

        // Every running worker's queue is full, so the send waits rather than failing on the stopped worker.
        let blocked = tokio::spawn(async move { sender.send(2).await });
        tokio::task::yield_now().await;
        assert!(!blocked.is_finished(), "send should wait for the running worker");

        assert_eq!(receivers[0].recv().await, Some(1));
        blocked.await.unwrap().unwrap();
        assert_eq!(receivers[0].recv().await, Some(2));
    }

    #[tokio::test]
    async fn moves_on_when_a_worker_stops_while_waiting_for_it() {
        let (mut sender, mut receivers) = bounded(workers(2), 2);
        sender.send(1).await.unwrap();
        sender.send(2).await.unwrap();

        // Both queues are full, so the send waits on the current worker.
        let blocked = tokio::spawn(async move { sender.send(3).await });
        tokio::task::yield_now().await;
        assert!(!blocked.is_finished(), "send should wait while every queue is full");

        // Stopping that worker moves the send on to the other worker, which it waits on until there's room.
        drop(receivers.remove(0));
        tokio::task::yield_now().await;
        assert!(!blocked.is_finished(), "send should wait for the remaining worker");

        assert_eq!(receivers[0].recv().await, Some(2));
        blocked.await.unwrap().unwrap();
        assert_eq!(receivers[0].recv().await, Some(3));
    }

    #[tokio::test]
    async fn errors_with_the_item_once_every_worker_stops() {
        let (mut sender, receivers) = bounded::<u32>(workers(2), 2);
        drop(receivers);

        let SendError(item) = sender.send(7).await.unwrap_err();
        assert_eq!(item, 7);
    }

    #[tokio::test]
    async fn receivers_finish_once_every_sender_drops() {
        let (mut sender, mut receivers) = bounded(workers(1), 4);
        let mut clone = sender.clone();
        sender.send(1).await.unwrap();
        clone.send(2).await.unwrap();
        drop(sender);
        drop(clone);

        assert_eq!(receivers[0].recv().await, Some(1));
        assert_eq!(receivers[0].recv().await, Some(2));
        assert_eq!(receivers[0].recv().await, None);
    }
}
