use crate::wait::{wait, wake};
use crate::{cancellation::CancellationToken, contract::ProtocolDescriptor, invalid, mapping::Shared};
use std::io;
use std::sync::atomic::{AtomicU32, Ordering};

pub const WRITE_OFFSET: usize = 128;
pub const READ_OFFSET: usize = 256;
pub const RING_OFFSET: usize = 384;
const HEADER: usize = 8;
const GAP: usize = 8;

pub struct Record<'a> {
    pub kind: u32,
    pub payload: &'a [u8],
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    Full,
    InvalidType,
    Oversized,
}
#[derive(Debug)]
pub struct SendResult {
    pub accepted: usize,
    pub rejection: Option<Rejection>,
    pub notification_error: Option<io::Error>,
}

impl Shared {
    fn index(&self, offset: usize) -> &AtomicU32 {
        // mmap and both fixed offsets meet 32-bit atomic alignment. x86_64/aarch64 use
        // native lock-free atomic words shared by the two processes.
        unsafe { &*self.ptr.as_ptr().add(offset).cast::<AtomicU32>() }
    }
    fn ring(&self) -> *mut u8 {
        unsafe { self.ptr.as_ptr().add(RING_OFFSET) }
    }
    fn indexes(&self) -> io::Result<(usize, usize)> {
        let r = self.index(READ_OFFSET).load(Ordering::Acquire) as usize;
        let w = self.index(WRITE_OFFSET).load(Ordering::Acquire) as usize;
        if r >= self.capacity || w >= self.capacity || !r.is_multiple_of(8) || !w.is_multiple_of(8) {
            return Err(invalid("queue index is outside the aligned ring"));
        }
        Ok((r, w))
    }
    pub(crate) fn send_batch(
        &mut self, records: &[Record<'_>], protocol: &ProtocolDescriptor,
    ) -> io::Result<SendResult> {
        self.send_batch_with_wake(records, protocol, wake)
    }
    fn send_batch_with_wake(
        &mut self, records: &[Record<'_>], protocol: &ProtocolDescriptor,
        wake_fn: impl FnOnce(&AtomicU32) -> io::Result<()>,
    ) -> io::Result<SendResult> {
        let (_, mut cursor) = self.indexes()?;
        let mut accepted = 0;
        let mut rejection = None;
        let mut published = false;
        for record in records {
            if !protocol.supports(record.kind) {
                rejection = Some(Rejection::InvalidType);
                break;
            }
            let Some(raw_size) = HEADER.checked_add(record.payload.len()) else {
                rejection = Some(Rejection::Oversized);
                break;
            };
            let Some(size) = raw_size.checked_add(7).map(|n| n & !7) else {
                rejection = Some(Rejection::Oversized);
                break;
            };
            if size > self.capacity - GAP || record.payload.len() > u32::MAX as usize {
                rejection = Some(Rejection::Oversized);
                break;
            }
            let read = self.index(READ_OFFSET).load(Ordering::Acquire) as usize;
            if read >= self.capacity || !read.is_multiple_of(8) {
                return Err(invalid("invalid read index"));
            }
            let used = if cursor >= read {
                cursor - read
            } else {
                self.capacity - read + cursor
            };
            if used > self.capacity - GAP {
                return Err(invalid("queue exceeds reserved gap"));
            }
            let free = self.capacity - GAP - used;
            let tail = self.capacity - cursor;
            if size > tail {
                if tail > free {
                    rejection = Some(Rejection::Full);
                    break;
                }
                unsafe {
                    std::ptr::write_bytes(self.ring().add(cursor), 0, HEADER);
                }
                cursor = 0;
                published = true;
                if tail + size > free {
                    rejection = Some(Rejection::Full);
                    break;
                }
            } else if size > free {
                rejection = Some(Rejection::Full);
                break;
            }
            unsafe {
                let at = self.ring().add(cursor);
                std::ptr::copy_nonoverlapping((record.payload.len() as u32).to_le_bytes().as_ptr(), at, 4);
                std::ptr::copy_nonoverlapping(record.kind.to_le_bytes().as_ptr(), at.add(4), 4);
                std::ptr::copy_nonoverlapping(record.payload.as_ptr(), at.add(HEADER), record.payload.len());
                std::ptr::write_bytes(at.add(raw_size), 0, size - raw_size);
            }
            cursor = (cursor + size) % self.capacity;
            accepted += 1;
            published = true;
        }
        let notification_error = if published {
            self.index(WRITE_OFFSET).store(cursor as u32, Ordering::Release);
            wake_fn(self.index(WRITE_OFFSET)).err()
        } else {
            None
        };
        Ok(SendResult {
            accepted,
            rejection,
            notification_error,
        })
    }
    pub(crate) fn receive(&mut self, protocol: &ProtocolDescriptor) -> io::Result<(u32, Vec<u8>)> {
        self.receive_inner(protocol, None)?
            .ok_or_else(|| invalid("uncancelled receive stopped"))
    }
    pub(crate) fn receive_with_cancel(
        &mut self, protocol: &ProtocolDescriptor, cancellation: &CancellationToken,
    ) -> io::Result<Option<(u32, Vec<u8>)>> {
        self.receive_inner(protocol, Some(cancellation))
    }
    fn receive_inner(
        &mut self, protocol: &ProtocolDescriptor, cancellation: Option<&CancellationToken>,
    ) -> io::Result<Option<(u32, Vec<u8>)>> {
        loop {
            if cancellation.is_some_and(|token| token.check().is_err()) {
                return Ok(None);
            }
            let (read, write) = self.indexes()?;
            if read == write {
                if let Some(token) = cancellation {
                    if !token.wait_on(self.index(WRITE_OFFSET), write as u32)? {
                        return Ok(None);
                    }
                } else {
                    wait(self.index(WRITE_OFFSET), write as u32)?;
                }
                continue;
            }
            let span = if write > read {
                write - read
            } else {
                self.capacity - read
            };
            if span < HEADER {
                return Err(invalid("published span lacks record header"));
            }
            let at = unsafe { self.ring().add(read) };
            let len = unsafe { u32::from_le_bytes(std::slice::from_raw_parts(at, 4).try_into().unwrap()) } as usize;
            let kind = unsafe { u32::from_le_bytes(std::slice::from_raw_parts(at.add(4), 4).try_into().unwrap()) };
            if kind == 0 {
                if len != 0 || read == 0 || write >= read {
                    return Err(invalid("invalid wrap marker"));
                }
                self.index(READ_OFFSET).store(0, Ordering::Release);
                continue;
            }
            if !protocol.supports(kind) {
                return Err(invalid("unknown record type"));
            }
            let size = len
                .checked_add(HEADER)
                .and_then(|n| n.checked_add(7))
                .map(|n| n & !7)
                .ok_or_else(|| invalid("record size overflow"))?;
            if size > self.capacity - GAP || size > span {
                return Err(invalid("record exceeds published contiguous span"));
            }
            let data = unsafe { std::slice::from_raw_parts(at.add(HEADER), len).to_vec() };
            self.index(READ_OFFSET)
                .store(((read + size) % self.capacity) as u32, Ordering::Release);
            return Ok(Some((kind, data)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CancellationToken;
    use std::time::{SystemTime, UNIX_EPOCH};
    const DEFAULT: ProtocolDescriptor = ProtocolDescriptor {
        id: *b"CORE0001",
        version: 2,
        message_types: &[1, 42],
    };

    #[test]
    fn cancelled_receive_keeps_published_record_available() {
        let (mut producer, mut consumer) = pair(64);
        producer.send_batch(&[record(b"retained")], &DEFAULT).unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel().unwrap();
        assert!(consumer.receive_with_cancel(&DEFAULT, &cancellation).unwrap().is_none());
        assert_eq!(consumer.receive(&DEFAULT).unwrap(), (1, b"retained".to_vec()));
    }

    #[test]
    fn cancellation_stops_a_blocked_receive_without_reclaiming_space() {
        let cancellation = CancellationToken::new();
        let worker_cancellation = cancellation.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let id = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64;
            let (mut consumer, _) = Shared::create(id, 64, DEFAULT.version).unwrap();
            let read_before = consumer.index(READ_OFFSET).load(Ordering::Acquire);
            ready_tx.send(()).unwrap();
            let received = consumer.receive_with_cancel(&DEFAULT, &worker_cancellation).unwrap();
            let read_after = consumer.index(READ_OFFSET).load(Ordering::Acquire);
            (received, read_before, read_after)
        });
        ready_rx.recv().unwrap();
        cancellation.cancel().unwrap();
        let (received, read_before, read_after) = worker.join().unwrap();
        assert!(received.is_none());
        assert_eq!(read_before, read_after);
    }
    const ALTERNATE: ProtocolDescriptor = ProtocolDescriptor {
        id: *b"ALT00001",
        version: 2,
        message_types: &[42],
    };
    const ONLY_ONE: ProtocolDescriptor = ProtocolDescriptor {
        id: *b"ONE00001",
        version: 2,
        message_types: &[1],
    };

    fn pair(capacity: usize) -> (Shared, Shared) {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = (SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64)
            .wrapping_add(NEXT_ID.fetch_add(1, Ordering::Relaxed));
        let (mut consumer, name) = Shared::create(id, capacity, 2).unwrap();
        let producer = Shared::open(&name, id, capacity, 2).unwrap();
        consumer.unlink_name().unwrap();
        (producer, consumer)
    }
    fn record(payload: &[u8]) -> Record<'_> {
        Record { kind: 1, payload }
    }

    #[test]
    fn full_ring_keeps_unread_records_and_accepts_only_prefix() {
        let (mut p, mut c) = pair(64);
        let a = [1u8; 16];
        let b = [2u8; 16];
        let d = [3u8; 16];
        let result = p.send_batch(&[record(&a), record(&b), record(&d)], &DEFAULT).unwrap();
        assert_eq!(result.accepted, 2);
        assert_eq!(result.rejection, Some(Rejection::Full));
        assert!(result.notification_error.is_none());
        assert_eq!(c.receive(&DEFAULT).unwrap().1, a);
        assert_eq!(c.receive(&DEFAULT).unwrap().1, b);
        let result = p.send_batch(&[record(&d)], &DEFAULT).unwrap();
        assert_eq!(result.accepted, 1);
        assert_eq!(c.receive(&DEFAULT).unwrap().1, d);
    }

    #[test]
    fn padding_only_publication_allows_a_later_send() {
        let (mut p, mut c) = pair(64);
        let a = [1u8; 16];
        let b = [2u8; 16];
        let large = [9u8; 24];
        assert_eq!(p.send_batch(&[record(&a), record(&b)], &DEFAULT).unwrap().accepted, 2);
        assert_eq!(c.receive(&DEFAULT).unwrap().1, a);
        let result = p.send_batch(&[record(&large)], &DEFAULT).unwrap();
        assert_eq!(result.accepted, 0);
        assert_eq!(result.rejection, Some(Rejection::Full));
        assert_eq!(p.index(WRITE_OFFSET).load(Ordering::Acquire), 0);
        assert_eq!(c.receive(&DEFAULT).unwrap().1, b);
        assert_eq!(p.send_batch(&[record(&large)], &DEFAULT).unwrap().accepted, 1);
        assert_eq!(c.receive(&DEFAULT).unwrap().1, large);
    }

    #[test]
    fn invalid_inputs_and_corrupt_headers_fail_without_releasing_bytes() {
        let (mut p, mut c) = pair(64);
        assert_eq!(
            p.send_batch(&[Record { kind: 0, payload: &[] }], &DEFAULT)
                .unwrap()
                .rejection,
            Some(Rejection::InvalidType)
        );
        assert_eq!(
            p.send_batch(&[record(&[0u8; 49])], &DEFAULT).unwrap().rejection,
            Some(Rejection::Oversized)
        );
        assert_eq!(p.index(WRITE_OFFSET).load(Ordering::Acquire), 0);
        p.send_batch(&[record(&[4u8; 8])], &DEFAULT).unwrap();
        unsafe {
            std::ptr::write_unaligned(c.ring().add(4).cast::<u32>(), 99u32.to_le());
        }
        assert!(c.receive(&DEFAULT).is_err());
        assert_eq!(c.index(READ_OFFSET).load(Ordering::Acquire), 0);
    }

    #[test]
    fn published_record_remains_accepted_when_wake_fails() {
        let (mut p, mut c) = pair(64);
        let result = p
            .send_batch_with_wake(&[record(b"published")], &DEFAULT, |_| {
                Err(io::Error::other("injected wake failure"))
            })
            .unwrap();
        assert_eq!(result.accepted, 1);
        assert!(result.rejection.is_none());
        assert_eq!(result.notification_error.unwrap().to_string(), "injected wake failure");
        assert_eq!(c.receive(&DEFAULT).unwrap().1, b"published");
    }

    #[test]
    fn empty_payload_and_exact_end_are_valid() {
        let (mut p, mut c) = pair(32);
        assert_eq!(
            p.send_batch(&[record(&[]), record(&[1u8; 8])], &DEFAULT)
                .unwrap()
                .accepted,
            2
        );
        assert_eq!(p.index(WRITE_OFFSET).load(Ordering::Acquire), 24);
        assert_eq!(c.receive(&DEFAULT).unwrap().1, Vec::<u8>::new());
        assert_eq!(c.receive(&DEFAULT).unwrap().1, [1u8; 8]);
        assert_eq!(p.send_batch(&[record(&[])], &DEFAULT).unwrap().accepted, 1);
        assert_eq!(p.index(WRITE_OFFSET).load(Ordering::Acquire), 0);
        assert!(c.receive(&DEFAULT).unwrap().1.is_empty());
    }
    #[test]
    fn capacity_and_derived_maximum_payload() {
        assert!(crate::config::validate_capacity(16).is_ok());
        assert!(crate::config::validate_capacity(1 << 30).is_ok());
        for bad in [0, 8, 17, (1 << 30) + 8] {
            assert!(crate::config::validate_capacity(bad).is_err());
        }
        let (mut p, mut c) = pair(64);
        let largest = vec![0xa5; 48];
        assert_eq!(
            p.send_batch(
                &[Record {
                    kind: 1,
                    payload: &largest
                }],
                &DEFAULT
            )
            .unwrap()
            .accepted,
            1
        );
        assert_eq!(
            p.send_batch(
                &[Record {
                    kind: 1,
                    payload: &[0; 49]
                }],
                &DEFAULT
            )
            .unwrap()
            .rejection,
            Some(Rejection::Oversized)
        );
        assert_eq!(c.receive(&DEFAULT).unwrap().1, largest);
    }
    #[test]
    fn mixed_records_survive_repeated_wraps_and_owned_data_survives_reuse() {
        let (mut p, mut c) = pair(128);
        let mut retained = Vec::new();
        for n in 0..2000u32 {
            let a = vec![n as u8; (n as usize * 7) % 49];
            let b = vec![(n >> 8) as u8; (n as usize * 11) % 41];
            let result = p
                .send_batch(
                    &[Record { kind: 1, payload: &a }, Record { kind: 1, payload: &b }],
                    &DEFAULT,
                )
                .unwrap();
            assert!(result.notification_error.is_none());
            if result.accepted >= 1 {
                let got = c.receive(&DEFAULT).unwrap().1;
                assert_eq!(got, a);
                retained.push((got, a.clone()));
            }
            if result.accepted >= 2 {
                assert_eq!(c.receive(&DEFAULT).unwrap().1, b);
            }
        }
        assert!(retained.len() > 1000, "most batches should accept a first record");
        assert!(retained.iter().all(|(got, expected)| got == expected));
    }
    #[test]
    fn wait_returns_when_snapshot_is_already_stale() {
        let (p, _) = pair(64);
        p.index(WRITE_OFFSET).store(8, Ordering::Release);
        wait(p.index(WRITE_OFFSET), 0).unwrap();
    }
    #[test]
    fn second_descriptor_supports_type_42_without_core_edits() {
        let (mut producer, mut consumer) = pair(128);
        let result = producer
            .send_batch(
                &[Record {
                    kind: 42,
                    payload: b"forty-two",
                }],
                &ALTERNATE,
            )
            .unwrap();
        assert_eq!(result.accepted, 1);
        assert_eq!(consumer.receive(&ALTERNATE).unwrap(), (42, b"forty-two".to_vec()));
        assert_eq!(
            producer
                .send_batch(
                    &[Record {
                        kind: 42,
                        payload: b"again"
                    }],
                    &ALTERNATE
                )
                .unwrap()
                .accepted,
            1
        );
        let before = consumer.index(READ_OFFSET).load(Ordering::Acquire);
        assert!(consumer.receive(&ONLY_ONE).is_err());
        assert_eq!(consumer.index(READ_OFFSET).load(Ordering::Acquire), before);
        assert_eq!(consumer.receive(&ALTERNATE).unwrap(), (42, b"again".to_vec()));
    }

    #[test]
    fn concurrent_small_ring_preserves_order_and_payload_across_wraps() {
        let capacity = 256;
        let id = (SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64).wrapping_add(0x5000_0000);
        let (mut consumer, name) = Shared::create(id, capacity, 2).unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let producer = std::thread::spawn(move || {
            let mut producer = Shared::open(&name, id, capacity, 2).unwrap();
            ready_tx.send(()).unwrap();
            for sequence in 0..10_000u32 {
                let mut payload = sequence.to_le_bytes().to_vec();
                payload.extend(std::iter::repeat_n(
                    (sequence % 251) as u8,
                    (sequence as usize * 17) % 96,
                ));
                loop {
                    let result = producer
                        .send_batch(
                            &[Record {
                                kind: 1,
                                payload: &payload,
                            }],
                            &DEFAULT,
                        )
                        .unwrap();
                    assert!(result.notification_error.is_none());
                    if result.accepted == 1 {
                        break;
                    }
                    assert_eq!(result.rejection, Some(Rejection::Full));
                    std::thread::yield_now();
                }
            }
        });
        ready_rx.recv().unwrap();
        consumer.unlink_name().unwrap();
        for sequence in 0..10_000u32 {
            let (kind, payload) = consumer.receive(&DEFAULT).unwrap();
            assert_eq!(kind, 1);
            assert_eq!(u32::from_le_bytes(payload[..4].try_into().unwrap()), sequence);
            assert_eq!(payload.len(), 4 + (sequence as usize * 17) % 96);
            assert!(payload[4..].iter().all(|&b| b == (sequence % 251) as u8));
        }
        producer.join().unwrap();
    }

    #[test]
    #[ignore = "copies more than 4 GiB to exercise repeated physical index wrap"]
    fn traffic_exceeds_four_gibibytes_without_counter_overflow() {
        let (mut p, mut c) = pair(1 << 20);
        let payload = vec![0x5a; (1 << 18) - 8];
        for _ in 0..16385 {
            let record = Record {
                kind: 1,
                payload: &payload,
            };
            let first = p.send_batch(&[record], &DEFAULT).unwrap();
            assert_eq!(first.accepted, 1);
            assert_eq!(c.receive(&DEFAULT).unwrap().1, payload);
        }
    }
}
