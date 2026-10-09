//! Single-publisher, many-subscriber shared-memory broadcast transport.
//!
//! One payload ring is shared by every active subscriber; each subscriber owns
//! an independent read cursor. Every accepted record is written once and is
//! visible to every active subscriber in publication order. Subscribers may
//! join and leave while the publisher is running.
//!
//! Backpressure is blocking: when the ring is full the publisher waits for the
//! limiting subscriber instead of dropping records. A stopped subscriber can
//! therefore stall publication indefinitely. That coupling is the selected PoC
//! policy; there is no eviction, heartbeat, or automatic recovery.

use crate::cancellation::{CancellationToken, WakePolicy};
use crate::contract::{
    BROADCAST_SLOTS_OFFSET, BROADCAST_SLOT_ACTIVE, BROADCAST_SLOT_FREE, BROADCAST_SLOT_GENERATION,
    BROADCAST_SLOT_PRODUCER_WAITING, BROADCAST_SLOT_READ_CURSOR, BROADCAST_SLOT_RESERVED, BROADCAST_SLOT_RETIRING,
    BROADCAST_SLOT_STATE, BROADCAST_SLOT_STRIDE, BROADCAST_WRITE_OFFSET,
};
use crate::mapping::Shared;
use crate::ring::{Record, Rejection, RECORD_HEADER, RESERVED_GAP};
use crate::wait::{wait, wake, wake_all};
use crate::{invalid, ProtocolDescriptor};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// Poll interval used while a cancellable publisher has no active subscribers.
const WAIT_POLL: Duration = Duration::from_millis(10);

/// Outcome of a broadcast send. It always carries the number of records already
/// published, even when the call stopped for cancellation or a fatal failure.
#[derive(Debug)]
pub struct BroadcastOutcome {
    /// Records published by this call. Accepted means published, not decoded or
    /// handled. A caller cannot retry the whole batch after a partial result.
    pub accepted: usize,
    /// First input rejection that stopped the batch, if any.
    pub rejection: Option<Rejection>,
    /// Wake failure after publication. The accepted prefix stays published.
    pub notification_error: Option<io::Error>,
    /// The local supervisor cancelled the send before it finished.
    pub cancelled: bool,
    /// A fatal transport error. `accepted` records are already published.
    pub failure: Option<io::Error>,
}

impl BroadcastOutcome {
    fn empty() -> Self {
        Self {
            accepted: 0,
            rejection: None,
            notification_error: None,
            cancelled: false,
            failure: None,
        }
    }
}

/// Publisher-local registry and cached reclamation budget. It lives only on the
/// publisher and is never mapped into shared memory.
#[derive(Default)]
pub(crate) struct Registry {
    /// Dense list of active slot IDs. Iterated on the publication path.
    pub(crate) active_slots: Vec<u32>,
    /// Reverse position by slot ID; -1 when the slot is not active.
    pub(crate) active_index: Vec<i32>,
    /// Free-list stack of reusable slot IDs.
    pub(crate) free_slots: Vec<u32>,
    /// Conservative free-byte budget; None forces a rescan.
    pub(crate) cached_free: Option<usize>,
    /// Slot currently registered as the publisher's capacity wait target.
    pub(crate) pin: Option<u32>,
    /// Slots reserved by an in-flight handshake but not yet active.
    pub(crate) pending: usize,
}

impl Registry {
    pub(crate) fn new(max_subscribers: usize) -> Self {
        let mut free_slots: Vec<u32> = (0..max_subscribers as u32).collect();
        free_slots.reverse();
        Self {
            active_slots: Vec::with_capacity(max_subscribers),
            active_index: vec![-1; max_subscribers],
            free_slots,
            cached_free: None,
            pin: None,
            pending: 0,
        }
    }
}

enum WaitAbort {
    Cancelled,
    Fatal(io::Error),
}

/// Publisher-local state shared with the control worker and the send path.
pub(crate) struct BroadcastInner {
    pub(crate) shared: Shared,
    pub(crate) protocol: ProtocolDescriptor,
    pub(crate) session: u64,
    pub(crate) capacity: usize,
    pub(crate) max_subscribers: usize,
    pub(crate) name: String,
    pub(crate) setup_timeout: Duration,
    pub(crate) registry: Mutex<Registry>,
    pub(crate) subscribers_changed: Condvar,
    pub(crate) shutdown: AtomicBool,
    /// Bounded number of handshakes handled concurrently by the control worker.
    pub(crate) handshakes: AtomicUsize,
}

impl BroadcastInner {
    pub(crate) fn write_word(&self) -> &AtomicU32 {
        self.shared.index(BROADCAST_WRITE_OFFSET)
    }

    fn slot_base(&self, slot: u32) -> usize {
        BROADCAST_SLOTS_OFFSET + slot as usize * BROADCAST_SLOT_STRIDE
    }

    pub(crate) fn slot_read_word(&self, slot: u32) -> &AtomicU32 {
        self.shared.index(self.slot_base(slot) + BROADCAST_SLOT_READ_CURSOR)
    }

    pub(crate) fn slot_state_word(&self, slot: u32) -> &AtomicU32 {
        self.shared.index(self.slot_base(slot) + BROADCAST_SLOT_STATE)
    }

    pub(crate) fn slot_generation_word(&self, slot: u32) -> &AtomicU32 {
        self.shared.index(self.slot_base(slot) + BROADCAST_SLOT_GENERATION)
    }

    pub(crate) fn slot_flag_word(&self, slot: u32) -> &AtomicU32 {
        self.shared
            .index(self.slot_base(slot) + BROADCAST_SLOT_PRODUCER_WAITING)
    }

    fn load_write(&self) -> io::Result<usize> {
        let w = self.write_word().load(Ordering::Acquire) as usize;
        if w >= self.capacity || !w.is_multiple_of(8) {
            return Err(invalid("broadcast write cursor is outside the aligned ring"));
        }
        Ok(w)
    }

    /// Conservative free-byte budget for the current published write position.
    /// `free = capacity - gap - max(lag[i])`, never the numerical minimum of
    /// wrapping physical cursors.
    fn scan_free(&self, registry: &Registry) -> io::Result<usize> {
        let w = self.write_word().load(Ordering::SeqCst) as usize;
        if w >= self.capacity || !w.is_multiple_of(8) {
            return Err(invalid("broadcast write cursor is outside the aligned ring"));
        }
        let mut max_lag = 0usize;
        for &slot in &registry.active_slots {
            let r = self.slot_read_word(slot).load(Ordering::SeqCst) as usize;
            if r >= self.capacity || !r.is_multiple_of(8) {
                return Err(invalid("subscriber read cursor is outside the aligned ring"));
            }
            let lag = (w + self.capacity - r) % self.capacity;
            if lag > max_lag {
                max_lag = lag;
            }
        }
        if max_lag > self.capacity - RESERVED_GAP {
            return Err(invalid("broadcast ring exceeds its reserved gap"));
        }
        Ok(self.capacity - RESERVED_GAP - max_lag)
    }

    /// Publishes the fitting prefix of records. Blocking on capacity is the
    /// selected policy; invalid or oversized records are rejected before any
    /// wait because readers cannot make them valid.
    pub(crate) fn send_batch(
        &self, records: &[Record<'_>], cancellation: Option<&CancellationToken>,
    ) -> BroadcastOutcome {
        let mut outcome = BroadcastOutcome::empty();
        while outcome.accepted < records.len() {
            match self.stage_chunk(records, outcome.accepted, cancellation) {
                Chunk::Published {
                    staged,
                    rejection,
                    notification_error,
                } => {
                    outcome.accepted += staged;
                    if let Some(error) = notification_error {
                        outcome.notification_error = Some(error);
                    }
                    if let Some(reason) = rejection {
                        outcome.rejection = Some(reason);
                        break;
                    }
                    if staged == 0 {
                        // Only reachable when the chunk published a wrap marker
                        // or a retry was needed; the outer loop continues.
                        continue;
                    }
                }
                Chunk::Cancelled => {
                    outcome.cancelled = true;
                    break;
                }
                Chunk::Fatal(error) => {
                    outcome.failure = Some(error);
                    break;
                }
            }
        }
        outcome
    }

    fn stage_chunk(&self, records: &[Record<'_>], start: usize, cancellation: Option<&CancellationToken>) -> Chunk {
        let mut guard = self.registry.lock().unwrap();
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                return Chunk::Fatal(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "broadcast publisher is shut down",
                ));
            }
            if cancellation.is_some_and(|token| token.check().is_err()) {
                return Chunk::Cancelled;
            }
            if guard.active_slots.is_empty() {
                match self.wait_for_subscriber(guard, cancellation) {
                    Ok(reacquired) => {
                        guard = reacquired;
                        continue;
                    }
                    Err(WaitAbort::Cancelled) => return Chunk::Cancelled,
                    Err(WaitAbort::Fatal(error)) => return Chunk::Fatal(error),
                }
            }
            if guard.cached_free.is_none() {
                match self.scan_free(&guard) {
                    Ok(free) => guard.cached_free = Some(free),
                    Err(error) => return Chunk::Fatal(error),
                }
            }
            let mut cursor = match self.load_write() {
                Ok(cursor) => cursor,
                Err(error) => return Chunk::Fatal(error),
            };
            let mut free = guard.cached_free.unwrap_or(0);
            let mut staged = 0usize;
            let mut rejection = None;
            let mut published = false;

            while start + staged < records.len() {
                let record = &records[start + staged];
                if !self.protocol.supports(record.kind) {
                    rejection = Some(Rejection::InvalidType);
                    break;
                }
                let size = match record_size(record.payload.len(), self.capacity) {
                    Ok(size) => size,
                    Err(reason) => {
                        rejection = Some(reason);
                        break;
                    }
                };
                let tail = self.capacity - cursor;
                if size <= tail {
                    if free < size {
                        if staged > 0 || published {
                            break;
                        }
                        match self.wait_for_space(guard, size, cancellation) {
                            Ok((reacquired, observed)) => {
                                guard = reacquired;
                                free = observed;
                                cursor = match self.load_write() {
                                    Ok(cursor) => cursor,
                                    Err(error) => return Chunk::Fatal(error),
                                };
                                continue;
                            }
                            Err(WaitAbort::Cancelled) => return Chunk::Cancelled,
                            Err(WaitAbort::Fatal(error)) => return Chunk::Fatal(error),
                        }
                    }
                    unsafe {
                        let at = self.shared.ring().add(cursor);
                        write_record(at, record, size);
                    }
                    cursor = (cursor + size) % self.capacity;
                    free -= size;
                    staged += 1;
                    continue;
                }
                // The record does not fit before the physical end: wrap first.
                if free < tail {
                    if staged > 0 || published {
                        break;
                    }
                    match self.wait_for_space(guard, tail, cancellation) {
                        Ok((reacquired, observed)) => {
                            guard = reacquired;
                            free = observed;
                            cursor = match self.load_write() {
                                Ok(cursor) => cursor,
                                Err(error) => return Chunk::Fatal(error),
                            };
                            continue;
                        }
                        Err(WaitAbort::Cancelled) => return Chunk::Cancelled,
                        Err(WaitAbort::Fatal(error)) => return Chunk::Fatal(error),
                    }
                }
                unsafe {
                    std::ptr::write_bytes(self.shared.ring().add(cursor), 0, RECORD_HEADER);
                }
                cursor = 0;
                free -= tail;
                published = true;
                if free >= size {
                    unsafe {
                        let at = self.shared.ring().add(0);
                        write_record(at, record, size);
                    }
                    cursor = size;
                    free -= size;
                    staged += 1;
                } else {
                    // Padding alone: publish it, then the outer loop waits for
                    // the record's own space. Never wait with a staged prefix
                    // that could make progress.
                    break;
                }
            }

            if staged > 0 || published {
                self.write_word().store(cursor as u32, Ordering::Release);
                guard.cached_free = Some(free);
                let notification_error = wake_all(self.write_word()).err();
                return Chunk::Published {
                    staged,
                    rejection,
                    notification_error,
                };
            }
            if rejection.is_some() {
                return Chunk::Published {
                    staged: 0,
                    rejection,
                    notification_error: None,
                };
            }
            // Nothing staged or published: at least one record remains, so
            // `start + staged < records.len()`. The loop must have broken out of
            // the staging `while` after a wait, which cannot happen here.
            return Chunk::Fatal(invalid("broadcast send made no progress"));
        }
    }

    /// Blocks until at least one subscriber is active. Uses a local condvar, not
    /// a shared poll, and stays cancellable.
    fn wait_for_subscriber<'a>(
        &'a self, guard: MutexGuard<'a, Registry>, cancellation: Option<&CancellationToken>,
    ) -> Result<MutexGuard<'a, Registry>, WaitAbort> {
        if let Some(token) = cancellation {
            if token.check().is_err() {
                return Err(WaitAbort::Cancelled);
            }
            let (reacquired, _) = self.subscribers_changed.wait_timeout(guard, WAIT_POLL).unwrap();
            return Ok(reacquired);
        }
        if self.shutdown.load(Ordering::SeqCst) {
            return Err(WaitAbort::Fatal(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "broadcast publisher is shut down",
            )));
        }
        Ok(self.subscribers_changed.wait(guard).unwrap())
    }

    /// Blocks until at least `needed` bytes are free, then returns the
    /// re-acquired registry and the refreshed free budget.
    ///
    /// It pins one limiting subscriber, arms its producer-waiting flag, rechecks
    /// capacity, then performs the shared compare-and-wait on that subscriber's
    /// read cursor. The store-then-load pairing (producer stores the flag then
    /// loads the cursor; reader stores the cursor then loads the flag) is the
    /// required proof: either the producer observes progress or the reader
    /// observes the armed flag. Everything here is SeqCst on purpose.
    fn wait_for_space<'a>(
        &'a self, mut guard: MutexGuard<'a, Registry>, needed: usize, cancellation: Option<&CancellationToken>,
    ) -> Result<(MutexGuard<'a, Registry>, usize), WaitAbort> {
        let free = match self.scan_free(&guard) {
            Ok(free) => free,
            Err(error) => return Err(WaitAbort::Fatal(error)),
        };
        guard.cached_free = Some(free);
        if free >= needed {
            return Ok((guard, free));
        }
        let w = match self.load_write() {
            Ok(w) => w,
            Err(error) => return Err(WaitAbort::Fatal(error)),
        };
        let mut limiting: Option<u32> = None;
        let mut max_lag = 0usize;
        for &slot in &guard.active_slots {
            let r = self.slot_read_word(slot).load(Ordering::SeqCst) as usize;
            if r >= self.capacity || !r.is_multiple_of(8) {
                return Err(WaitAbort::Fatal(invalid(
                    "subscriber read cursor is outside the aligned ring",
                )));
            }
            let lag = (w + self.capacity - r) % self.capacity;
            if lag > max_lag {
                max_lag = lag;
                limiting = Some(slot);
            }
        }
        let Some(slot) = limiting else {
            return Ok((guard, free));
        };
        guard.pin = Some(slot);
        self.slot_flag_word(slot).store(1, Ordering::SeqCst);
        let recheck = match self.scan_free(&guard) {
            Ok(recheck) => recheck,
            Err(error) => {
                self.slot_flag_word(slot).store(0, Ordering::SeqCst);
                guard.pin = None;
                return Err(WaitAbort::Fatal(error));
            }
        };
        guard.cached_free = Some(recheck);
        if recheck >= needed {
            self.slot_flag_word(slot).store(0, Ordering::SeqCst);
            guard.pin = None;
            return Ok((guard, recheck));
        }
        let expected = self.slot_read_word(slot).load(Ordering::SeqCst);
        let read_ptr = self.slot_read_word(slot) as *const AtomicU32 as usize;
        drop(guard);
        // SAFETY: the slot is pinned, and retirement keeps a pinned slot
        // unavailable until this wait clears the pin, so the address cannot be
        // reused. The mapping lives as long as `self`.
        let word = unsafe { &*(read_ptr as *const AtomicU32) };
        let wait_result = match cancellation {
            Some(token) => token.wait_on_policy(word, expected, WakePolicy::One),
            None => wait(word, expected).map(|()| true),
        };
        let mut guard = self.registry.lock().unwrap();
        self.slot_flag_word(slot).store(0, Ordering::SeqCst);
        guard.pin = None;
        self.finish_retiring(&mut guard, slot);
        match wait_result {
            Ok(true) => {
                if guard.cached_free.is_none() {
                    let free = match self.scan_free(&guard) {
                        Ok(free) => free,
                        Err(error) => return Err(WaitAbort::Fatal(error)),
                    };
                    guard.cached_free = Some(free);
                }
                let free = guard.cached_free.unwrap();
                Ok((guard, free))
            }
            Ok(false) => Err(WaitAbort::Cancelled),
            Err(error) => Err(WaitAbort::Fatal(error)),
        }
    }

    /// Reserves a free slot and a fresh generation for a joining subscriber.
    pub(crate) fn reserve_slot(&self, registry: &mut Registry) -> io::Result<(u32, u32)> {
        let Some(slot) = registry.free_slots.pop() else {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "no free subscriber slot"));
        };
        let previous = self.slot_generation_word(slot).load(Ordering::Relaxed);
        if previous == u32::MAX {
            registry.free_slots.push(slot);
            return Err(invalid("subscriber slot generation exhausted"));
        }
        let generation = previous + 1;
        self.slot_generation_word(slot).store(generation, Ordering::Relaxed);
        self.slot_read_word(slot).store(0, Ordering::Relaxed);
        self.slot_flag_word(slot).store(0, Ordering::Relaxed);
        self.slot_state_word(slot)
            .store(BROADCAST_SLOT_RESERVED, Ordering::Release);
        registry.pending += 1;
        Ok((slot, generation))
    }

    /// Releases a reservation whose handshake failed before activation.
    pub(crate) fn release_reservation(&self, registry: &mut Registry, slot: u32) {
        if self.slot_state_word(slot).load(Ordering::Relaxed) != BROADCAST_SLOT_RESERVED {
            return;
        }
        self.slot_state_word(slot).store(BROADCAST_SLOT_FREE, Ordering::Release);
        registry.free_slots.push(slot);
        registry.pending = registry.pending.saturating_sub(1);
        registry.cached_free = None;
    }

    /// Activates a reserved slot at the current published write position. This
    /// is the activation boundary: publications before it are excluded, those
    /// after it are retained.
    pub(crate) fn activate_slot(&self, registry: &mut Registry, slot: u32, generation: u32) -> io::Result<usize> {
        if self.slot_state_word(slot).load(Ordering::Acquire) != BROADCAST_SLOT_RESERVED {
            return Err(invalid("subscriber slot is not reserved"));
        }
        if self.slot_generation_word(slot).load(Ordering::Relaxed) != generation {
            return Err(invalid("subscriber slot generation mismatch"));
        }
        let w = self.load_write()?;
        self.slot_read_word(slot).store(w as u32, Ordering::SeqCst);
        self.slot_flag_word(slot).store(0, Ordering::SeqCst);
        self.slot_state_word(slot)
            .store(BROADCAST_SLOT_ACTIVE, Ordering::Release);
        let position = registry.active_slots.len() as i32;
        registry.active_slots.push(slot);
        registry.active_index[slot as usize] = position;
        registry.cached_free = None;
        registry.pending = registry.pending.saturating_sub(1);
        self.subscribers_changed.notify_all();
        Ok(w)
    }

    /// Removes an active subscriber. The slot becomes Retiring until the
    /// publisher clears any pin on it, so a cursor ABA cannot occur.
    pub(crate) fn retire_slot(&self, registry: &mut Registry, slot: u32, generation: u32) -> io::Result<()> {
        if slot as usize >= self.max_subscribers {
            return Err(invalid("subscriber slot is out of range"));
        }
        if self.slot_state_word(slot).load(Ordering::Acquire) != BROADCAST_SLOT_ACTIVE {
            return Err(invalid("subscriber slot is not active"));
        }
        if self.slot_generation_word(slot).load(Ordering::Relaxed) != generation {
            return Err(invalid("subscriber slot generation mismatch"));
        }
        let position = registry.active_index[slot as usize];
        if position < 0 {
            return Err(invalid("subscriber slot is missing from the active list"));
        }
        let last = registry.active_slots.len() - 1;
        let last_slot = registry.active_slots[last];
        registry.active_slots.swap_remove(position as usize);
        registry.active_index[slot as usize] = -1;
        if (position as usize) < registry.active_slots.len() {
            registry.active_index[last_slot as usize] = position;
        }
        registry.cached_free = None;
        self.slot_state_word(slot)
            .store(BROADCAST_SLOT_RETIRING, Ordering::Release);
        if registry.pin == Some(slot) {
            // Move the waited word to the current published write position so a
            // wake-before-wait cannot be missed, then wake the pinned producer.
            let w = self.load_write()?;
            self.slot_read_word(slot).store(w as u32, Ordering::SeqCst);
            wake(self.slot_read_word(slot))?;
        } else {
            self.finish_retiring(registry, slot);
        }
        Ok(())
    }

    fn finish_retiring(&self, registry: &mut Registry, slot: u32) {
        if self.slot_state_word(slot).load(Ordering::Acquire) != BROADCAST_SLOT_RETIRING {
            return;
        }
        self.slot_state_word(slot).store(BROADCAST_SLOT_FREE, Ordering::Release);
        registry.free_slots.push(slot);
        registry.cached_free = None;
    }

    /// Diagnostic count of active subscribers. Callers must not treat a sampled
    /// count as proof that publication is safe.
    pub(crate) fn subscriber_count(&self) -> usize {
        self.registry.lock().unwrap().active_slots.len()
    }
}

enum Chunk {
    Published {
        staged: usize,
        rejection: Option<Rejection>,
        notification_error: Option<io::Error>,
    },
    Cancelled,
    Fatal(io::Error),
}

/// Aligned record size, or the rejection that makes the record unstageable.
fn record_size(payload_len: usize, capacity: usize) -> Result<usize, Rejection> {
    let raw = RECORD_HEADER.checked_add(payload_len).ok_or(Rejection::Oversized)?;
    let size = raw.checked_add(7).map(|n| n & !7).ok_or(Rejection::Oversized)?;
    if size > capacity - RESERVED_GAP || payload_len > u32::MAX as usize {
        return Err(Rejection::Oversized);
    }
    Ok(size)
}

/// Writes a complete record (header, payload, zero padding) at `at`.
/// SAFETY: `at` must point at `size` exclusively owned ring bytes.
unsafe fn write_record(at: *mut u8, record: &Record<'_>, size: usize) {
    let payload_len = record.payload.len();
    std::ptr::copy_nonoverlapping((payload_len as u32).to_le_bytes().as_ptr(), at, 4);
    std::ptr::copy_nonoverlapping(record.kind.to_le_bytes().as_ptr(), at.add(4), 4);
    std::ptr::copy_nonoverlapping(record.payload.as_ptr(), at.add(RECORD_HEADER), payload_len);
    std::ptr::write_bytes(
        at.add(RECORD_HEADER + payload_len),
        0,
        size - RECORD_HEADER - payload_len,
    );
}

/// One subscriber's independently mapped view of the shared ring.
pub struct Subscription {
    shared: Shared,
    protocol: ProtocolDescriptor,
    session: u64,
    slot: u32,
    generation: u32,
    capacity: usize,
    control_endpoint: Option<crate::config::SetupEndpoint>,
    control_timeout: Duration,
    unsubscribed: bool,
}

impl Subscription {
    pub(crate) fn new(
        shared: Shared, protocol: ProtocolDescriptor, session: u64, slot: u32, generation: u32, capacity: usize,
    ) -> Self {
        Self {
            shared,
            protocol,
            session,
            slot,
            generation,
            capacity,
            control_endpoint: None,
            control_timeout: Duration::from_secs(1),
            unsubscribed: false,
        }
    }

    pub(crate) fn set_control(&mut self, endpoint: crate::config::SetupEndpoint, timeout: Duration) {
        self.control_endpoint = Some(endpoint);
        self.control_timeout = timeout;
    }

    pub(crate) fn control_endpoint(&self) -> Option<crate::config::SetupEndpoint> {
        self.control_endpoint.clone()
    }

    pub(crate) fn control_timeout(&self) -> Duration {
        self.control_timeout
    }

    pub(crate) fn protocol_descriptor(&self) -> ProtocolDescriptor {
        self.protocol
    }

    pub(crate) fn unsubscribed(&self) -> bool {
        self.unsubscribed
    }

    pub(crate) fn mark_unsubscribed(&mut self) {
        self.unsubscribed = true;
    }

    pub fn session_id(&self) -> u64 {
        self.session
    }

    pub fn slot_id(&self) -> u32 {
        self.slot
    }

    pub fn generation(&self) -> u32 {
        self.generation
    }

    fn write_word(&self) -> &AtomicU32 {
        self.shared.index(BROADCAST_WRITE_OFFSET)
    }

    fn read_word(&self) -> &AtomicU32 {
        self.shared
            .index(BROADCAST_SLOTS_OFFSET + self.slot as usize * BROADCAST_SLOT_STRIDE + BROADCAST_SLOT_READ_CURSOR)
    }

    fn flag_word(&self) -> &AtomicU32 {
        self.shared.index(
            BROADCAST_SLOTS_OFFSET + self.slot as usize * BROADCAST_SLOT_STRIDE + BROADCAST_SLOT_PRODUCER_WAITING,
        )
    }

    fn ring(&self) -> *mut u8 {
        self.shared.ring()
    }

    /// Returns the next owned record, or `None` after local cancellation.
    pub fn receive(&mut self) -> io::Result<(u32, Vec<u8>)> {
        self.receive_inner(None)?
            .ok_or_else(|| invalid("uncancelled broadcast receive stopped"))
    }

    /// Receives one record, or `None` after local cancellation. A record whose
    /// copy already started may still be delivered.
    pub fn receive_with_cancel(&mut self, cancellation: &CancellationToken) -> io::Result<Option<(u32, Vec<u8>)>> {
        self.receive_inner(Some(cancellation))
    }

    fn receive_inner(&mut self, cancellation: Option<&CancellationToken>) -> io::Result<Option<(u32, Vec<u8>)>> {
        loop {
            if cancellation.is_some_and(|token| token.check().is_err()) {
                return Ok(None);
            }
            let w = self.write_word().load(Ordering::Acquire) as usize;
            if w >= self.capacity || !w.is_multiple_of(8) {
                return Err(invalid("broadcast write cursor is outside the aligned ring"));
            }
            let r = self.read_word().load(Ordering::SeqCst) as usize;
            if r >= self.capacity || !r.is_multiple_of(8) {
                return Err(invalid("subscriber read cursor is outside the aligned ring"));
            }
            if r == w {
                if let Some(token) = cancellation {
                    if !token.wait_on_all(self.write_word(), w as u32)? {
                        return Ok(None);
                    }
                } else {
                    wait(self.write_word(), w as u32)?;
                }
                continue;
            }
            let span = if w > r { w - r } else { self.capacity - r };
            if span < RECORD_HEADER {
                return Err(invalid("published span lacks record header"));
            }
            let at = unsafe { self.ring().add(r) };
            let len = unsafe { u32::from_le_bytes(std::slice::from_raw_parts(at, 4).try_into().unwrap()) } as usize;
            let kind = unsafe { u32::from_le_bytes(std::slice::from_raw_parts(at.add(4), 4).try_into().unwrap()) };
            if kind == 0 {
                if len != 0 || r == 0 || w >= r {
                    return Err(invalid("invalid wrap marker"));
                }
                self.read_word().store(0, Ordering::SeqCst);
                continue;
            }
            if !self.protocol.supports(kind) {
                return Err(invalid("unknown record type"));
            }
            let size = len
                .checked_add(RECORD_HEADER)
                .and_then(|n| n.checked_add(7))
                .map(|n| n & !7)
                .ok_or_else(|| invalid("record size overflow"))?;
            if size > self.capacity - RESERVED_GAP || size > span {
                return Err(invalid("record exceeds published contiguous span"));
            }
            let data = unsafe { std::slice::from_raw_parts(at.add(RECORD_HEADER), len).to_vec() };
            self.read_word()
                .store(((r + size) % self.capacity) as u32, Ordering::SeqCst);
            // Advance first, then check the flag and wake one producer space
            // waiter. The producer's store-then-load pairs with this load.
            if self.flag_word().load(Ordering::SeqCst) == 1 {
                wake(self.read_word())?;
            }
            return Ok(Some((kind, data)));
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        crate::broadcast_setup::unsubscribe_best_effort(self);
    }
}

impl Drop for BroadcastInner {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.subscribers_changed.notify_all();
        let mut registry = self.registry.lock().unwrap();
        if let Some(slot) = registry.pin.take() {
            self.slot_flag_word(slot).store(0, Ordering::SeqCst);
        }
    }
}

/// Shared handle type used by the publisher and its control worker.
pub(crate) type SharedInner = Arc<BroadcastInner>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{BROADCAST_SLOTS_OFFSET, BROADCAST_SLOT_STRIDE as STRIDE};

    const PROTOCOL: ProtocolDescriptor = ProtocolDescriptor {
        id: *b"BCAST001",
        version: 1,
        message_types: &[1, 2],
    };

    fn unique_id() -> u64 {
        crate::test_unique_id()
    }

    struct Fixture {
        inner: Arc<BroadcastInner>,
        name: String,
        session: u64,
        capacity: usize,
        max_subscribers: usize,
    }

    impl Fixture {
        fn new(capacity: usize, max_subscribers: usize) -> Self {
            let session = unique_id();
            let (shared, name) =
                Shared::create_broadcast(session, capacity, PROTOCOL.version, max_subscribers).unwrap();
            let inner = Arc::new(BroadcastInner {
                shared,
                protocol: PROTOCOL,
                session,
                capacity,
                max_subscribers,
                name: name.clone(),
                setup_timeout: Duration::from_secs(1),
                registry: Mutex::new(Registry::new(max_subscribers)),
                subscribers_changed: Condvar::new(),
                shutdown: AtomicBool::new(false),
                handshakes: AtomicUsize::new(0),
            });
            Self {
                inner,
                name,
                session,
                capacity,
                max_subscribers,
            }
        }

        /// Activates one subscriber and returns its slot, generation, and the
        /// independent subscriber mapping a real process would hold.
        fn add_subscriber(&self) -> (u32, u32, Shared) {
            let (slot, generation) = {
                let mut registry = self.inner.registry.lock().unwrap();
                self.inner.reserve_slot(&mut registry).unwrap()
            };
            {
                let mut registry = self.inner.registry.lock().unwrap();
                self.inner.activate_slot(&mut registry, slot, generation).unwrap();
            }
            let region = BROADCAST_SLOTS_OFFSET + STRIDE * self.max_subscribers + self.capacity;
            let ring_offset = BROADCAST_SLOTS_OFFSET + STRIDE * self.max_subscribers;
            let shared = Shared::open_broadcast(
                &self.name,
                self.session,
                self.capacity,
                PROTOCOL.version,
                self.max_subscribers,
                region,
                ring_offset,
            )
            .unwrap();
            (slot, generation, shared)
        }
    }

    fn record(kind: u32, payload: &[u8]) -> Record<'_> {
        Record { kind, payload }
    }

    fn subscription(fixture: &Fixture, slot: u32, generation: u32, shared: Shared) -> Subscription {
        Subscription::new(shared, PROTOCOL, fixture.session, slot, generation, fixture.capacity)
    }

    #[test]
    fn round_trip_reaches_subscriber_in_order() {
        let fixture = Fixture::new(256, 4);
        let (slot, generation, shared) = fixture.add_subscriber();
        let mut subscriber = subscription(&fixture, slot, generation, shared);
        let outcome = fixture
            .inner
            .send_batch(&[record(1, b"first"), record(2, b"second")], None);
        assert_eq!(outcome.accepted, 2, "failure={:?}", outcome.failure);
        assert_eq!(subscriber.receive().unwrap(), (1, b"first".to_vec()));
        assert_eq!(subscriber.receive().unwrap(), (2, b"second".to_vec()));
    }

    #[test]
    fn invalid_and_oversized_records_are_rejected_without_publishing() {
        let fixture = Fixture::new(64, 4);
        let _ = fixture.add_subscriber();
        let outcome = fixture.inner.send_batch(&[record(9, b"x")], None);
        assert_eq!(outcome.accepted, 0);
        assert_eq!(outcome.rejection, Some(Rejection::InvalidType));
        let oversized = vec![0u8; 64];
        let outcome = fixture.inner.send_batch(&[record(1, &oversized)], None);
        assert_eq!(outcome.accepted, 0);
        assert_eq!(outcome.rejection, Some(Rejection::Oversized));
        assert_eq!(fixture.inner.write_word().load(Ordering::SeqCst), 0);
    }

    #[test]
    fn padding_only_then_progress_with_a_concurrent_reader() {
        let fixture = Fixture::new(64, 4);
        let (slot, generation, shared) = fixture.add_subscriber();
        let mut subscriber = subscription(&fixture, slot, generation, shared);
        let a = [1u8; 16];
        let b = [2u8; 16];
        let large = [9u8; 24];
        assert_eq!(
            fixture.inner.send_batch(&[record(1, &a), record(1, &b)], None).accepted,
            2
        );
        assert_eq!(subscriber.receive().unwrap().1, a);
        // The next send wraps the tail and then blocks until the reader frees
        // space. A second thread drains the ring while the publisher waits.
        let reader = std::thread::spawn(move || {
            let first = subscriber.receive().unwrap().1;
            let second = subscriber.receive().unwrap().1;
            (first, second)
        });
        assert_eq!(fixture.inner.send_batch(&[record(1, &large)], None).accepted, 1);
        let (first, second) = reader.join().unwrap();
        assert_eq!(first, b);
        assert_eq!(second, large);
    }

    #[test]
    fn late_join_excludes_history() {
        let fixture = Fixture::new(256, 4);
        let (slot, generation, shared) = fixture.add_subscriber();
        let mut first = subscription(&fixture, slot, generation, shared);
        assert_eq!(fixture.inner.send_batch(&[record(1, b"e1")], None).accepted, 1);
        assert_eq!(first.receive().unwrap().1, b"e1");
        // The second subscriber activates at the current write position.
        let (slot2, generation2, shared2) = fixture.add_subscriber();
        assert_eq!(fixture.inner.send_batch(&[record(1, b"e2")], None).accepted, 1);
        let mut second = subscription(&fixture, slot2, generation2, shared2);
        assert_eq!(second.receive().unwrap().1, b"e2");
    }

    #[test]
    fn all_active_subscribers_receive_identical_order() {
        let fixture = Fixture::new(4096, 8);
        let mut subscribers = Vec::new();
        for _ in 0..3 {
            let (slot, generation, shared) = fixture.add_subscriber();
            subscribers.push(subscription(&fixture, slot, generation, shared));
        }
        let records: Vec<Vec<u8>> = (0..50u8).map(|n| vec![n; 8 + n as usize]).collect();
        let borrowed: Vec<Record<'_>> = records.iter().map(|payload| record(1, payload)).collect();
        assert_eq!(fixture.inner.send_batch(&borrowed, None).accepted, 50);
        for subscriber in subscribers.iter_mut() {
            for expected in &records {
                let (kind, payload) = subscriber.receive().unwrap();
                assert_eq!(kind, 1);
                assert_eq!(&payload, expected);
            }
        }
    }

    #[test]
    fn full_ring_blocks_then_resumes_when_the_reader_advances() {
        let fixture = Fixture::new(64, 4);
        let (slot, generation, shared) = fixture.add_subscriber();
        let mut subscriber = subscription(&fixture, slot, generation, shared);
        let a = [1u8; 16];
        let b = [2u8; 16];
        let d = [3u8; 16];
        assert_eq!(
            fixture.inner.send_batch(&[record(1, &a), record(1, &b)], None).accepted,
            2
        );
        let reader = std::thread::spawn(move || {
            let first = subscriber.receive().unwrap().1;
            let second = subscriber.receive().unwrap().1;
            let third = subscriber.receive().unwrap().1;
            (first, second, third)
        });
        // The ring is full: this send blocks until the reader releases space.
        assert_eq!(fixture.inner.send_batch(&[record(1, &d)], None).accepted, 1);
        let (first, second, third) = reader.join().unwrap();
        assert_eq!(first, a);
        assert_eq!(second, b);
        assert_eq!(third, d);
    }

    #[test]
    fn dense_registry_removal_keeps_reverse_index_consistent() {
        let mut registry = Registry::new(4);
        for slot in 0..3u32 {
            let position = registry.active_slots.len() as i32;
            registry.active_slots.push(slot);
            registry.active_index[slot as usize] = position;
        }
        let slot = 1u32;
        let position = registry.active_index[slot as usize];
        let last = registry.active_slots.len() - 1;
        let last_slot = registry.active_slots[last];
        registry.active_slots.swap_remove(position as usize);
        registry.active_index[slot as usize] = -1;
        if (position as usize) < registry.active_slots.len() {
            registry.active_index[last_slot as usize] = position;
        }
        assert_eq!(registry.active_slots, vec![0, 2]);
        assert_eq!(registry.active_index[0], 0);
        assert_eq!(registry.active_index[1], -1);
        assert_eq!(registry.active_index[2], 1);
    }
}
