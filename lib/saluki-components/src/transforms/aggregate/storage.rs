//! Stores live aggregate metrics in dense, typed pages.
//!
//! # Design
//!
//! The hash table stores only a row location. The row holds the context used to check for a match.
//! Separate pages give small rows, such as gauges, space for only their own payload type.
//! The index can grow without moving the rows, but a lookup must follow the location to read the row.
//! Side stores save space in each row at the cost of extra lookups and unpacking for less common values and metadata.
//!
//! ```text
//! Context hash -> HashTable<u32> -> Slot -> Rows[kind] -> Pages<Row<T>>
//!                                                               |
//!                           MetadataStore <- metadata_id <- [Header | payload]
//!                                                                        |
//!                                      SideStore<MetricValues> <- overflow index
//! ```
//!
//! The diagram shows logical fields and references, not field offsets or Rust's physical field order.
//! The byte sizes describe the 64-bit Rust layout. They do not define an ABI or a wire format.
//!
//! | Row | Bytes | Inline payload |
//! | --- | ---: | --- |
//! | `GaugeRow` | 40 | Up to two scalar points |
//! | `CountRow` | 48 | Up to two scalar points and the last update time |
//! | `RateRow` | 56 | Up to two scalar points and their rate interval |
//! | `SetRow` | 56 | One set bucket |
//! | `HistogramRow` | 80 | One bucket with up to three weighted samples |
//! | `DistributionRow` | 88 | One sketch bucket with up to four bins |
//!
//! Each row includes a 16-byte `Header`. An inline set bucket can still own heap storage for its members.
//!
use std::{hash::BuildHasher, mem, num::NonZeroU64, time::Duration};

use ddsketch::DDSketch;
use hashbrown::HashTable;
use saluki_core::data_model::event::metric::{
    context::Context, Histogram, HistogramPoints, MetricMetadata, MetricOrigin, MetricValues, ScalarPoints, SetBucket,
    SetPoints, SketchPoints,
};
use stringtheory::MetaString;

use super::{telemetry::OriginContextId, AggregateMetricType as Kind};

const PAGE_BYTES: usize = 32 * 1024;
// Leave room for the resource tracking allocator's pointer trailer.
const PAGE_DATA_BYTES: usize = PAGE_BYTES - mem::size_of::<usize>();
const SPARE_PAGES: usize = 2;
const FALLBACK: u16 = 1 << 15;
const WEIGHTS_VARY: u16 = 1 << 8;
const OWNED_METADATA: u32 = 1 << 31;
const SLOT_BITS: u32 = 29;
const SLOT_MASK: u32 = (1 << SLOT_BITS) - 1;

// Each kind selects its own page sequence. The ordinal must agree in `ALL`, `index`, and the slot encoding.
impl Kind {
    pub(super) const ALL: [Self; 6] = [
        Self::Gauge,
        Self::Counter,
        Self::Rate,
        Self::Set,
        Self::Histogram,
        Self::Distribution,
    ];

    const fn index(self) -> usize {
        match self {
            Self::Gauge => 0,
            Self::Counter => 1,
            Self::Rate => 2,
            Self::Set => 3,
            Self::Histogram => 4,
            Self::Distribution => 5,
        }
    }
}

/// The current location of a row, not a stable ID.
///
/// ```text
/// bits  31 .. 29 | 28 ........................ 0
///       kind (3) | row number within kind (29)
/// ```
///
/// Swapping rows or removing a row can move another row. Storage updates the affected index entries before reuse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Slot(u32);

impl Slot {
    /// Encodes a kind and a zero-based row number.
    ///
    /// # Panics
    ///
    /// Panics if the row number does not fit in 29 bits.
    pub(super) fn new(kind: Kind, index: usize) -> Self {
        let index = u32::try_from(index).expect("row number fits in the index");
        assert!(index <= SLOT_MASK, "row number fits in the index");
        Self((kind.index() as u32) << SLOT_BITS | index)
    }

    pub(super) const fn kind(self) -> Kind {
        Kind::ALL[(self.0 >> SLOT_BITS) as usize]
    }

    pub(super) const fn index(self) -> usize {
        (self.0 & SLOT_MASK) as usize
    }
}

/// A dense row sequence backed by page-sized allocations.
///
/// ```text
/// pages -> [full] [full] ... [last, possibly partial]
/// spare -> [empty] [empty]    (up to SPARE_PAGES, most recent first on reuse)
/// ```
///
/// Only the last live page can contain unused row slots. The row number selects a page and its offset within that page.
/// On removal, the last row fills the gap. Storage repairs the index entry for any row that moves.
/// Empty pages move to `spare` until it reaches its limit. This retains some memory to avoid allocation during churn.
struct Pages<T> {
    pages: Vec<Vec<T>>,
    spare: Vec<Vec<T>>,
    len: usize,
}

impl<T> Default for Pages<T> {
    fn default() -> Self {
        Self {
            pages: Vec::new(),
            spare: Vec::new(),
            len: 0,
        }
    }
}

impl<T> Pages<T> {
    const ROWS: usize = PAGE_DATA_BYTES / mem::size_of::<T>();

    fn get(&self, index: usize) -> &T {
        &self.pages[index / Self::ROWS][index % Self::ROWS]
    }

    fn get_mut(&mut self, index: usize) -> &mut T {
        &mut self.pages[index / Self::ROWS][index % Self::ROWS]
    }

    fn push(&mut self, value: T) {
        if self.len.is_multiple_of(Self::ROWS) {
            self.pages
                .push(self.spare.pop().unwrap_or_else(|| Vec::with_capacity(Self::ROWS)));
        }
        self.pages[self.len / Self::ROWS].push(value);
        self.len += 1;
    }

    fn swap_remove(&mut self, index: usize) -> T {
        let page = self.pages.last_mut().expect("a live row has a page");
        let last = page.pop().expect("a live page has rows");
        self.len -= 1;
        if page.is_empty() {
            let page = self.pages.pop().expect("the last page exists");
            if self.spare.len() < SPARE_PAGES {
                self.spare.push(page);
            }
        }
        if index == self.len {
            last
        } else {
            mem::replace(self.get_mut(index), last)
        }
    }
}

enum SideSlot<T> {
    Live(T),
    Free(Option<u32>),
}

/// Stores uncommon payloads without increasing the size of every row.
///
/// ```text
/// row -> slot i: Live(T)
/// free -> slot j: Free(next) -> slot k: Free(None)
/// ```
///
/// A row owns each live slot. Removing a value adds its slot to the head of the free list.
/// Insertion reuses the most recently freed slot before it grows the pages.
/// Slot numbers cannot change while a row owns one. A partly free store can retain its high-water allocation.
/// When no live slots remain, trimming keeps only the allowed spare pages and clears the free list.
struct SideStore<T> {
    slots: Pages<SideSlot<T>>,
    free: Option<u32>,
    live: usize,
}

impl<T> Default for SideStore<T> {
    fn default() -> Self {
        Self {
            slots: Pages::default(),
            free: None,
            live: 0,
        }
    }
}

impl<T> SideStore<T> {
    fn insert(&mut self, value: T) -> u32 {
        self.live += 1;
        if let Some(index) = self.free {
            let SideSlot::Free(next) = mem::replace(self.slots.get_mut(index as usize), SideSlot::Live(value)) else {
                unreachable!("the free list contains only free slots");
            };
            self.free = next;
            index
        } else {
            let index = u32::try_from(self.slots.len).expect("side slot number fits in the index");
            self.slots.push(SideSlot::Live(value));
            index
        }
    }

    fn get_mut(&mut self, index: u32) -> &mut T {
        match self.slots.get_mut(index as usize) {
            SideSlot::Live(value) => value,
            SideSlot::Free(_) => unreachable!("a retained row owns its side slot"),
        }
    }

    fn remove(&mut self, index: u32) -> T {
        let SideSlot::Live(value) = mem::replace(self.slots.get_mut(index as usize), SideSlot::Free(self.free)) else {
            unreachable!("a retained row owns its side slot");
        };
        self.free = Some(index);
        self.live -= 1;
        value
    }

    fn trim_if_empty(&mut self) {
        if self.live == 0 && self.slots.len != 0 {
            while self.slots.spare.len() < SPARE_PAGES {
                let Some(mut page) = self.slots.pages.pop() else { break };
                page.clear();
                self.slots.spare.push(page);
            }
            self.slots.pages.clear();
            self.slots.len = 0;
            self.free = None;
        }
    }
}

/// Stores uncommon metadata and encodes common origin and unit combinations in the row.
///
/// ```text
/// metadata_id   bit 31 | bits 30 ........................ 0
///                    1 | owned metadata slot
///                    0 | origin detail (30..3) | ms (2) | origin code (1..0)
/// ```
///
/// Inline origin codes represent no origin, DogStatsD, or an origin with product 10 and subproduct 11.
/// The first origin and the first non-empty unit stay with the context until removal, even if its kind changes.
/// An update can fill an empty unit but cannot change the origin or overwrite a unit.
/// A flush removes a non-counter when no open values remain. An idle counter can keep its metadata until expiry.
#[derive(Default)]
struct MetadataStore(SideStore<MetricMetadata>);

impl MetadataStore {
    fn insert(&mut self, metadata: MetricMetadata) -> u32 {
        if let Some(id) = static_metadata_id(&metadata) {
            id
        } else {
            let index = self.0.insert(metadata);
            assert!(index < OWNED_METADATA, "metadata slot fits in the index");
            OWNED_METADATA | index
        }
    }

    fn fill_unit(&mut self, id: &mut u32, unit: MetaString) {
        if unit.is_empty() {
            return;
        }
        if *id & OWNED_METADATA != 0 {
            let stored = self.0.get_mut(*id & !OWNED_METADATA);
            if stored.unit.is_empty() {
                stored.unit = unit;
            }
        } else if *id & 4 == 0 {
            if unit == "millisecond" {
                *id |= 4;
            } else {
                *id = self.insert(static_metadata(*id).with_unit(unit));
            }
        }
    }

    fn clone_metadata(&self, id: u32) -> MetricMetadata {
        if id & OWNED_METADATA == 0 {
            static_metadata(id)
        } else {
            match self.0.slots.get((id & !OWNED_METADATA) as usize) {
                SideSlot::Live(metadata) => metadata.clone(),
                SideSlot::Free(_) => unreachable!("a retained row owns its metadata"),
            }
        }
    }

    fn remove(&mut self, id: u32) {
        if id & OWNED_METADATA != 0 {
            drop(self.0.remove(id & !OWNED_METADATA));
        }
    }
}

fn static_metadata_id(metadata: &MetricMetadata) -> Option<u32> {
    let unit = match metadata.unit() {
        None => 0,
        Some("millisecond") => 4,
        _ => return None,
    };
    let origin = match metadata.origin() {
        None => 0,
        Some(MetricOrigin::OriginMetadata {
            product: 10,
            subproduct: 10,
            product_detail: 0,
        }) => 1,
        Some(MetricOrigin::OriginMetadata {
            product: 10,
            subproduct: 11,
            product_detail,
        }) if *product_detail < (OWNED_METADATA >> 3) => (*product_detail << 3) | 2,
        _ => return None,
    };
    Some(origin | unit)
}

fn static_metadata(id: u32) -> MetricMetadata {
    let origin = match id & 3 {
        0 => None,
        1 => Some(MetricOrigin::dogstatsd()),
        _ => Some(MetricOrigin::OriginMetadata {
            product: 10,
            subproduct: 11,
            product_detail: id >> 3,
        }),
    };
    let unit = if id & 4 == 0 {
        MetaString::empty()
    } else {
        MetaString::from_static("millisecond")
    };
    MetricMetadata { origin, unit }
}

/// The common part of every typed row.
///
/// `origin_id` is the telemetry admission token. It tracks origin-context accounting, not the metric's origin metadata.
/// `flags` selects how to read the payload:
///
/// ```text
/// flags & FALLBACK != 0 -> timestamp field stores an index into overflow
/// flags & FALLBACK == 0 -> payload is inline; flags count or describe its contents
/// ```
///
/// Inline `flags == 0` means empty. For histograms and sketches, the count is encoded plus one.
/// The histogram also uses a flag bit to distinguish per-sample weights from one shared weight.
pub(super) struct Header {
    pub(super) context: Context,
    metadata_id: u32,
    pub(super) origin_id: OriginContextId,
    flags: u16,
}

/// A common header paired with the payload for one metric kind.
struct Row<T> {
    header: Header,
    payload: T,
}

/// Up to two scalar points in timestamp order.
///
/// A singleton update replaces a gauge at the same timestamp. Counters and rates add to the existing point.
/// If a point cannot fit inline, storage merges the full values and tries to pack the result again.
#[derive(Default)]
struct Scalar {
    values: [f64; 2],
    timestamps: [u32; 2],
}

impl Scalar {
    fn pack(&mut self, points: &ScalarPoints, epoch: u64) -> Option<u16> {
        if points.len() > 2 {
            return None;
        }
        for (i, (timestamp, value)) in points.into_iter().enumerate() {
            self.timestamps[i] = encode_timestamp(epoch, timestamp)?;
            self.values[i] = value;
        }
        Some(points.len() as u16)
    }

    fn unpack(&self, len: u16, epoch: u64) -> ScalarPoints {
        (0..usize::from(len))
            .map(|i| (decode_timestamp(epoch, self.timestamps[i]), self.values[i]))
            .collect()
    }

    fn merge_single(&mut self, len: &mut u16, points: &ScalarPoints, epoch: u64, add: bool) -> bool {
        let Some((timestamp, value)) = points.single_point() else {
            return false;
        };
        let Some(timestamp) = encode_timestamp(epoch, timestamp) else {
            return false;
        };
        let n = usize::from(*len);
        if let Some(index) = self.timestamps[..n].iter().position(|&t| t == timestamp) {
            if add {
                self.values[index] += value;
            } else {
                self.values[index] = value;
            }
        } else if n < 2 {
            let index = usize::from(n == 1 && self.timestamps[0] < timestamp);
            if n == 1 && index == 0 {
                self.timestamps[1] = self.timestamps[0];
                self.values[1] = self.values[0];
            }
            self.timestamps[index] = timestamp;
            self.values[index] = value;
            *len += 1;
        } else {
            return false;
        }
        true
    }
}

/// Counter points and their last real update time for idle expiry.
///
/// Flush preserves `last_seen` when it adds zero values or repacks open buckets.
#[derive(Default)]
struct Count {
    scalar: Scalar,
    last_seen: u64,
}

/// Rate points and their common interval.
///
/// Inline addition requires matching intervals. Other updates use the general metric merge policy.
#[derive(Default)]
struct Rate {
    scalar: Scalar,
    interval: Duration,
}

#[derive(Default)]
struct Single<T> {
    value: T,
    timestamp: u32,
}

#[derive(Default)]
struct HistogramData {
    samples: [(f64, f64); 3],
    shared_weight: f64,
}

#[derive(Default)]
struct DistributionData {
    count: u64,
    summary: [f64; 4],
    counts: [u32; 4],
    keys: [i16; 4],
}

type GaugeRow = Row<Scalar>;
type CountRow = Row<Count>;
type RateRow = Row<Rate>;
type SetRow = Row<Single<SetBucket>>;
type HistogramRow = Row<Single<HistogramData>>;
type DistributionRow = Row<Single<DistributionData>>;

#[derive(Default)]
struct Rows {
    gauge: Pages<GaugeRow>,
    count: Pages<CountRow>,
    rate: Pages<RateRow>,
    set: Pages<SetRow>,
    histogram: Pages<HistogramRow>,
    distribution: Pages<DistributionRow>,
}

impl Rows {
    fn len(&self, kind: Kind) -> usize {
        match kind {
            Kind::Gauge => self.gauge.len,
            Kind::Counter => self.count.len,
            Kind::Rate => self.rate.len,
            Kind::Set => self.set.len,
            Kind::Histogram => self.histogram.len,
            Kind::Distribution => self.distribution.len,
        }
    }

    fn header(&self, slot: Slot) -> &Header {
        let i = slot.index();
        match slot.kind() {
            Kind::Gauge => &self.gauge.get(i).header,
            Kind::Counter => &self.count.get(i).header,
            Kind::Rate => &self.rate.get(i).header,
            Kind::Set => &self.set.get(i).header,
            Kind::Histogram => &self.histogram.get(i).header,
            Kind::Distribution => &self.distribution.get(i).header,
        }
    }

    fn header_mut(&mut self, slot: Slot) -> &mut Header {
        let i = slot.index();
        match slot.kind() {
            Kind::Gauge => &mut self.gauge.get_mut(i).header,
            Kind::Counter => &mut self.count.get_mut(i).header,
            Kind::Rate => &mut self.rate.get_mut(i).header,
            Kind::Set => &mut self.set.get_mut(i).header,
            Kind::Histogram => &mut self.histogram.get_mut(i).header,
            Kind::Distribution => &mut self.distribution.get_mut(i).header,
        }
    }

    fn push(&mut self, kind: Kind, header: Header) {
        match kind {
            Kind::Gauge => self.gauge.push(Row {
                header,
                payload: Scalar::default(),
            }),
            Kind::Counter => self.count.push(Row {
                header,
                payload: Count::default(),
            }),
            Kind::Rate => self.rate.push(Row {
                header,
                payload: Rate::default(),
            }),
            Kind::Set => self.set.push(Row {
                header,
                payload: Single::default(),
            }),
            Kind::Histogram => self.histogram.push(Row {
                header,
                payload: Single::default(),
            }),
            Kind::Distribution => self.distribution.push(Row {
                header,
                payload: Single::default(),
            }),
        }
    }

    fn swap_remove(&mut self, slot: Slot) -> Header {
        let i = slot.index();
        match slot.kind() {
            Kind::Gauge => self.gauge.swap_remove(i).header,
            Kind::Counter => self.count.swap_remove(i).header,
            Kind::Rate => self.rate.swap_remove(i).header,
            Kind::Set => self.set.swap_remove(i).header,
            Kind::Histogram => self.histogram.swap_remove(i).header,
            Kind::Distribution => self.distribution.swap_remove(i).header,
        }
    }

    fn auxiliary(&mut self, slot: Slot) -> &mut u32 {
        let i = slot.index();
        match slot.kind() {
            Kind::Gauge => &mut self.gauge.get_mut(i).payload.timestamps[0],
            Kind::Counter => &mut self.count.get_mut(i).payload.scalar.timestamps[0],
            Kind::Rate => &mut self.rate.get_mut(i).payload.scalar.timestamps[0],
            Kind::Set => &mut self.set.get_mut(i).payload.timestamp,
            Kind::Histogram => &mut self.histogram.get_mut(i).payload.timestamp,
            Kind::Distribution => &mut self.distribution.get_mut(i).payload.timestamp,
        }
    }
}

/// Owns the rows, their context index, and the side stores.
#[derive(Default)]
pub(super) struct Storage {
    rows: Rows,
    index: HashTable<u32>,
    hash_builder: foldhash::quality::RandomState,
    metadata: MetadataStore,
    overflow: SideStore<MetricValues>,
    // The first insertion fixes the epoch. Center the offset range on its arrival time when possible.
    epoch: Option<u64>,
}

impl Storage {
    /// Returns the largest row size, including the header and Rust layout padding.
    pub(super) const fn max_row_bytes() -> usize {
        let sizes = [
            mem::size_of::<GaugeRow>(),
            mem::size_of::<CountRow>(),
            mem::size_of::<RateRow>(),
            mem::size_of::<SetRow>(),
            mem::size_of::<HistogramRow>(),
            mem::size_of::<DistributionRow>(),
        ];
        let mut largest = 0;
        let mut i = 0;
        while i < sizes.len() {
            if sizes[i] > largest {
                largest = sizes[i];
            }
            i += 1;
        }
        largest
    }

    /// Returns the per-row page cost at the lowest density among the row types.
    ///
    /// Charge full pages, including space for the tracking trailer and any remainder that cannot hold another row.
    pub(super) const fn paged_row_bytes() -> usize {
        PAGE_BYTES.div_ceil(PAGE_DATA_BYTES / Self::max_row_bytes())
    }

    /// Estimates index backing bytes per row at growth.
    ///
    /// Each hashbrown bucket holds one index entry and one control byte. Large tables grow at a 7/8 load factor.
    /// Doubling the bucket count leaves the new table just under half full.
    /// This estimate excludes the trailing control group and the temporary overlap with the old table.
    pub(super) const fn index_entry_bytes() -> usize {
        (2 * Self::index_bucket_bytes() * 8).div_ceil(7)
    }

    const fn index_bucket_bytes() -> usize {
        mem::size_of::<u32>() + mem::size_of::<u8>()
    }

    /// Returns the fixed allowance for one partial page and the spare pages of each row kind.
    ///
    /// The estimate does not include the page directories, side stores, or heap allocations owned by rows.
    pub(super) const fn page_overhead_bytes() -> usize {
        PAGE_BYTES * Kind::ALL.len() * (1 + SPARE_PAGES)
    }

    pub(super) fn len(&self) -> usize {
        self.index.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    pub(super) fn len_for(&self, kind: Kind) -> usize {
        self.rows.len(kind)
    }

    pub(super) fn header(&self, slot: Slot) -> &Header {
        self.rows.header(slot)
    }

    pub(super) fn metadata(&self, slot: Slot) -> MetricMetadata {
        self.metadata.clone_metadata(self.header(slot).metadata_id)
    }

    pub(super) fn find(&self, context: &Context) -> Option<Slot> {
        self.index
            .find(self.hash_builder.hash_one(context), |&slot| {
                self.header(Slot(slot)).context == *context
            })
            .copied()
            .map(Slot)
    }

    pub(super) fn insert(
        &mut self, context: Context, values: MetricValues, metadata: MetricMetadata, last_seen: u64,
        origin_id: OriginContextId,
    ) {
        let kind = Kind::from(&values);
        let slot = Slot::new(kind, self.rows.len(kind));
        let hash = self.hash_builder.hash_one(&context);
        let metadata_id = self.metadata.insert(metadata);
        self.rows.push(
            kind,
            Header {
                context,
                metadata_id,
                origin_id,
                flags: 0,
            },
        );
        self.epoch
            .get_or_insert_with(|| last_seen.saturating_sub(u64::from(u32::MAX) / 2));
        self.set_values(slot, values, last_seen);
        let rows = &self.rows;
        let hashes = &self.hash_builder;
        self.index
            .insert_unique(hash, slot.0, |&id| hashes.hash_one(&rows.header(Slot(id)).context));
    }

    /// Merges an update into a context, or moves it to a new row kind when the kind changes.
    ///
    /// A kind change keeps the context and its metadata but drops values of the old kind.
    pub(super) fn update(&mut self, slot: Slot, values: MetricValues, metadata: MetricMetadata, last_seen: u64) {
        let kind = Kind::from(&values);
        let epoch = self.epoch.expect("a live row has an epoch");
        let header = self.rows.header_mut(slot);
        if !metadata.unit.is_empty() {
            self.metadata.fill_unit(&mut header.metadata_id, metadata.unit);
        }
        if slot.kind() != kind {
            self.discard_values(slot);
            let mut header = self.unlink(slot);
            header.flags = 0;
            let new_slot = Slot::new(kind, self.rows.len(kind));
            let hash = self.hash_builder.hash_one(&header.context);
            self.rows.push(kind, header);
            self.set_values(new_slot, values, last_seen);
            let rows = &self.rows;
            let hashes = &self.hash_builder;
            self.index
                .insert_unique(hash, new_slot.0, |&id| hashes.hash_one(&rows.header(Slot(id)).context));
            return;
        }
        if header.flags & FALLBACK == 0 {
            let i = slot.index();
            let merged = match &values {
                MetricValues::Gauge(points) => {
                    let row = self.rows.gauge.get_mut(i);
                    row.payload.merge_single(&mut row.header.flags, points, epoch, false)
                }
                MetricValues::Counter(points) => {
                    let row = self.rows.count.get_mut(i);
                    row.payload.last_seen = last_seen;
                    row.payload
                        .scalar
                        .merge_single(&mut row.header.flags, points, epoch, true)
                }
                MetricValues::Rate(points, interval) => {
                    let row = self.rows.rate.get_mut(i);
                    row.payload.interval == *interval
                        && row
                            .payload
                            .scalar
                            .merge_single(&mut row.header.flags, points, epoch, true)
                }
                _ => false,
            };
            if merged {
                return;
            }
        }
        let mut old_values = self.take_values(slot);
        old_values.merge(values);
        self.set_values(slot, old_values, last_seen);
    }

    pub(super) fn last_seen(&self, slot: Slot) -> u64 {
        if slot.kind() == Kind::Counter {
            self.rows.count.get(slot.index()).payload.last_seen
        } else {
            0
        }
    }

    /// Takes the values without removing the row or its metadata.
    ///
    /// This clears the flags and returns any overflow slot to the free list.
    /// Repack a retained row with `set_values`, or remove the row before processing another update.
    pub(super) fn take_values(&mut self, slot: Slot) -> MetricValues {
        let flags = mem::take(&mut self.rows.header_mut(slot).flags);
        if flags & FALLBACK != 0 {
            return self.overflow.remove(*self.rows.auxiliary(slot));
        }
        let i = slot.index();
        let epoch = self.epoch.expect("a live row has an epoch");
        match slot.kind() {
            Kind::Gauge => MetricValues::Gauge(self.rows.gauge.get(i).payload.unpack(flags, epoch)),
            Kind::Counter => MetricValues::Counter(self.rows.count.get(i).payload.scalar.unpack(flags, epoch)),
            Kind::Rate => {
                let rate = &self.rows.rate.get(i).payload;
                MetricValues::Rate(rate.scalar.unpack(flags, epoch), rate.interval)
            }
            Kind::Set => {
                let payload = &mut self.rows.set.get_mut(i).payload;
                let buckets = (flags != 0).then(|| {
                    (
                        decode_timestamp(epoch, payload.timestamp),
                        mem::take(&mut payload.value),
                    )
                });
                MetricValues::Set(SetPoints::from_buckets(buckets))
            }
            Kind::Histogram => {
                let payload = &self.rows.histogram.get(i).payload;
                let buckets = (flags != 0).then(|| {
                    let count = (flags & 0xff) - 1;
                    let histogram = Histogram::from_weighted_samples(
                        payload.value.samples[..usize::from(count)].iter().copied(),
                        payload.value.shared_weight,
                        flags & WEIGHTS_VARY != 0,
                    );
                    (decode_timestamp(epoch, payload.timestamp), histogram)
                });
                MetricValues::Histogram(buckets.into_iter().collect::<HistogramPoints>())
            }
            Kind::Distribution => {
                let payload = &self.rows.distribution.get(i).payload;
                let buckets = (flags != 0).then(|| {
                    let count = usize::from(flags - 1);
                    let data = &payload.value;
                    let [min, max, sum, avg] = data.summary;
                    let bins = data.keys[..count]
                        .iter()
                        .copied()
                        .zip(data.counts[..count].iter().copied());
                    let sketch = DDSketch::from_raw_parts(bins, data.count, min, max, sum, avg);
                    (decode_timestamp(epoch, payload.timestamp), sketch)
                });
                MetricValues::Distribution(buckets.into_iter().collect::<SketchPoints>())
            }
        }
    }

    /// Fills a new row or a row emptied by `take_values`.
    ///
    /// Pass values of the row's kind. A live overflow slot must already be released.
    /// When the values cannot fit inline, the row stores the index of a new overflow slot.
    pub(super) fn set_values(&mut self, slot: Slot, values: MetricValues, last_seen: u64) {
        let i = slot.index();
        let epoch = self.epoch.expect("a live row has an epoch");
        let flags = match &values {
            MetricValues::Gauge(points) => self.rows.gauge.get_mut(i).payload.pack(points, epoch),
            MetricValues::Counter(points) => {
                let payload = &mut self.rows.count.get_mut(i).payload;
                payload.last_seen = last_seen;
                payload.scalar.pack(points, epoch)
            }
            MetricValues::Rate(points, interval) => {
                let payload = &mut self.rows.rate.get_mut(i).payload;
                payload.interval = *interval;
                payload.scalar.pack(points, epoch)
            }
            MetricValues::Set(points) if points.len() == 1 => {
                // Borrowed iteration exposes the timestamp without cloning any member.
                if let Some(timestamp) = points
                    .into_iter()
                    .next()
                    .and_then(|(ts, _)| encode_timestamp(epoch, ts))
                {
                    let MetricValues::Set(points) = values else {
                        unreachable!()
                    };
                    let (_, value) = points.into_buckets().next().expect("there is one Set bucket");
                    let row = self.rows.set.get_mut(i);
                    row.payload = Single { value, timestamp };
                    row.header.flags = 1;
                    return;
                }
                None
            }
            MetricValues::Histogram(points) if points.len() == 1 => {
                let (timestamp, histogram) = points.into_iter().next().expect("there is one histogram bucket");
                let samples = histogram.samples();
                if let Some(timestamp) = encode_timestamp(epoch, timestamp).filter(|_| samples.len() <= 3) {
                    let payload = &mut self.rows.histogram.get_mut(i).payload;
                    payload.timestamp = timestamp;
                    let (shared_weight, weights_vary) = histogram.weighting();
                    payload.value.shared_weight = shared_weight;
                    for (dest, sample) in payload.value.samples.iter_mut().zip(samples) {
                        *dest = (sample.value.0, sample.weight.0);
                    }
                    Some((samples.len() as u16 + 1) | if weights_vary { WEIGHTS_VARY } else { 0 })
                } else {
                    None
                }
            }
            MetricValues::Distribution(points) if points.len() == 1 => {
                let (timestamp, sketch) = points.into_iter().next().expect("there is one sketch bucket");
                if let Some(timestamp) = encode_timestamp(epoch, timestamp).filter(|_| sketch.bin_count() <= 4) {
                    let payload = &mut self.rows.distribution.get_mut(i).payload;
                    payload.timestamp = timestamp;
                    payload.value.count = sketch.count();
                    payload.value.summary = [
                        sketch.stored_min(),
                        sketch.stored_max(),
                        sketch.stored_sum(),
                        sketch.stored_avg(),
                    ];
                    for (j, bin) in sketch.bins().iter().enumerate() {
                        payload.value.keys[j] = bin.key() as i16;
                        payload.value.counts[j] = bin.count();
                    }
                    Some(sketch.bin_count() as u16 + 1)
                } else {
                    None
                }
            }
            _ if values.is_empty() => Some(0),
            _ => None,
        };
        self.rows.header_mut(slot).flags = if let Some(flags) = flags {
            flags
        } else {
            *self.rows.auxiliary(slot) = self.overflow.insert(values);
            FALLBACK
        };
    }

    fn discard_values(&mut self, slot: Slot) {
        if self.header(slot).flags & FALLBACK != 0 {
            self.overflow.remove(*self.rows.auxiliary(slot));
        }
    }

    pub(super) fn remove(&mut self, slot: Slot) -> Header {
        self.discard_values(slot);
        let header = self.unlink(slot);
        self.metadata.remove(header.metadata_id);
        header
    }

    // Remove the index entry before moving the last row into its place. Repair the moved row's entry by old slot.
    fn unlink(&mut self, slot: Slot) -> Header {
        let hash = self.hash_builder.hash_one(&self.header(slot).context);
        self.index
            .find_entry(hash, |&id| id == slot.0)
            .expect("a live row is indexed")
            .remove();
        let header = self.rows.swap_remove(slot);
        let len = self.rows.len(slot.kind());
        if slot.index() < len {
            let from = Slot::new(slot.kind(), len);
            let hash = self.hash_builder.hash_one(&self.header(slot).context);
            *self
                .index
                .find_mut(hash, |&id| id == from.0)
                .expect("a moved row is indexed") = slot.0;
        }
        header
    }

    pub(super) fn trim_empty_side_stores(&mut self) {
        self.metadata.0.trim_if_empty();
        self.overflow.trim_if_empty();
    }
}

/// Encodes a nonzero timestamp as an unsigned offset from the storage epoch.
///
/// Missing timestamps and values outside the offset range require overflow storage.
/// Offset zero means the epoch itself, not a missing timestamp. A zero epoch cannot encode a real time at offset zero.
fn encode_timestamp(epoch: u64, timestamp: Option<NonZeroU64>) -> Option<u32> {
    u32::try_from(timestamp?.get().checked_sub(epoch)?).ok()
}

// Only decode an occupied timestamp field. Flags distinguish empty and overflow rows from inline offsets.
fn decode_timestamp(epoch: u64, timestamp: u32) -> Option<NonZeroU64> {
    NonZeroU64::new(epoch + u64::from(timestamp))
}

#[cfg(test)]
mod tests {
    use saluki_core::data_model::{event::metric::SampleRate, tags::TagSet};

    use super::*;

    #[test]
    fn row_sizes() {
        assert_eq!(mem::size_of::<Header>(), 16);
        assert_eq!(mem::size_of::<GaugeRow>(), 40);
        assert_eq!(mem::size_of::<CountRow>(), 48);
        assert_eq!(mem::size_of::<RateRow>(), 56);
        assert_eq!(mem::size_of::<SetRow>(), 56);
        assert_eq!(mem::size_of::<HistogramRow>(), 80);
        assert_eq!(mem::size_of::<DistributionRow>(), 88);
    }

    #[test]
    fn every_type_matches_metric_values_merge() {
        let context = Context::from_static_name("dense.roundtrip");
        let different = Context::from_static_name("dense.swap.tail");
        let first_metadata = MetricMetadata::default().with_origin(MetricOrigin::dogstatsd());
        let new_metadata = MetricMetadata::default()
            .with_source_type(Some("generic".into()))
            .with_unit("bytes");
        let mut storage = Storage::default();
        let mut histogram = Histogram::default();
        histogram.insert(-8.0, SampleRate::unsampled());
        histogram.insert(8.0, SampleRate::try_from(0.25).unwrap());
        let histogram = HistogramPoints::from_iter([(NonZeroU64::new(10), histogram)]);
        let rate = Duration::new(10, 17);
        let cases = [
            (MetricValues::gauge((10, 4.0)), MetricValues::gauge((10, 5.0))),
            (MetricValues::counter((10, -0.0)), MetricValues::counter((20, 2.0))),
            (MetricValues::rate((10, 6.0), rate), MetricValues::rate((10, 7.0), rate)),
            (
                MetricValues::rate((10, 6.0), rate),
                MetricValues::rate((11, 2.0), Duration::new(10, 18)),
            ),
            (MetricValues::set((10, "a")), MetricValues::set((10, ["a", "b"]))),
            (MetricValues::histogram(histogram), MetricValues::histogram((10, 3.0))),
            (
                MetricValues::distribution((10, [2.0, 3.0])),
                MetricValues::distribution((10, 5.0)),
            ),
        ];
        storage.insert(
            context.clone(),
            cases[0].0.clone(),
            first_metadata.clone(),
            10,
            OriginContextId::default(),
        );
        storage.insert(
            different.clone(),
            MetricValues::gauge((10, 42.0)),
            MetricMetadata::default(),
            10,
            OriginContextId::default(),
        );
        for (before, incoming) in cases {
            let mut expected = before.clone();
            expected.merge(incoming.clone());
            let slot = storage.find(&context).unwrap();
            drop(storage.take_values(slot));
            storage.update(slot, before, new_metadata.clone(), 10);
            let slot = storage.find(&context).unwrap();
            storage.update(slot, incoming, MetricMetadata::default().with_unit("ignored"), 20);
            assert_eq!(storage.find(&context), Some(slot));
            assert_eq!(storage.take_values(slot), expected);
            assert_eq!(storage.metadata(slot), first_metadata.clone().with_unit("bytes"));
            assert_eq!(storage.len(), 2);
        }
        let tail = storage.find(&different).unwrap();
        assert_eq!(storage.take_values(tail), MetricValues::gauge((10, 42.0)));
    }

    #[test]
    fn overflow_preserves_wide_timestamps_and_overlapping_buckets() {
        let mut storage = Storage::default();
        let context = Context::from_static_name("dense.overflow");
        let values = MetricValues::gauge([(1, 1.0), (2, 2.0), (3, 3.0), (u64::MAX, 4.0)]);
        storage.insert(
            context.clone(),
            values.clone(),
            MetricMetadata::default(),
            u64::MAX - 5,
            OriginContextId::default(),
        );
        let slot = storage.find(&context).unwrap();
        assert_eq!(storage.overflow.live, 1);
        let incoming = MetricValues::gauge((u64::MAX, 7.0));
        storage.update(slot, incoming, MetricMetadata::default(), u64::MAX);
        assert_eq!(storage.overflow.live, 1);
        let mut actual = storage.take_values(slot);
        assert_eq!(
            actual.split_at_timestamp(u64::MAX - 1),
            Some(MetricValues::gauge([(1, 1.0), (2, 2.0), (3, 3.0)]))
        );
        assert_eq!(actual, MetricValues::gauge((u64::MAX, 7.0)));
        storage.set_values(slot, actual, u64::MAX);
        assert_eq!(storage.overflow.live, 0);
        assert_eq!(storage.take_values(slot), MetricValues::gauge((u64::MAX, 7.0)));
    }

    #[test]
    fn warm_churn_reuses_empty_pages_and_the_thin_index() {
        let mut storage = Storage::default();
        let rows_per_page = Pages::<GaugeRow>::ROWS;
        let contexts = (0..4 * rows_per_page)
            .map(|i| Context::from_parts(format!("dense.churn.{i}"), TagSet::default()))
            .collect::<Vec<_>>();
        let insert = |storage: &mut Storage, context: &Context| {
            storage.insert(
                context.clone(),
                MetricValues::gauge((10, 1.0)),
                MetricMetadata::default(),
                10,
                OriginContextId::default(),
            );
        };
        for context in &contexts {
            insert(&mut storage, context);
        }
        let capacity = storage.index.capacity();
        for context in &contexts[1..] {
            let slot = storage.find(context).unwrap();
            assert_eq!(storage.remove(slot).context, *context);
        }
        assert_eq!(storage.len(), 1);
        assert_eq!(storage.rows.gauge.pages.len(), 1);
        assert_eq!(storage.rows.gauge.spare.len(), SPARE_PAGES);
        assert!(storage.index.capacity() <= capacity);
        let spare = storage.rows.gauge.spare.last().unwrap().as_ptr();
        for context in &contexts[1..] {
            insert(&mut storage, context);
        }
        assert_eq!(storage.len(), contexts.len());
        assert_eq!(storage.rows.gauge.pages[1].as_ptr(), spare);
        assert_eq!(storage.index.capacity(), capacity);
        for context in &contexts {
            assert_eq!(&storage.header(storage.find(context).unwrap()).context, context);
        }
    }
}
