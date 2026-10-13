//! The queue between the source's datagram readers and its datagram decoders.
//!
//! Every connectionless listener (UDP, Unix datagrams) has a reader task per socket, which receives datagrams into
//! pooled buffers and queues them; a fixed pool of decoder tasks takes them off the queue and decodes them. Any decoder
//! takes any datagram: the decoders share one queue, so a decoder that is free takes the next datagram, whichever
//! decoder is busy.
//!
//! The queue is a lock-free list of blocks of slots ([`SegQueue`]), bounded by a count of the items it holds, with two
//! [`Notify`]s: one wakes a decoder waiting for a datagram, the other a reader waiting for room. Neither side takes a
//! lock to queue or take a datagram, so a decoder never waits for another decoder: only for a datagram, when the queue
//! is empty. (A receiver shared behind an asynchronous mutex, held while waiting for the next datagram, made every
//! decoder wait for the one holding it, and the woken waiter often could not run until the holder had decoded its
//! datagram, which serialized the decoders.) The list allocates its blocks as datagrams arrive and frees them as they
//! are taken, so an idle queue holds next to nothing, whatever its capacity.
//!
//! One queue for every decoder, rather than a queue per decoder fed in rotation, keeps what a single queue gives: a
//! datagram never waits behind a busy decoder while another is free, and under light load the decoder that waited last
//! takes the next datagram, so that a few decoders do the work with fuller event buffers.
//!
//! **Wake-ups.** A datagram queued while decoders sleep wakes one of them, but only one at a time: until the decoder
//! woken last has started running, further datagrams wake nobody, since that decoder takes whatever is queued when it
//! runs, and decoders already running take datagrams without waiting. Once it runs, the next datagram queued wakes
//! another, so the decoders working grow while datagrams keep coming faster than they are taken. The woken decoder,
//! though, waits for the thread that woke it (the scheduler runs a task woken from a worker thread on that thread,
//! where other threads cannot take it), and that thread is busy reading. So a reader that queues more than
//! `BOOST_BACKLOG_BYTES` while the decoder it woke has not run wakes one more, once: the second wake-up also moves the
//! first decoder where an idle thread takes it, and the decoding leaves the reader's thread. A few small datagrams are
//! decoded on the reader's thread as well as anywhere, and waking a decoder per datagram, as a plain notification
//! would, costs a wake-up of another thread for almost every datagram at one metric per datagram, and with it a third
//! to a half more CPU per metric. A reader waiting for room is woken only while one waits.
//!
//! **Cooperative scheduling.** As the channels of the Tokio scheduler do, a send and a receive each spend a unit of the
//! task's budget, so that a decoder taking datagram after datagram from a queue that never empties still yields its
//! thread now and then, to a reader or a woken decoder waiting there.
//!
//! The queue closes for decoders when the last sender is dropped: they take what is left, then see the end. It closes
//! for readers when the last receiver is dropped: a send then fails and gives the datagram back.

use std::{
    pin::pin,
    sync::{
        atomic::{fence, AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
};

use crossbeam_queue::SegQueue;
use tokio::{sync::Notify, task::coop::cooperative};

/// The bytes of datagrams a reader queues while a receiver it woke has not run, beyond which it wakes one more.
///
/// The receiver woken last waits for the thread that woke it, which is busy reading. One or two full datagrams are
/// tens to hundreds of microseconds of decoding, worth moving to an idle thread; the same bytes in datagrams of one
/// metric are a burst that the reader's thread decodes about as fast. Chosen by measurement: 32 KiB kept too much
/// decoding on the reader's thread at eight decoders, and waking per datagram cost the most CPU per metric.
const BOOST_BACKLOG_BYTES: usize = 8 * 1024;

struct Shared<T> {
    items: SegQueue<T>,

    /// The most items the queue holds.
    capacity: usize,

    /// The items queued, and those being queued: a sender reserves its item's place here before queuing it, and a
    /// receiver gives the place back after taking an item, so that the queue never holds more than `capacity`.
    len: AtomicUsize,

    /// Notified, for one waiting receiver (the last to wait), when an item is queued and no wake-up is pending; and,
    /// for every waiter, when the last sender is dropped.
    readable: Notify,

    /// Whether a receiver was woken for an item (or a wake-up was left for the next receiver to wait) and has not yet
    /// started running: while one is pending, queuing an item wakes nobody, unless a backlog builds.
    wake_pending: AtomicBool,

    /// Whether, while a wake-up is pending, a reader's backlog woke one more receiver.
    boosted: AtomicBool,

    /// Notified, for one waiting sender, when an item is taken while senders wait; and, for every waiter, when the last
    /// receiver is dropped.
    writable: Notify,

    /// The senders waiting for room.
    blocked_senders: AtomicUsize,

    senders: AtomicUsize,
    receivers: AtomicUsize,
}

impl<T> Shared<T> {
    /// Queues `item` if there is room, and returns whether it woke a receiver (or left a wake-up for the next one to
    /// wait), which it does only if no wake-up was pending.
    fn try_push(&self, item: T) -> Result<bool, T> {
        // The count only bounds the queue, so it needs no ordering: the fences below order it where it matters.
        if self.len.fetch_add(1, Ordering::Relaxed) >= self.capacity {
            self.len.fetch_sub(1, Ordering::Relaxed);
            return Err(item);
        }
        self.items.push(item);

        // Orders the push before the read of `wake_pending`, as a receiver orders its clearing of the flag before its
        // next pop (see `take_wake_up`): either the receiver sees this item, or this sees the flag cleared and wakes a
        // receiver.
        fence(Ordering::SeqCst);
        if !self.wake_pending.load(Ordering::Relaxed) && !self.wake_pending.swap(true, Ordering::Relaxed) {
            // The receiver that waited last, and so most recently decoded, takes it: under light load, a few
            // decoders do the work, with warm caches and fuller event buffers, while the others stay asleep.
            self.readable.notify_last();
            return Ok(true);
        }
        Ok(false)
    }

    /// Wakes one more receiver, once per pending wake-up.
    fn boost(&self) {
        if !self.boosted.load(Ordering::Relaxed) && !self.boosted.swap(true, Ordering::Relaxed) {
            self.readable.notify_last();
        }
    }

    fn try_pop(&self) -> Option<T> {
        let item = self.items.pop()?;
        self.len.fetch_sub(1, Ordering::Relaxed);

        // Orders the room made before the read of the waiting senders, as a sender orders its registration before
        // trying again: either the sender sees the room, or this sees the sender and wakes it.
        fence(Ordering::SeqCst);
        if self.blocked_senders.load(Ordering::Relaxed) > 0 {
            self.writable.notify_one();
        }
        Some(item)
    }

    /// Clears the pending wake-up, once the receiver holding it runs, so that the next item queued wakes another
    /// receiver.
    fn take_wake_up(&self) {
        self.boosted.store(false, Ordering::Relaxed);
        self.wake_pending.store(false, Ordering::Relaxed);
        // Orders the clearing before this receiver's next pop: see `try_push`.
        fence(Ordering::SeqCst);
    }
}

/// Creates a datagram queue that holds at most `capacity` items, with one sender and one receiver.
///
/// Clone the sender for every reader, and the receiver for every decoder.
pub(super) fn datagram_queue<T>(capacity: usize) -> (DatagramSender<T>, DatagramReceiver<T>) {
    let shared = Arc::new(Shared {
        items: SegQueue::new(),
        capacity,
        len: AtomicUsize::new(0),
        readable: Notify::new(),
        wake_pending: AtomicBool::new(false),
        boosted: AtomicBool::new(false),
        writable: Notify::new(),
        blocked_senders: AtomicUsize::new(0),
        senders: AtomicUsize::new(1),
        receivers: AtomicUsize::new(1),
    });
    (
        DatagramSender {
            shared: Arc::clone(&shared),
            backlog: AtomicUsize::new(0),
        },
        DatagramReceiver { shared },
    )
}

/// Returns the most bytes a datagram queue of `capacity` items of `T` holds, when it is full.
///
/// The queue allocates blocks of 31 slots, each the size of an item and its state word, as items arrive, and frees
/// each block once its items are taken, keeping at most one block that is not full at either end.
pub(super) const fn bytes_when_full<T>(capacity: usize) -> usize {
    const SLOTS_PER_BLOCK: usize = 31;
    let block = size_of::<usize>() + SLOTS_PER_BLOCK * (size_of::<T>() + size_of::<usize>());
    (capacity.div_ceil(SLOTS_PER_BLOCK) + 1).saturating_mul(block)
}

/// The sending side of a datagram queue.
pub(super) struct DatagramSender<T> {
    shared: Arc<Shared<T>>,

    /// The bytes this sender queued since it last woke a receiver or one more; only this sender writes it.
    backlog: AtomicUsize,
}

impl<T> DatagramSender<T> {
    /// Queues `item`, as [`send_sized`][Self::send_sized] does, as an item of no size.
    #[cfg(test)]
    pub(super) async fn send(&self, item: T) -> Result<(), T> {
        self.send_sized(item, 0).await
    }

    /// Queues `item`, of `bytes` bytes, waiting for room while the queue is full.
    ///
    /// While a receiver this sender woke has not run yet, the sender counts the bytes it queues, and past
    /// `BOOST_BACKLOG_BYTES` wakes one more receiver (see the module's documentation).
    ///
    /// # Errors
    ///
    /// If every receiver has been dropped, the item is given back.
    ///
    /// # Cancel safety
    ///
    /// Cancel-safe: if the future is dropped before it completes, the item was not queued, and a wake-up it received
    /// passes to another waiting sender.
    pub(super) async fn send_sized(&self, item: T, bytes: usize) -> Result<(), T> {
        // As the scheduler's channels do: a send spends a unit of the task's budget, and yields once it is spent.
        cooperative(self.send_uncooperative(item, bytes)).await
    }

    async fn send_uncooperative(&self, mut item: T, bytes: usize) -> Result<(), T> {
        let shared = &*self.shared;
        loop {
            if shared.receivers.load(Ordering::Acquire) == 0 {
                return Err(item);
            }
            item = match shared.try_push(item) {
                Ok(woke) => {
                    self.queued(woke, bytes);
                    return Ok(());
                }
                Err(item) => item,
            };

            // Register for the next wake-up, then check again: a receiver wakes a sender only while one is counted as
            // waiting, and the last receiver leaving wakes only registered waiters.
            let mut writable = pin!(shared.writable.notified());
            writable.as_mut().enable();
            let _waiting = BlockedSender::register(shared);
            if shared.receivers.load(Ordering::Acquire) == 0 {
                return Err(item);
            }
            item = match shared.try_push(item) {
                Ok(woke) => {
                    self.queued(woke, bytes);
                    return Ok(());
                }
                Err(item) => item,
            };
            writable.await;
        }
    }

    /// Counts an item of `bytes` bytes queued, which `woke` a receiver or not, toward this sender's backlog.
    fn queued(&self, woke: bool, bytes: usize) {
        if woke {
            self.backlog.store(0, Ordering::Relaxed);
            return;
        }
        let backlog = self.backlog.load(Ordering::Relaxed).saturating_add(bytes);
        if backlog > BOOST_BACKLOG_BYTES {
            self.backlog.store(0, Ordering::Relaxed);
            self.shared.boost();
        } else {
            self.backlog.store(backlog, Ordering::Relaxed);
        }
    }
}

/// A sender counted as waiting for room, until it is dropped.
struct BlockedSender<'a, T> {
    shared: &'a Shared<T>,
}

impl<'a, T> BlockedSender<'a, T> {
    fn register(shared: &'a Shared<T>) -> Self {
        shared.blocked_senders.fetch_add(1, Ordering::Relaxed);
        // Orders the registration before the sender tries again: see `Shared::try_pop`.
        fence(Ordering::SeqCst);
        Self { shared }
    }
}

impl<T> Drop for BlockedSender<'_, T> {
    fn drop(&mut self) {
        self.shared.blocked_senders.fetch_sub(1, Ordering::Relaxed);
    }
}

impl<T> Clone for DatagramSender<T> {
    fn clone(&self) -> Self {
        self.shared.senders.fetch_add(1, Ordering::Relaxed);
        Self {
            shared: Arc::clone(&self.shared),
            backlog: AtomicUsize::new(0),
        }
    }
}

impl<T> Drop for DatagramSender<T> {
    fn drop(&mut self) {
        if self.shared.senders.fetch_sub(1, Ordering::AcqRel) == 1 {
            // The last sender: every waiting receiver must see the end.
            self.shared.readable.notify_waiters();
        }
    }
}

/// The receiving side of a datagram queue.
pub(super) struct DatagramReceiver<T> {
    shared: Arc<Shared<T>>,
}

impl<T> DatagramReceiver<T> {
    /// Takes the next item, waiting for one while the queue is empty.
    ///
    /// Returns `None` once every sender has been dropped and the queue is empty.
    ///
    /// # Cancel safety
    ///
    /// Cancel-safe: if the future is dropped before it completes, no item was taken, and a wake-up it received passes
    /// to another waiting receiver.
    pub(super) async fn recv(&self) -> Option<T> {
        // As the scheduler's channels do: a receive spends a unit of the task's budget, and yields once it is spent.
        cooperative(self.recv_uncooperative()).await
    }

    async fn recv_uncooperative(&self) -> Option<T> {
        let shared = &*self.shared;
        loop {
            if let Some(item) = shared.try_pop() {
                return Some(item);
            }

            // Register for the next wake-up, then check again: the last sender leaving wakes only registered waiters,
            // so the check for it must follow the registration. If registering took a wake-up left for the next
            // receiver to wait (an item was queued while none waited), this receiver holds the pending wake-up, and
            // gives it back before taking the item. (A push between the empty pop and the registration either left
            // that wake-up or found one pending, whose receiver takes the item; popping again takes it at once.)
            let mut readable = pin!(shared.readable.notified());
            if readable.as_mut().enable() {
                shared.take_wake_up();
            }
            if let Some(item) = shared.try_pop() {
                // A wake-up given to this receiver since it registered passes to another waiting receiver when the
                // registration is dropped, and stays pending for it.
                return Some(item);
            }
            if shared.senders.load(Ordering::Acquire) == 0 {
                // Every item queued before the last sender left is visible now, since that sender's push happened
                // before its drop.
                return shared.try_pop();
            }
            readable.await;
            shared.take_wake_up();
        }
    }
}

#[cfg(test)]
impl<T> DatagramReceiver<T> {
    /// Returns the number of items queued.
    pub(super) fn len(&self) -> usize {
        self.shared.items.len()
    }
}

impl<T> Clone for DatagramReceiver<T> {
    fn clone(&self) -> Self {
        self.shared.receivers.fetch_add(1, Ordering::Relaxed);
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<T> Drop for DatagramReceiver<T> {
    fn drop(&mut self) {
        if self.shared.receivers.fetch_sub(1, Ordering::AcqRel) == 1 {
            // The last receiver: every waiting sender must see that nothing will take its item.
            self.shared.writable.notify_waiters();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        future::Future,
        pin::Pin,
        sync::atomic::AtomicUsize,
        task::{Context, Poll, Wake, Waker},
        time::Duration,
    };

    use proptest::prelude::*;
    use tokio::{task::yield_now, time::timeout};

    use super::*;

    const PATIENCE: Duration = Duration::from_secs(10);

    /// A waker that counts its wake-ups.
    #[derive(Default)]
    struct CountingWaker(AtomicUsize);

    impl CountingWaker {
        fn woken(&self) -> usize {
            self.0.load(Ordering::SeqCst)
        }
    }

    impl Wake for CountingWaker {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Polls `future` once with `waker`.
    fn poll_once<F: Future>(future: Pin<&mut F>, waker: &Arc<CountingWaker>) -> Poll<F::Output> {
        future.poll(&mut Context::from_waker(&Waker::from(Arc::clone(waker))))
    }

    /// Queues `item`, of `bytes` bytes, which must not have to wait for room.
    fn send_sized_now<T>(sender: &DatagramSender<T>, item: T, bytes: usize) {
        let waker = Arc::new(CountingWaker::default());
        let sending = pin!(sender.send_sized(item, bytes));
        assert!(matches!(poll_once(sending, &waker), Poll::Ready(Ok(()))));
    }

    /// Queues `item`, of no size, which must not have to wait for room.
    fn send_now<T>(sender: &DatagramSender<T>, item: T) {
        send_sized_now(sender, item, 0);
    }

    #[test]
    fn a_burst_of_items_wakes_one_waiting_receiver_and_the_next_item_after_it_runs_wakes_another() {
        let (sender, receiver) = datagram_queue(8);
        let receivers = [receiver.clone(), receiver.clone(), receiver];
        let wakers = [(); 3].map(|_| Arc::new(CountingWaker::default()));
        let mut waiting = receivers.each_ref().map(|receiver| Box::pin(receiver.recv()));
        for (recv, waker) in waiting.iter_mut().zip(&wakers) {
            assert!(poll_once(recv.as_mut(), waker).is_pending());
        }

        // Three items queued before any receiver runs wake only the receiver that waited last: when it runs, it takes
        // what is queued, and the others stay asleep.
        for item in 0..3 {
            send_now(&sender, item);
        }
        assert_eq!(wakers.each_ref().map(|waker| waker.woken()), [0, 0, 1]);
        assert_eq!(poll_once(waiting[2].as_mut(), &wakers[2]), Poll::Ready(Some(0)));

        // Once it has run, the next item wakes another, while it decodes what it took.
        send_now(&sender, 3);
        assert_eq!(wakers.each_ref().map(|waker| waker.woken()), [0, 1, 1]);
        assert_eq!(poll_once(waiting[1].as_mut(), &wakers[1]), Poll::Ready(Some(1)));
        send_now(&sender, 4);
        assert_eq!(wakers.each_ref().map(|waker| waker.woken()), [1, 1, 1]);
        assert_eq!(poll_once(waiting[0].as_mut(), &wakers[0]), Poll::Ready(Some(2)));
    }

    #[test]
    fn a_backlog_queued_while_the_woken_receiver_has_not_run_wakes_one_more() {
        let (sender, receiver) = datagram_queue(16);
        let receivers = [receiver.clone(), receiver.clone(), receiver];
        let wakers = [(); 3].map(|_| Arc::new(CountingWaker::default()));
        let mut waiting = receivers.each_ref().map(|receiver| Box::pin(receiver.recv()));
        for (recv, waker) in waiting.iter_mut().zip(&wakers) {
            assert!(poll_once(recv.as_mut(), waker).is_pending());
        }
        let woken = || wakers.each_ref().map(|waker| waker.woken());

        // The first datagram wakes a receiver; while it has not run, datagrams queue up to the threshold without
        // waking anyone, and the one past it wakes one more.
        let half = BOOST_BACKLOG_BYTES / 2 + 1;
        send_sized_now(&sender, 0, half);
        send_sized_now(&sender, 1, half);
        assert_eq!(woken(), [0, 0, 1]);
        send_sized_now(&sender, 2, half);
        assert_eq!(woken(), [0, 1, 1], "a backlog wakes one more");

        // Only one more, until a woken receiver runs.
        for item in 3..6 {
            send_sized_now(&sender, item, half);
        }
        assert_eq!(woken(), [0, 1, 1]);
        assert_eq!(poll_once(waiting[2].as_mut(), &wakers[2]), Poll::Ready(Some(0)));
        send_now(&sender, 6);
        assert_eq!(
            woken(),
            [1, 1, 1],
            "once a woken receiver ran, the next datagram wakes another"
        );
    }

    #[test]
    fn a_wake_up_left_while_no_receiver_waits_goes_to_the_next_receiver_to_wait() {
        let (sender, receiver) = datagram_queue(8);
        let other = receiver.clone();
        let waker = Arc::new(CountingWaker::default());

        // An item queued while no receiver waits leaves the wake-up for the next one; a receiver that does not wait
        // takes the item, and the next receiver to wait finds the wake-up, gives it back, and waits.
        send_now(&sender, 1);
        let mut taking = Box::pin(receiver.recv());
        assert_eq!(poll_once(taking.as_mut(), &waker), Poll::Ready(Some(1)));
        let mut waiting = Box::pin(other.recv());
        assert!(poll_once(waiting.as_mut(), &waker).is_pending());

        // So the next item wakes it.
        send_now(&sender, 2);
        assert_eq!(waker.woken(), 1);
        assert_eq!(poll_once(waiting.as_mut(), &waker), Poll::Ready(Some(2)));
    }

    #[tokio::test]
    async fn a_receiver_taking_item_after_item_yields_to_other_tasks_now_and_then() {
        // As a decoder does with a queue that never empties: without yielding, it would keep its thread from the
        // other tasks, such as the reader, for as long as items keep coming.
        let (sender, receiver) = datagram_queue(1_000);
        for item in 0..1_000 {
            sender.send(item).await.unwrap();
        }
        let other_ran = Arc::new(AtomicBool::new(false));
        tokio::spawn({
            let other_ran = Arc::clone(&other_ran);
            async move { other_ran.store(true, Ordering::SeqCst) }
        });
        for item in 0..1_000 {
            assert_eq!(receiver.recv().await, Some(item));
        }
        assert!(
            other_ran.load(Ordering::SeqCst),
            "the receiver yielded to the other task"
        );
    }

    #[test]
    fn a_queue_allocates_room_as_items_arrive_whatever_its_capacity() {
        // A queue that allocated its capacity up front could not be created at all.
        let (sender, receiver) = datagram_queue::<[u8; 4096]>(usize::MAX / 2);
        send_now(&sender, [7; 4096]);
        assert_eq!(receiver.len(), 1);
        let waker = Arc::new(CountingWaker::default());
        assert_eq!(poll_once(pin!(receiver.recv()), &waker), Poll::Ready(Some([7; 4096])));
    }

    #[tokio::test]
    async fn one_receiver_takes_every_item_in_order_and_sees_the_end_once_the_senders_are_gone() {
        let (sender, receiver) = datagram_queue(4);
        let second_sender = sender.clone();
        for item in 0..3 {
            sender.send(item).await.unwrap();
        }
        drop(sender);
        second_sender.send(3).await.unwrap();
        assert_eq!(receiver.recv().await, Some(0));
        drop(second_sender);

        // The senders are gone, and what they queued is still taken before the end.
        for item in 1..4 {
            assert_eq!(receiver.recv().await, Some(item));
        }
        assert_eq!(receiver.recv().await, None);
        assert_eq!(receiver.recv().await, None, "the end stays the end");
    }

    #[tokio::test]
    async fn a_waiting_receiver_wakes_for_an_item_and_for_the_end() {
        let (sender, receiver) = datagram_queue::<u32>(2);
        let other = receiver.clone();
        let waiting = tokio::spawn(async move { other.recv().await });
        yield_now().await;
        sender.send(7).await.unwrap();
        assert_eq!(timeout(PATIENCE, waiting).await.unwrap().unwrap(), Some(7));

        let other = receiver.clone();
        let waiting = tokio::spawn(async move { other.recv().await });
        yield_now().await;
        drop(sender);
        assert_eq!(timeout(PATIENCE, waiting).await.unwrap().unwrap(), None);
    }

    #[tokio::test]
    async fn a_sender_waits_while_the_queue_is_full_and_gets_its_item_back_once_no_receiver_is_left() {
        let (sender, receiver) = datagram_queue(1);
        sender.send(1).await.unwrap();
        let blocked = sender.clone();
        let mut waiting = tokio::spawn(async move { blocked.send(2).await });
        yield_now().await;
        assert!(
            timeout(Duration::from_millis(20), &mut waiting).await.is_err(),
            "a full queue holds the sender back"
        );
        assert_eq!(receiver.recv().await, Some(1));
        assert_eq!(
            timeout(PATIENCE, waiting).await.unwrap().unwrap(),
            Ok(()),
            "taking an item makes room"
        );
        assert_eq!(receiver.recv().await, Some(2));

        // With the queue full again, dropping the last receiver releases a waiting sender with its item.
        sender.send(3).await.unwrap();
        let blocked = sender.clone();
        let waiting = tokio::spawn(async move { blocked.send(4).await });
        yield_now().await;
        drop(receiver);
        assert_eq!(timeout(PATIENCE, waiting).await.unwrap().unwrap(), Err(4));
        assert_eq!(sender.send(5).await, Err(5));
    }

    #[tokio::test]
    async fn a_wake_up_given_to_a_receiver_that_stops_waiting_passes_to_another() {
        // The item wakes one of two waiting receivers, and one of them stops waiting before it runs: whichever was
        // woken, the other takes the item. Once with each of them stopping, so that one of the two runs has the woken
        // receiver stop.
        for stop_first in [false, true] {
            let (sender, receiver) = datagram_queue(2);
            let (first, second) = (receiver.clone(), receiver.clone());
            let first = tokio::spawn(async move { first.recv().await });
            yield_now().await;
            let second = tokio::spawn(async move { second.recv().await });
            yield_now().await;

            sender.send(9).await.unwrap();
            let (stopping, remaining) = if stop_first { (first, second) } else { (second, first) };
            stopping.abort();
            assert!(stopping.await.unwrap_err().is_cancelled());
            assert_eq!(timeout(PATIENCE, remaining).await.unwrap().unwrap(), Some(9));
        }
    }

    /// Sends `items` items from `senders` senders through a queue of `capacity` to `receivers` receivers on a runtime
    /// of four threads; each receiver also stops waiting at its first `ticks` ticks, 50 µs apart, as a decoder does at
    /// its flush tick, and then waits for wake-ups alone. Every item must be taken while the senders are still there,
    /// so that a lost wake-up leaves an item queued: the test then times out. Returns what each receiver took, once
    /// the senders are gone and every receiver saw the end.
    fn deliver(capacity: usize, senders: usize, receivers: usize, items: usize, ticks: usize) -> Vec<Vec<usize>> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let (sender, receiver) = datagram_queue(capacity);
            let taken_count = Arc::new(AtomicUsize::new(0));
            let sending = (0..senders)
                .map(|index| {
                    let sender = sender.clone();
                    tokio::spawn(async move {
                        for item in (index..items).step_by(senders) {
                            sender.send(item).await.unwrap();
                        }
                    })
                })
                .collect::<Vec<_>>();
            let receiving = (0..receivers)
                .map(|_| {
                    let receiver = receiver.clone();
                    let taken_count = Arc::clone(&taken_count);
                    tokio::spawn(async move {
                        let mut taken = Vec::new();
                        let mut tick = tokio::time::interval(Duration::from_micros(50));
                        let mut ticks_left = ticks;
                        loop {
                            let item = if ticks_left > 0 {
                                tokio::select! {
                                    item = receiver.recv() => item,
                                    _ = tick.tick() => {
                                        ticks_left -= 1;
                                        continue;
                                    }
                                }
                            } else {
                                receiver.recv().await
                            };
                            match item {
                                Some(item) => {
                                    taken.push(item);
                                    taken_count.fetch_add(1, Ordering::SeqCst);
                                }
                                None => break,
                            }
                        }
                        taken
                    })
                })
                .collect::<Vec<_>>();
            drop(receiver);
            for task in sending {
                timeout(PATIENCE, task).await.expect("every send completes").unwrap();
            }
            timeout(PATIENCE, async {
                while taken_count.load(Ordering::SeqCst) < items {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .expect("every item is taken while the senders are there, without waiting for the end");
            drop(sender);
            let mut taken = Vec::new();
            for task in receiving {
                taken.push(
                    timeout(PATIENCE, task)
                        .await
                        .expect("every receiver sees the end")
                        .unwrap(),
                );
            }
            taken
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn property_test_every_item_is_taken_exactly_once_and_in_order_per_sender(
            capacity in 1usize..8,
            senders in 1usize..4,
            receivers in 1usize..5,
            items in 0usize..3_000,
            ticks in 0usize..20,
        ) {
            let taken = deliver(capacity, senders, receivers, items, ticks);
            let all = taken.iter().flatten().copied().collect::<Vec<_>>();
            prop_assert_eq!(all.len(), items, "nothing is lost or taken twice");
            prop_assert_eq!(all.iter().copied().collect::<BTreeSet<_>>(), (0..items).collect::<BTreeSet<_>>());

            // One receiver takes one sender's items in the order they were sent.
            for received in &taken {
                for sender in 0..senders {
                    let from = received.iter().filter(|item| *item % senders == sender).collect::<Vec<_>>();
                    prop_assert!(from.windows(2).all(|pair| pair[0] < pair[1]));
                }
            }
        }
    }
}

// Loom models of the queue's protocols: the wake-ups of receivers (one pending at a time), the room a full queue
// makes for waiting senders, and the close. The models transcribe them with loom primitives, since loom instruments
// neither tokio's `Notify` nor crossbeam's queue:
//
// - `Notify` is transcribed with a loom mutex and condition variable, as the queue uses it: `enable` takes a stored
//   permit or registers the waiter; `notify_last` and `notify_one` notify the last or first registered waiter, or
//   store a permit when none waits; `notify_waiters` notifies every registered waiter; a registered waiter that stops
//   waiting passes a `notify_last` or `notify_one` it received to the next waiter, or stores it as a permit, as tokio
//   does; a waiter blocks until it is notified. A lost wake-up leaves every thread blocked, which loom reports as a
//   deadlock. (The model is shared through `std`'s `Arc`, whose drop loom does not track: dropping a `loom` `Arc`
//   while loom unwinds from a deadlock panics again, which aborts the test.)
// - The list of items is a count, as an abstract queue: a pop reads it with no ordering of its own (crossbeam's pops
//   add a fence, which the protocols do not rely on), and takes an item by compare-and-swap; a push adds to it.
//
// The models leave out the backlog's extra wake-up (`DatagramSender::queued` and `Shared::boost`), which only adds a
// notification and clears nothing a waiter relies on, and the cooperative budget, which only delays a poll.
// `Model::{try_push, try_pop, take_wake_up, send, recv}` and the drops must stay in lockstep with the code above.
// `Variant` transcribes orders the protocols forbid, which the models reject.
#[cfg(all(test, feature = "loom"))]
mod loom_tests {
    use std::sync::Arc;

    use loom::{
        sync::{
            atomic::{fence, AtomicBool, AtomicUsize, Ordering},
            Condvar, Mutex,
        },
        thread,
    };

    /// How a waiter registered with a transcribed `Notify` was notified.
    #[derive(Clone, Copy, PartialEq)]
    enum Notified {
        Waiting,
        /// By `notify_last` or `notify_one`: passed on if the waiter stops waiting.
        One,
        /// By `notify_waiters`.
        All,
    }

    #[derive(Default)]
    struct NotifyState {
        permit: bool,
        waiters: Vec<(usize, Notified)>,
    }

    /// tokio's `Notify`, transcribed for the operations the queue uses.
    #[derive(Default)]
    struct Notify {
        state: Mutex<NotifyState>,
        changed: Condvar,
    }

    impl Notify {
        /// `notified()` and `enable()`: returns `true` if it took a stored permit, and registers waiter `id` otherwise.
        fn enable(&self, id: usize) -> bool {
            let mut state = self.state.lock().unwrap();
            if std::mem::take(&mut state.permit) {
                return true;
            }
            state.waiters.push((id, Notified::Waiting));
            false
        }

        fn notify_locked(&self, state: &mut NotifyState, last: bool) {
            let mut waiting = state
                .waiters
                .iter_mut()
                .filter(|(_, notified)| *notified == Notified::Waiting);
            let chosen = if last { waiting.last() } else { waiting.next() };
            match chosen {
                Some((_, notified)) => {
                    *notified = Notified::One;
                    self.changed.notify_all();
                }
                None => state.permit = true,
            }
        }

        fn notify_last(&self) {
            self.notify_locked(&mut self.state.lock().unwrap(), true);
        }

        fn notify_one(&self) {
            self.notify_locked(&mut self.state.lock().unwrap(), false);
        }

        fn notify_waiters(&self) {
            let mut state = self.state.lock().unwrap();
            for (_, notified) in state.waiters.iter_mut() {
                if *notified == Notified::Waiting {
                    *notified = Notified::All;
                }
            }
            self.changed.notify_all();
        }

        /// The `.await` of registered waiter `id`: returns once it is notified.
        fn wait(&self, id: usize) {
            let mut state = self.state.lock().unwrap();
            loop {
                let position = state.waiters.iter().position(|(waiter, _)| *waiter == id).unwrap();
                if state.waiters[position].1 != Notified::Waiting {
                    state.waiters.remove(position);
                    return;
                }
                state = self.changed.wait(state).unwrap();
            }
        }

        /// The drop of registered waiter `id` before its `.await` completed: a `notify_last` (`last`) or `notify_one`
        /// it received passes on.
        fn stop_waiting(&self, id: usize, last: bool) {
            let mut state = self.state.lock().unwrap();
            let position = state.waiters.iter().position(|(waiter, _)| *waiter == id).unwrap();
            let (_, notified) = state.waiters.remove(position);
            if notified == Notified::One {
                self.notify_locked(&mut state, last);
            }
        }
    }

    /// The protocols as the queue follows them, or an order they forbid.
    #[derive(Clone, Copy, PartialEq)]
    enum Variant {
        /// As the queue does.
        Queue,
        /// A push reads `wake_pending` without ordering its push before the read.
        NoPushFence,
        /// A receiver that runs pops without ordering its clearing of `wake_pending` before the pop.
        NoClearFence,
        /// A receiver that runs after a wake-up leaves `wake_pending` set (only registering clears it).
        NoClearOnWake,
        /// A receiver whose registration took a stored wake-up leaves `wake_pending` set.
        NoClearOnEnable,
        /// A receiver that sees no sender left returns the end without popping once more.
        NoFinalPop,
        /// A pop reads the waiting senders without ordering the room it made before the read.
        NoPopFence,
    }

    /// What a receiver's `recv` returns.
    #[derive(PartialEq)]
    enum Received {
        Item,
        End,
        /// It stopped waiting once registered, as a decoder does at its tick.
        Stopped,
    }

    struct Model {
        variant: Variant,
        items: AtomicUsize,
        capacity: usize,
        len: AtomicUsize,
        readable: Notify,
        wake_pending: AtomicBool,
        writable: Notify,
        blocked_senders: AtomicUsize,
        senders: AtomicUsize,
        receivers: AtomicUsize,
        /// The items taken, which the model's main thread can wait for.
        taken: Mutex<usize>,
        taken_changed: Condvar,
    }

    impl Model {
        fn new(variant: Variant, capacity: usize, senders: usize, receivers: usize) -> Self {
            Self {
                variant,
                items: AtomicUsize::new(0),
                capacity,
                len: AtomicUsize::new(0),
                readable: Notify::default(),
                wake_pending: AtomicBool::new(false),
                writable: Notify::default(),
                blocked_senders: AtomicUsize::new(0),
                senders: AtomicUsize::new(senders),
                receivers: AtomicUsize::new(receivers),
                taken: Mutex::new(0),
                taken_changed: Condvar::new(),
            }
        }

        fn taken(&self) -> usize {
            *self.taken.lock().unwrap()
        }

        /// Blocks until `count` items were taken.
        fn wait_for_taken(&self, count: usize) {
            let mut taken = self.taken.lock().unwrap();
            while *taken < count {
                taken = self.taken_changed.wait(taken).unwrap();
            }
        }

        fn pop_item(&self) -> bool {
            loop {
                let items = self.items.load(Ordering::Relaxed);
                if items == 0 {
                    return false;
                }
                if self
                    .items
                    .compare_exchange(items, items - 1, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    return true;
                }
            }
        }

        fn try_push(&self) -> bool {
            if self.len.fetch_add(1, Ordering::Relaxed) >= self.capacity {
                self.len.fetch_sub(1, Ordering::Relaxed);
                return false;
            }
            self.items.fetch_add(1, Ordering::AcqRel);
            if self.variant != Variant::NoPushFence {
                fence(Ordering::SeqCst);
            }
            if !self.wake_pending.load(Ordering::Relaxed) && !self.wake_pending.swap(true, Ordering::Relaxed) {
                self.readable.notify_last();
            }
            true
        }

        fn try_pop(&self) -> bool {
            if !self.pop_item() {
                return false;
            }
            self.len.fetch_sub(1, Ordering::Relaxed);
            if self.variant != Variant::NoPopFence {
                fence(Ordering::SeqCst);
            }
            if self.blocked_senders.load(Ordering::Relaxed) > 0 {
                self.writable.notify_one();
            }
            *self.taken.lock().unwrap() += 1;
            self.taken_changed.notify_all();
            true
        }

        fn take_wake_up(&self) {
            self.wake_pending.store(false, Ordering::Relaxed);
            if self.variant != Variant::NoClearFence {
                fence(Ordering::SeqCst);
            }
        }

        /// `DatagramSender::send` from sender `id`: returns whether the item was queued.
        fn send(&self, id: usize) -> bool {
            loop {
                if self.receivers.load(Ordering::Acquire) == 0 {
                    return false;
                }
                if self.try_push() {
                    return true;
                }
                let took = self.writable.enable(id);
                self.blocked_senders.fetch_add(1, Ordering::Relaxed);
                fence(Ordering::SeqCst);
                let done = if self.receivers.load(Ordering::Acquire) == 0 {
                    Some(false)
                } else if self.try_push() {
                    Some(true)
                } else {
                    None
                };
                if let Some(queued) = done {
                    if !took {
                        self.writable.stop_waiting(id, false);
                    }
                    self.blocked_senders.fetch_sub(1, Ordering::Relaxed);
                    return queued;
                }
                if !took {
                    self.writable.wait(id);
                }
                self.blocked_senders.fetch_sub(1, Ordering::Relaxed);
            }
        }

        /// `DatagramReceiver::recv` from receiver `id`; with `stop`, it stops waiting once registered.
        fn recv(&self, id: usize, stop: bool) -> Received {
            loop {
                if self.try_pop() {
                    return Received::Item;
                }
                let took = self.readable.enable(id);
                if took && self.variant != Variant::NoClearOnEnable {
                    self.take_wake_up();
                }
                if self.try_pop() {
                    if !took {
                        self.readable.stop_waiting(id, true);
                    }
                    return Received::Item;
                }
                if self.senders.load(Ordering::Acquire) == 0 {
                    if !took {
                        self.readable.stop_waiting(id, true);
                    }
                    let last = self.variant != Variant::NoFinalPop && self.try_pop();
                    return if last { Received::Item } else { Received::End };
                }
                if !took {
                    if stop {
                        self.readable.stop_waiting(id, true);
                        return Received::Stopped;
                    }
                    self.readable.wait(id);
                }
                if self.variant != Variant::NoClearOnWake {
                    self.take_wake_up();
                }
            }
        }

        fn drop_sender(&self) {
            if self.senders.fetch_sub(1, Ordering::AcqRel) == 1 {
                self.readable.notify_waiters();
            }
        }

        fn drop_receiver(&self) {
            if self.receivers.fetch_sub(1, Ordering::AcqRel) == 1 {
                self.writable.notify_waiters();
            }
        }
    }

    /// Runs a receiver that takes items until it sees the end, stopping waiting once (as at a tick) if `stops`.
    fn receive(model: &Model, id: usize, mut stops: bool) {
        while model.recv(id, std::mem::take(&mut stops)) != Received::End {}
        model.drop_receiver();
    }

    fn check(f: impl Fn() + Sync + Send + 'static) {
        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = Some(3);
        builder.check(f);
    }

    /// One sender queues two items, and leaves only once both are taken; `receivers` receivers take them, the first
    /// stopping waiting once if `stops`. A lost wake-up leaves an item queued while every receiver sleeps and the
    /// sender waits: loom reports the deadlock.
    fn every_item_wakes_a_receiver(variant: Variant, receivers: usize, stops: bool) {
        check(move || {
            let model = Arc::new(Model::new(variant, 4, 1, receivers));
            let receiving = (0..receivers)
                .map(|id| {
                    let model = Arc::clone(&model);
                    thread::spawn(move || receive(&model, id, stops && id == 0))
                })
                .collect::<Vec<_>>();
            for _ in 0..2 {
                assert!(model.send(10));
            }
            model.wait_for_taken(2);
            model.drop_sender();
            for receiver in receiving {
                receiver.join().unwrap();
            }
        });
    }

    /// One sender queues two items and leaves at once; one receiver takes what is there, then sees the end. Every item
    /// queued is taken before the end.
    fn the_end_comes_after_every_item(variant: Variant) {
        check(move || {
            let model = Arc::new(Model::new(variant, 4, 1, 1));
            let receiver = {
                let model = Arc::clone(&model);
                thread::spawn(move || receive(&model, 0, false))
            };
            let queued = (0..2).filter(|_| model.send(10)).count();
            model.drop_sender();
            receiver.join().unwrap();
            assert_eq!(model.taken(), queued, "an item queued before the end was lost");
        });
    }

    /// Two senders queue an item each through a queue of one; one receiver takes both. A sender waiting for room is
    /// woken when the receiver makes it: otherwise loom reports the deadlock.
    fn a_full_queue_wakes_a_waiting_sender(variant: Variant) {
        check(move || {
            let model = Arc::new(Model::new(variant, 1, 2, 1));
            let senders = [10, 11].map(|id| {
                let model = Arc::clone(&model);
                thread::spawn(move || {
                    assert!(model.send(id));
                    model.drop_sender();
                })
            });
            receive(&model, 0, false);
            for sender in senders {
                sender.join().unwrap();
            }
            assert_eq!(model.taken(), 2);
        });
    }

    #[test]
    fn every_item_queued_wakes_one_of_two_receivers_one_of_which_stops_waiting_once() {
        every_item_wakes_a_receiver(Variant::Queue, 2, true);
    }

    #[test]
    fn every_item_queued_wakes_the_receiver() {
        every_item_wakes_a_receiver(Variant::Queue, 1, false);
    }

    #[test]
    fn every_item_queued_before_the_end_is_taken() {
        the_end_comes_after_every_item(Variant::Queue);
    }

    #[test]
    fn a_receiver_making_room_wakes_a_waiting_sender() {
        a_full_queue_wakes_a_waiting_sender(Variant::Queue);
    }

    // Without the fence between a push and its read of `wake_pending`, the push can miss the flag's clearing while the
    // receiver that cleared it misses the item: loom finds the lost wake-up.
    #[test]
    #[should_panic(expected = "deadlock")]
    fn a_push_unordered_before_its_read_of_the_flag_loses_a_wake_up() {
        every_item_wakes_a_receiver(Variant::NoPushFence, 1, false);
    }

    // Likewise without the fence between a receiver's clearing of the flag and its next pop.
    #[test]
    #[should_panic(expected = "deadlock")]
    fn a_clearing_unordered_before_the_next_pop_loses_a_wake_up() {
        every_item_wakes_a_receiver(Variant::NoClearFence, 1, false);
    }

    // A woken receiver that leaves the flag set holds the wake-up while it runs: the next item wakes nobody.
    #[test]
    #[should_panic(expected = "deadlock")]
    fn a_woken_receiver_that_keeps_the_flag_loses_a_wake_up() {
        every_item_wakes_a_receiver(Variant::NoClearOnWake, 1, false);
    }

    // A receiver whose registration took the wake-up an item left while no receiver waited, and which then takes an
    // item without waiting, holds the wake-up if it leaves the flag set: the next item wakes nobody.
    #[test]
    #[should_panic(expected = "deadlock")]
    fn a_receiver_that_keeps_the_flag_after_taking_a_stored_wake_up_loses_a_wake_up() {
        every_item_wakes_a_receiver(Variant::NoClearOnEnable, 1, false);
    }

    // A receiver that sees no sender left and returns the end without popping once more can miss the last item.
    #[test]
    #[should_panic(expected = "an item queued before the end was lost")]
    fn ending_without_a_last_pop_loses_an_item() {
        the_end_comes_after_every_item(Variant::NoFinalPop);
    }

    // Without the fence between a pop and its read of the waiting senders, a sender can miss the room while the pop
    // misses the sender: loom finds the sender waiting forever.
    #[test]
    #[should_panic(expected = "deadlock")]
    fn a_pop_unordered_before_its_read_of_the_waiting_senders_loses_a_sender() {
        a_full_queue_wakes_a_waiting_sender(Variant::NoPopFence);
    }
}
