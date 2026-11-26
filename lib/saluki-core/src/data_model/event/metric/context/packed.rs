//! The packed storage of a [`Context`](super::Context): one immutable allocation behind an 8-byte handle.
//!
//! A packed context stores its key, name, host, origin tags, and instrumented tags in one heap allocation. The handle,
//! [`Packed`], is one pointer. Recurring strings of 8 bytes or more become 4-byte [`SymbolTable`] IDs, and other
//! strings stay inline as UTF-8 bytes.
//!
//! # Design
//!
//! A context is immutable after construction. Thus, the allocation needs no lock, and only the reference counts in
//! the header change. A change to a context, such as a new tag, builds a new allocation.
//!
//! The allocation has four parts:
//!
//! 1. A fixed [`Header`] with the reference counts, the key, the resolver [`Domain`], and the lengths and flags that
//!    locate the other parts.
//! 2. A typed prefix with the optional origin tags and the optional strict-mode lease. `PrefixKind` names the
//!    `#[repr(C)]` struct that the allocation starts with, so the compiler computes every prefix offset.
//! 3. The inline bytes of the name and the host, if the name and host are not symbol IDs.
//! 4. The tag records, with a bitmap that marks ID records when inline and ID records mix.
//!
//! Each tag record is either a 4-byte symbol ID or a LEB128 length followed by the UTF-8 bytes of the tag. The
//! constructor picks the smaller form for the whole tag list: all IDs, all inline, or mixed with a bitmap.
//!
//! # Layout
//!
//! ```text
//!  Packed / WeakPacked (8 B)
//!  +-----+
//!  | ptr |----+
//!  +-----+    |
//!             v
//!  offset 0   +----------------------------------------------------------+
//!             | Header (56 B on 64-bit targets)                          |
//!             |   strong: AtomicUsize        weak: AtomicUsize           |
//!             |   key: ContextKey            domain: Option<Arc<Domain>> |
//!             |   bytes, name, host, tag_count, additions: u32           |
//!             |   flags: u8                  prefix: PrefixKind (u8)     |
//!  56         +----------------------------------------------------------+
//!             | typed prefix, selected by `Header::prefix`:              |
//!             |   None        => (no bytes)                              |
//!             |   Origin      => origin: TagSet                          |
//!             |   Lease       => lease: Arc<StrictLease>                 |
//!             |   OriginLease => origin: TagSet, lease: Arc<StrictLease> |
//!  prefix     +----------------------------------------------------------+
//!  size       | name bytes       (absent if NAME_ID)                     |
//!             | host bytes       (absent if no HOST, or if HOST_ID)      |
//!             | bitmap           (tag_count.div_ceil(8) B if MIXED)      |
//!             | tag records, in order:                                   |
//!             |   ID record      => u32 symbol ID, unaligned             |
//!             |   inline record  => LEB128 length, then UTF-8 bytes      |
//!  bytes      +----------------------------------------------------------+
//!             | padding to the 8-byte alignment of Header                |
//!             +----------------------------------------------------------+
//! ```
//!
//! For example, take a context with a symbol name and 51-byte tags, and no host, origin, or lease. These are the sizes
//! of its allocation:
//!
//! | Tags                                      | Records                         | Allocation |
//! | ----------------------------------------- | ------------------------------- | ---------- |
//! | none                                      | none                            | 56 B       |
//! | 8 recurring tags                          | 8 × 4 B IDs                     | 88 B       |
//! | 7 recurring tags and 1 unique tag         | 1 B bitmap, 7 × 4 B, 1 × 52 B   | 144 B      |
//! | 8 unique tags                             | 8 × 52 B inline                 | 472 B      |
//! | 50 recurring tags                         | 50 × 4 B IDs                    | 256 B      |
//!
//! The mixed case uses 137 bytes, and the allocator rounds it up to 144 bytes.

use std::{
    alloc::{alloc, dealloc, handle_alloc_error, Layout},
    mem::{offset_of, ManuallyDrop},
    ptr::NonNull,
    slice, str,
    sync::{
        atomic::{fence, AtomicUsize, Ordering},
        Arc, LazyLock,
    },
};

use hashbrown::HashSet;
use metrics::Gauge;
use saluki_common::hash::FastBuildHasher;
use smallvec::SmallVec;
use stringtheory::{
    interning::{Symbol, SymbolTable},
    MetaString,
};

use super::ContextKey;
use crate::data_model::tags::{SharedTagSet, Tag, TagSet, TagStr};

/// The context has a host.
const HOST: u8 = 1 << 0;

/// `Header::name` is a symbol ID. Otherwise, it is the length of the inline name bytes.
const NAME_ID: u8 = 1 << 1;

/// `Header::host` is a symbol ID. Otherwise, it is the length of the inline host bytes.
const HOST_ID: u8 = 1 << 2;

/// Every tag record is a symbol ID, so the allocation has no bitmap.
const ALL_IDS: u8 = 1 << 3;

/// The tag records mix IDs and inline tags. A bitmap before the records marks each ID record.
const MIXED: u8 = 1 << 4;

/// The context counts toward the active-context gauge of its domain.
const TRACKED: u8 = 1 << 5;

/// The tags came from a modified [`TagSet`]. [`TagSet::is_modified`] reports `true` for the tag set that a context
/// rebuilds from these tags.
const MODIFIED: u8 = 1 << 6;

/// The number of tags that the constructor checks for duplicates with a linear scan.
///
/// Above this number, the constructor uses a hash set. The resolver also collects up to this many tags on the stack
/// before it builds a context.
pub(super) const INLINE_TAG_CAPACITY: usize = 16;

/// The minimum length, in bytes, of a string that the constructor stores as a symbol ID.
///
/// A symbol saves the most for long recurring strings. A string shorter than 8 bytes needs at most 8 bytes inline,
/// with its length prefix. As a symbol, it needs a 4-byte ID, a table record, and a reference count update for each
/// context, so the saving is too small.
const MIN_SYMBOL_LEN: usize = 8;

/// The origin tags of a context that has no origin prefix.
static EMPTY_ORIGIN: LazyLock<TagSet> = LazyLock::new(TagSet::default);

/// The state that a resolver shares with every context that it builds.
pub(super) struct Domain {
    /// The symbol table that holds the recurring strings of the contexts.
    pub(super) symbols: Arc<SymbolTable>,

    /// The gauge of live contexts with the `TRACKED` flag.
    pub(super) active: Gauge,
}

/// The interned strings of a context that the resolver built while heap fallback was disabled.
///
/// The bounded interner counts each interned string against its capacity until the last reference to the string
/// drops. A strict context holds this lease, so its strings stay reserved in the interner for as long as the context
/// lives. The context still copies the bytes of these strings into its own allocation for fast reads.
pub(super) struct StrictLease {
    /// The interned name.
    pub(super) name: MetaString,

    /// The interned host, if the context has one.
    pub(super) host: Option<MetaString>,

    /// The interned tags.
    pub(super) tags: SharedTagSet,
}

impl StrictLease {
    /// Returns a lease for a rewritten context with the given name, host, and tags.
    ///
    /// Each value that matches a value in this lease reuses its interned string. Each other value becomes a new
    /// string that does not use the interner.
    pub(super) fn rewrite<'a>(&self, name: &str, host: Option<&str>, tags: impl Iterator<Item = &'a str>) -> Arc<Self> {
        let text = |old: Option<&MetaString>, value: &str| {
            old.filter(|old| old.as_ref() == value)
                .cloned()
                .unwrap_or_else(|| MetaString::from(value))
        };
        let tags = tags
            .map(|value| {
                (&self.tags)
                    .into_iter()
                    .find(|tag| tag.as_str() == value)
                    .cloned()
                    .unwrap_or_else(|| Tag::from(value))
            })
            .collect();
        Arc::new(Self {
            name: text(Some(&self.name), name),
            host: host.map(|host| text(self.host.as_ref(), host)),
            tags,
        })
    }
}

/// The inputs of [`Packed::new`], other than the instrumented tags.
pub(super) struct Parts<'a> {
    /// The key of the context.
    pub(super) key: ContextKey,

    /// The metric name.
    pub(super) name: &'a str,

    /// The host, if the context has one.
    pub(super) host: Option<&'a str>,

    /// The origin tags. The context stores them in its prefix only if they are not empty or are modified.
    pub(super) origin: TagSet,

    /// The resolver domain. Without a domain, the context stores every string inline.
    pub(super) domain: Option<&'a Arc<Domain>>,

    /// Whether the context counts toward the active-context gauge of `domain`.
    ///
    /// # Panics
    ///
    /// [`Packed::new`] panics if this is `true` and `domain` is `None`.
    pub(super) tracked: bool,

    /// The tag metadata to record and the deduplication mode.
    pub(super) metadata: TagMetadata,

    /// The strict-mode lease. A context with a lease stores every string inline and never uses the symbol table.
    pub(super) lease: Option<&'a Arc<StrictLease>>,
}

/// Metadata that a context keeps so it can rebuild an equivalent [`TagSet`].
#[derive(Clone, Copy, Default)]
pub(super) struct TagMetadata {
    /// The number of tags at the end of the list that a mutation added to the base tags.
    ///
    /// A rebuilt tag set puts these tags in its overlay, so they keep their priority over base tags with the same
    /// key.
    pub(super) additions: usize,

    /// Whether the source tag set was modified from its base.
    pub(super) modified: bool,

    /// Whether the constructor removes duplicate tags. The constructor never records this field.
    pub(super) deduplicate: bool,
}

impl TagMetadata {
    /// Returns the metadata of `tags`, without deduplication.
    pub(super) fn from_tag_set(tags: &TagSet) -> Self {
        Self {
            additions: tags.addition_count(),
            modified: tags.is_modified(),
            deduplicate: false,
        }
    }
}

/// The fixed start of every packed context allocation.
///
/// Only the reference counts change after construction. All other fields describe the rest of the allocation. See the
/// module documentation for the layout.
///
/// # Reference counts
///
/// `strong` and `weak` follow the protocol of [`Arc`]:
///
/// 1. A new context starts with `strong == 1` and `weak == 1`. All strong references together hold one implicit weak
///    reference.
/// 2. [`Packed::clone`] and [`Packed::downgrade`] add one with `Relaxed` ordering. A new reference comes from an
///    existing one, so nothing else needs to be ordered. The process aborts if a count reaches `isize::MAX`.
/// 3. A drop subtracts one with `Release` ordering. The thread that takes a count to zero runs an `Acquire` fence, so
///    it sees every write of the other owners before it tears anything down.
/// 4. When `strong` reaches zero, the last strong owner tears down the payload in this order: it releases the symbol
///    IDs of the name, the host, and the tags; it decrements the active-context gauge; it drops the lease and then the
///    origin tags; and it drops the domain. Then it drops the implicit weak reference.
/// 5. When `weak` reaches zero, the last owner frees the allocation. Until then, only the counts and the layout fields
///    stay valid.
///
/// [`WeakPacked::upgrade`] uses a compare-and-swap loop. It never raises `strong` from zero, so a torn-down payload
/// never becomes reachable again. `strong_count` loads with `Relaxed` ordering: callers use it only as a hint, or
/// under a lock that stops new strong references.
#[repr(C)]
struct Header {
    /// The number of strong references.
    strong: AtomicUsize,

    /// The number of weak references, plus one while any strong reference exists.
    weak: AtomicUsize,

    /// The key of the context.
    key: ContextKey,

    /// The resolver domain, if the context has one. The last strong owner drops it.
    domain: ManuallyDrop<Option<Arc<Domain>>>,

    /// The length of the allocation in bytes, before padding to the alignment of the header.
    bytes: u32,

    /// The symbol ID of the name if `NAME_ID` is set. Otherwise, the length of the inline name bytes.
    name: u32,

    /// The symbol ID of the host if `HOST_ID` is set. Otherwise, the length of the inline host bytes, which is 0 when
    /// the context has no host.
    host: u32,

    /// The number of tag records.
    tag_count: u32,

    /// The number of tags at the end of the list that a mutation added. See [`TagMetadata::additions`].
    additions: u32,

    /// The flag bits, such as `HOST` and `MIXED`.
    flags: u8,

    /// The kind of typed prefix that follows the header.
    ///
    /// This byte uses header padding, so the header stays 56 bytes.
    prefix: PrefixKind,
}

/// The kind of typed prefix that starts a packed context allocation.
///
/// Each variant names one `#[repr(C)]` struct that begins with [`Header`]. The accessors cast the allocation to that
/// struct, so the compiler computes every field offset. The `const` assertions below check that each struct adds no
/// padding. Thus, a packed context uses exactly the bytes of its header and its present prefix fields.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PrefixKind {
    /// The allocation starts with [`Header`] only.
    None,

    /// The allocation starts with [`WithOrigin`].
    Origin,

    /// The allocation starts with [`WithLease`].
    Lease,

    /// The allocation starts with [`WithOriginLease`].
    OriginLease,
}

impl PrefixKind {
    /// Returns the prefix kind for a context with or without origin tags and a lease.
    const fn new(origin: bool, lease: bool) -> Self {
        match (origin, lease) {
            (false, false) => Self::None,
            (true, false) => Self::Origin,
            (false, true) => Self::Lease,
            (true, true) => Self::OriginLease,
        }
    }

    /// Returns the size of the header and the prefix fields, which is the offset of the inline bytes.
    const fn size(self) -> usize {
        match self {
            Self::None => size_of::<Header>(),
            Self::Origin => size_of::<WithOrigin>(),
            Self::Lease => size_of::<WithLease>(),
            Self::OriginLease => size_of::<WithOriginLease>(),
        }
    }
}

/// The prefix of a context with origin tags.
#[repr(C)]
struct WithOrigin {
    header: Header,
    /// The origin tags. The last strong owner drops them.
    origin: ManuallyDrop<TagSet>,
}

/// The prefix of a strict-mode context without origin tags.
#[repr(C)]
struct WithLease {
    header: Header,
    /// The strict-mode lease. The last strong owner drops it.
    lease: ManuallyDrop<Arc<StrictLease>>,
}

/// The prefix of a strict-mode context with origin tags.
#[repr(C)]
struct WithOriginLease {
    header: Header,
    /// The origin tags. The last strong owner drops them after the lease.
    origin: ManuallyDrop<TagSet>,
    /// The strict-mode lease. The last strong owner drops it first.
    lease: ManuallyDrop<Arc<StrictLease>>,
}

// Each prefix struct must share the alignment of `Header`, because `Header::layout` uses that alignment for every
// allocation. Each prefix field must follow the previous field with no padding, so the typed prefix uses no more
// bytes than its fields.
const _: () = {
    assert!(align_of::<WithOrigin>() == align_of::<Header>());
    assert!(align_of::<WithLease>() == align_of::<Header>());
    assert!(align_of::<WithOriginLease>() == align_of::<Header>());
    assert!(offset_of!(WithOrigin, origin) == size_of::<Header>());
    assert!(size_of::<WithOrigin>() == size_of::<Header>() + size_of::<TagSet>());
    assert!(offset_of!(WithLease, lease) == size_of::<Header>());
    assert!(size_of::<WithLease>() == size_of::<Header>() + size_of::<Arc<StrictLease>>());
    assert!(offset_of!(WithOriginLease, origin) == size_of::<Header>());
    assert!(offset_of!(WithOriginLease, lease) == size_of::<Header>() + size_of::<TagSet>());
    assert!(size_of::<WithOriginLease>() == size_of::<Header>() + size_of::<TagSet>() + size_of::<Arc<StrictLease>>());
    #[cfg(target_pointer_width = "64")]
    assert!(size_of::<Header>() == 56);
};

// `Packed` and `WeakPacked` share their payload between threads. Every payload type must thus be `Send + Sync`.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ContextKey>();
    assert_send_sync::<Arc<Domain>>();
    assert_send_sync::<TagSet>();
    assert_send_sync::<Arc<StrictLease>>();
};

impl Header {
    /// Returns the allocation layout for `bytes` bytes, padded to the alignment of the header.
    fn layout(bytes: usize) -> Layout {
        Layout::from_size_align(bytes, align_of::<Self>())
            .unwrap()
            .pad_to_align()
    }

    /// Returns `true` if any bit of `flag` is set.
    fn has(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }

    /// Returns the symbol table of the domain.
    ///
    /// # Panics
    ///
    /// Panics if the context has no domain. A context stores symbol IDs only if it has a domain.
    fn symbols(&self) -> &SymbolTable {
        &self.domain.as_ref().expect("symbol has a resolver domain").symbols
    }

}

/// A strong reference to a packed context allocation.
///
/// See [`Header`] for the reference count protocol.
pub(super) struct Packed(NonNull<Header>);

// SAFETY: The allocation is immutable except for atomic reference counts. The payload types are `Send + Sync`, as the
// static assertions above check.
unsafe impl Send for Packed {}
// SAFETY: Shared access only reads immutable fields and updates atomic counts.
unsafe impl Sync for Packed {}

impl Packed {
    /// Builds a packed context from `parts` and the instrumented `tags`.
    ///
    /// Each string of `MIN_SYMBOL_LEN` bytes or more becomes a symbol ID if the domain symbol table admits it. A
    /// context with a lease stores every string inline. The constructor stores the tags as all IDs, all inline, or
    /// mixed, whichever is smallest.
    ///
    /// # Panics
    ///
    /// Panics if the allocation length, a string length, or the tag count does not fit in a `u32`, or if
    /// `parts.tracked` is `true` without a domain.
    pub(super) fn new<'a>(parts: Parts<'_>, tags: impl IntoIterator<Item = &'a str>) -> Self {
        let Parts {
            key,
            name,
            host,
            origin,
            domain,
            tracked,
            metadata,
            lease,
        } = parts;
        let intern = |text: &str| -> Option<Symbol<'_>> {
            if lease.is_some() || text.len() < MIN_SYMBOL_LEN {
                return None;
            }
            domain?.symbols.try_intern(text)
        };
        let name_symbol = intern(name);
        let host_symbol = host.and_then(intern);
        let mut prepared: SmallVec<[(&str, Option<Symbol<'_>>); INLINE_TAG_CAPACITY]> = SmallVec::new();
        let mut ids = 0;
        let mut all_inline_bytes = 0;
        let mut tag_bytes = 0;
        let tags = tags.into_iter();
        let mut seen = (metadata.deduplicate && tags.size_hint().0 > INLINE_TAG_CAPACITY)
            .then(HashSet::<&str, FastBuildHasher>::default);
        for text in tags {
            if metadata.deduplicate {
                let duplicate = match &mut seen {
                    Some(seen) => !seen.insert(text),
                    None => prepared.iter().any(|(entry, _)| *entry == text),
                };
                if duplicate {
                    continue;
                }
            }
            let symbol = intern(text);
            let inline_bytes = leb_size(text.len()) + text.len();
            all_inline_bytes += inline_bytes;
            tag_bytes += if symbol.is_some() {
                ids += 1;
                4
            } else {
                inline_bytes
            };
            prepared.push((text, symbol));
        }
        let count = prepared.len();
        let mut flags = 0;
        let mut bitmap_bytes = 0;
        if ids > 0 {
            if ids == count {
                flags |= ALL_IDS;
            } else if tag_bytes + count.div_ceil(8) < all_inline_bytes {
                flags |= MIXED;
                bitmap_bytes = count.div_ceil(8);
            } else {
                // Mixed records with a bitmap would not be smaller. Store every tag inline, and drop the symbols.
                ids = 0;
                tag_bytes = all_inline_bytes;
            }
        }
        let has_origin = !origin.is_empty() || origin.is_modified();
        for (enabled, flag) in [
            (host.is_some(), HOST),
            (name_symbol.is_some(), NAME_ID),
            (host_symbol.is_some(), HOST_ID),
            (tracked, TRACKED),
            (metadata.modified, MODIFIED),
        ] {
            if enabled {
                flags |= flag;
            }
        }
        let prefix = PrefixKind::new(has_origin, lease.is_some());
        let name_bytes = if name_symbol.is_none() { name.len() } else { 0 };
        let host_bytes = if host_symbol.is_none() {
            host.map_or(0, str::len)
        } else {
            0
        };
        let bytes = prefix.size() + name_bytes + host_bytes + bitmap_bytes + tag_bytes;
        let layout = Header::layout(bytes);
        let recorded_bytes = u32::try_from(bytes).expect("packed context length fits in u32");
        let header = Header {
            strong: AtomicUsize::new(1),
            weak: AtomicUsize::new(1),
            key,
            domain: ManuallyDrop::new(domain.cloned()),
            bytes: recorded_bytes,
            name: name_symbol.map_or_else(
                || name.len().try_into().expect("name length fits in u32"),
                Symbol::into_id,
            ),
            host: host_symbol.map_or_else(
                || host_bytes.try_into().expect("host length fits in u32"),
                Symbol::into_id,
            ),
            tag_count: count.try_into().expect("tag count fits in u32"),
            additions: metadata
                .additions
                .min(count)
                .try_into()
                .expect("addition count fits in u32"),
            flags,
            prefix,
        };
        // SAFETY: The layout is nonzero and has the alignment of every prefix struct. `prefix` names the struct that
        // the allocation starts with, and each tail region is disjoint and sized above.
        let ptr = unsafe {
            let ptr = alloc(layout);
            let header_ptr = NonNull::new(ptr.cast::<Header>()).unwrap_or_else(|| handle_alloc_error(layout));
            match (has_origin.then_some(origin), lease) {
                (None, None) => header_ptr.as_ptr().write(header),
                (Some(origin), None) => ptr.cast::<WithOrigin>().write(WithOrigin {
                    header,
                    origin: ManuallyDrop::new(origin),
                }),
                (None, Some(lease)) => ptr.cast::<WithLease>().write(WithLease {
                    header,
                    lease: ManuallyDrop::new(Arc::clone(lease)),
                }),
                (Some(origin), Some(lease)) => ptr.cast::<WithOriginLease>().write(WithOriginLease {
                    header,
                    origin: ManuallyDrop::new(origin),
                    lease: ManuallyDrop::new(Arc::clone(lease)),
                }),
            }
            let mut cursor = ptr.add(prefix.size());
            if name_bytes != 0 {
                cursor.copy_from_nonoverlapping(name.as_ptr(), name_bytes);
                cursor = cursor.add(name_bytes);
            }
            if host_bytes != 0 {
                cursor.copy_from_nonoverlapping(host.unwrap().as_ptr(), host_bytes);
                cursor = cursor.add(host_bytes);
            }
            let bitmap = cursor;
            bitmap.write_bytes(0, bitmap_bytes);
            cursor = cursor.add(bitmap_bytes);
            for (index, (text, symbol)) in prepared.into_iter().enumerate() {
                if let Some(symbol) = symbol.filter(|_| ids != 0) {
                    if bitmap_bytes != 0 {
                        *bitmap.add(index / 8) |= 1 << (index % 8);
                    }
                    // The context takes over the symbol reference. `Packed::drop` releases it.
                    cursor.cast::<u32>().write_unaligned(symbol.into_id());
                    cursor = cursor.add(4);
                } else {
                    cursor = write_leb(cursor, text.len());
                    cursor.copy_from_nonoverlapping(text.as_ptr(), text.len());
                    cursor = cursor.add(text.len());
                }
            }
            debug_assert_eq!(cursor, ptr.add(bytes));
            header_ptr
        };
        if tracked {
            domain.unwrap().active.increment(1);
        }
        Self(ptr)
    }

    /// Returns the header.
    fn header(&self) -> &Header {
        // SAFETY: A strong reference keeps the header and payload live.
        unsafe { self.0.as_ref() }
    }

    /// Returns the key of the context.
    pub(super) fn key(&self) -> ContextKey {
        self.header().key
    }

    /// Returns `true` if both handles point to the same allocation.
    pub(super) fn ptr_eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }

    /// Returns the size of the allocation in bytes, with padding.
    pub(super) fn allocation_bytes(&self) -> usize {
        Header::layout(self.header().bytes as usize).size()
    }

    /// Returns the resolver domain, if the context has one.
    pub(super) fn domain(&self) -> Option<&Arc<Domain>> {
        self.header().domain.as_ref()
    }

    /// Returns the number of strong references.
    ///
    /// The load uses `Relaxed` ordering. Another thread can change the count right after the load, so use the result
    /// only as a hint, or under a lock that stops new strong references.
    pub(super) fn strong_count(&self) -> usize {
        self.header().strong.load(Ordering::Relaxed)
    }

    /// Returns a weak reference to this allocation.
    pub(super) fn downgrade(&self) -> WeakPacked {
        increment(&self.header().weak);
        WeakPacked(self.0)
    }

    /// Returns `true` if a rewritten copy of this context takes over its place in the active-context gauge.
    ///
    /// This is the case for a tracked context with no other strong owner. The rewrite replaces and drops this
    /// context, so the gauge stays the same. A copy of a shared context does not count, so copies never inflate the
    /// gauge.
    pub(super) fn track_rewrite(&self) -> bool {
        self.header().has(TRACKED) && self.strong_count() == 1
    }

    /// Returns the name.
    pub(super) fn name(&self) -> &str {
        let header = self.header();
        if header.has(NAME_ID) {
            // SAFETY: The context owns this symbol reference.
            unsafe { header.symbols().get_unchecked(header.name) }
        } else {
            // SAFETY: The name consists of UTF-8 bytes copied into the tail.
            unsafe { str::from_utf8_unchecked(&self.tail()[..header.name as usize]) }
        }
    }

    /// Returns the host, if the context has one.
    pub(super) fn host(&self) -> Option<&str> {
        let header = self.header();
        if !header.has(HOST) {
            return None;
        }
        Some(if header.has(HOST_ID) {
            // SAFETY: The context owns this symbol reference.
            unsafe { header.symbols().get_unchecked(header.host) }
        } else {
            let start = self.name_bytes();
            // SAFETY: The host consists of UTF-8 bytes copied into the tail.
            unsafe { str::from_utf8_unchecked(&self.tail()[start..start + header.host as usize]) }
        })
    }

    /// Returns the origin tags, or an empty tag set if the context has none.
    pub(super) fn origin(&self) -> &TagSet {
        let ptr = self.0.as_ptr();
        // SAFETY: `prefix` names the struct that the allocation starts with. A strong reference keeps its fields live.
        unsafe {
            match self.header().prefix {
                PrefixKind::Origin => &(*ptr.cast::<WithOrigin>()).origin,
                PrefixKind::OriginLease => &(*ptr.cast::<WithOriginLease>()).origin,
                PrefixKind::None | PrefixKind::Lease => &EMPTY_ORIGIN,
            }
        }
    }

    /// Returns the strict-mode lease, if the context has one.
    pub(super) fn lease(&self) -> Option<&Arc<StrictLease>> {
        let ptr = self.0.as_ptr();
        // SAFETY: `prefix` names the struct that the allocation starts with. A strong reference keeps its fields live.
        unsafe {
            match self.header().prefix {
                PrefixKind::Lease => Some(&*(*ptr.cast::<WithLease>()).lease),
                PrefixKind::OriginLease => Some(&*(*ptr.cast::<WithOriginLease>()).lease),
                PrefixKind::None | PrefixKind::Origin => None,
            }
        }
    }

    /// Returns the number of instrumented tags.
    pub(super) fn tag_count(&self) -> usize {
        self.header().tag_count as usize
    }

    /// Returns the recorded tag metadata. The `deduplicate` field is always `false`.
    pub(super) fn metadata(&self) -> TagMetadata {
        TagMetadata {
            additions: self.header().additions as usize,
            modified: self.header().has(MODIFIED),
            deduplicate: false,
        }
    }

    /// Returns an iterator over the instrumented tags, in their original order.
    pub(super) fn tag_iter(&self) -> TagIter<'_> {
        TagIter {
            header: self.header(),
            raw: self.raw_tags(),
        }
    }

    /// Returns the number of inline name bytes, which is 0 if the name is a symbol ID.
    fn name_bytes(&self) -> usize {
        let header = self.header();
        if header.has(NAME_ID) {
            0
        } else {
            header.name as usize
        }
    }

    /// Returns the bytes after the typed prefix: the inline name and host, the bitmap, and the tag records.
    fn tail(&self) -> &[u8] {
        let header = self.header();
        let start = header.prefix.size();
        // SAFETY: The byte tail is fully initialized and excludes the typed prefix and padding.
        unsafe { slice::from_raw_parts(self.0.as_ptr().cast::<u8>().add(start), header.bytes as usize - start) }
    }

    /// Returns an iterator over the raw tag records.
    fn raw_tags(&self) -> RawTags<'_> {
        let header = self.header();
        let host_bytes = if header.has(HOST_ID) { 0 } else { header.host as usize };
        let tags = &self.tail()[self.name_bytes() + host_bytes..];
        let count = self.tag_count();
        let bitmap_bytes = if header.has(MIXED) { count.div_ceil(8) } else { 0 };
        let (bitmap, payload) = tags.split_at(bitmap_bytes);
        RawTags {
            bitmap,
            payload,
            count,
            index: 0,
            all_ids: header.has(ALL_IDS),
        }
    }
}

impl Clone for Packed {
    fn clone(&self) -> Self {
        increment(&self.header().strong);
        Self(self.0)
    }
}

impl Drop for Packed {
    fn drop(&mut self) {
        let header = self.header();
        if header.strong.fetch_sub(1, Ordering::Release) != 1 {
            return;
        }
        fence(Ordering::Acquire);
        let prefix = header.prefix;
        // SAFETY: The last strong reference owns the payload. The implicit weak reference keeps allocation live.
        // `prefix` names the struct that the allocation starts with. No weak reference reads the payload.
        unsafe {
            if let Some(domain) = self.domain() {
                if header.has(NAME_ID) {
                    domain.symbols.release(header.name);
                }
                if header.has(HOST_ID) {
                    domain.symbols.release(header.host);
                }
                if header.has(ALL_IDS | MIXED) {
                    for tag in self.raw_tags() {
                        if let RawTag::Id(id) = tag {
                            domain.symbols.release(id);
                        }
                    }
                }
                if header.has(TRACKED) {
                    domain.active.decrement(1);
                }
            }
            let ptr = self.0.as_ptr();
            match prefix {
                PrefixKind::None => {}
                PrefixKind::Origin => ManuallyDrop::drop(&mut (*ptr.cast::<WithOrigin>()).origin),
                PrefixKind::Lease => ManuallyDrop::drop(&mut (*ptr.cast::<WithLease>()).lease),
                PrefixKind::OriginLease => {
                    let prefix = ptr.cast::<WithOriginLease>();
                    ManuallyDrop::drop(&mut (*prefix).lease);
                    ManuallyDrop::drop(&mut (*prefix).origin);
                }
            }
            ManuallyDrop::drop(&mut (*ptr).domain);
        }
        drop(WeakPacked(self.0));
    }
}

/// A weak reference to a packed context allocation.
///
/// A weak reference keeps the allocation, but not the payload. See [`Header`] for the reference count protocol.
pub(super) struct WeakPacked(NonNull<Header>);

// SAFETY: Weak references only inspect immutable layout metadata and atomic counts until they upgrade.
unsafe impl Send for WeakPacked {}
// SAFETY: Shared access only reads the atomic strong count.
unsafe impl Sync for WeakPacked {}

impl WeakPacked {
    /// Returns the number of strong references. A count of 0 means that the payload is gone.
    ///
    /// The load uses `Relaxed` ordering, so the result is a hint. A nonzero count can drop to 0 right after the load.
    pub(super) fn strong_count(&self) -> usize {
        // SAFETY: The weak reference keeps the count live.
        unsafe { (*self.0.as_ptr()).strong.load(Ordering::Relaxed) }
    }

    /// Returns a strong reference, or `None` if the last strong reference has dropped.
    pub(super) fn upgrade(&self) -> Option<Packed> {
        // SAFETY: The weak reference keeps the count live. A zero count cannot be resurrected.
        let strong = unsafe { &(*self.0.as_ptr()).strong };
        strong
            .fetch_update(Ordering::Acquire, Ordering::Relaxed, |count| {
                if count == 0 {
                    return None;
                }
                if count >= isize::MAX as usize {
                    std::process::abort();
                }
                Some(count + 1)
            })
            .ok()?;
        Some(Packed(self.0))
    }
}

impl Drop for WeakPacked {
    fn drop(&mut self) {
        // SAFETY: The weak reference keeps these fields live. The last reference can free the allocation.
        unsafe {
            let ptr = self.0.as_ptr();
            if (*ptr).weak.fetch_sub(1, Ordering::Release) == 1 {
                fence(Ordering::Acquire);
                dealloc(ptr.cast(), Header::layout((*ptr).bytes as usize));
            }
        }
    }
}

/// Adds one to a reference count, and aborts the process if the count reaches `isize::MAX`.
///
/// `Relaxed` is enough: the caller already holds a reference, so the allocation is live.
#[inline]
fn increment(count: &AtomicUsize) {
    if count.fetch_add(1, Ordering::Relaxed) >= isize::MAX as usize {
        std::process::abort();
    }
}

/// Returns the number of bytes that the unsigned LEB128 encoding of `n` uses.
///
/// Each byte holds 7 bits of the value, and 0 uses one byte.
fn leb_size(n: usize) -> usize {
    ((usize::BITS - n.leading_zeros()).max(1) as usize).div_ceil(7)
}

/// Writes `n` at `cursor` in unsigned LEB128 encoding, and returns the pointer after the last byte.
///
/// Each byte holds 7 bits of the value, low bits first. The high bit of each byte except the last is set.
///
/// # Safety
///
/// The caller must make sure that `cursor` is valid for writes of `leb_size(n)` bytes.
unsafe fn write_leb(mut cursor: *mut u8, mut n: usize) -> *mut u8 {
    // SAFETY: The caller reserves `leb_size(n)` bytes, and the loop writes exactly that many.
    unsafe {
        while n >= 128 {
            cursor.write(n as u8 | 0x80);
            cursor = cursor.add(1);
            n >>= 7;
        }
        cursor.write(n as u8);
        cursor.add(1)
    }
}

/// An iterator over the raw tag records of a packed context.
#[derive(Clone)]
struct RawTags<'a> {
    /// The bitmap that marks ID records. It is empty unless the records are mixed.
    bitmap: &'a [u8],

    /// The remaining record bytes.
    payload: &'a [u8],

    /// The number of records.
    count: usize,

    /// The index of the next record.
    index: usize,

    /// Whether every record is an ID.
    all_ids: bool,
}

/// One raw tag record.
enum RawTag<'a> {
    /// A symbol ID.
    Id(u32),

    /// An inline tag.
    Inline(&'a str),
}

impl<'a> Iterator for RawTags<'a> {
    type Item = RawTag<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index == self.count {
            return None;
        }
        let index = self.index;
        self.index += 1;
        if self.all_ids || (!self.bitmap.is_empty() && self.bitmap[index / 8] & (1 << (index % 8)) != 0) {
            let (id, rest) = self.payload.split_first_chunk::<4>().unwrap();
            self.payload = rest;
            return Some(RawTag::Id(u32::from_ne_bytes(*id)));
        }
        let mut len = 0;
        let mut shift = 0;
        loop {
            let (&byte, rest) = self.payload.split_first().unwrap();
            self.payload = rest;
            len |= usize::from(byte & 0x7f) << shift;
            if byte < 0x80 {
                break;
            }
            shift += 7;
        }
        let (text, rest) = self.payload.split_at(len);
        self.payload = rest;
        // SAFETY: Construction copies each tag from a valid UTF-8 string.
        Some(RawTag::Inline(unsafe { str::from_utf8_unchecked(text) }))
    }
}

/// An iterator over the instrumented tags of a packed context, as borrowed [`TagStr`] values.
///
/// The iterator resolves each ID record through the symbol table of the context.
#[derive(Clone)]
pub(super) struct TagIter<'a> {
    /// The header of the context. It gives access to the symbol table.
    header: &'a Header,

    /// The raw records.
    raw: RawTags<'a>,
}

impl<'a> Iterator for TagIter<'a> {
    type Item = &'a TagStr;

    fn next(&mut self) -> Option<Self::Item> {
        Some(TagStr::new(match self.raw.next()? {
            RawTag::Inline(text) => text,
            // SAFETY: The context owns a reference to this symbol for the whole borrow.
            RawTag::Id(id) => unsafe { self.header.symbols().get_unchecked(id) },
        }))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.raw.count - self.raw.index;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for TagIter<'_> {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_prefix_kind_round_trips_its_fields() {
        let origin: TagSet = [Tag::from("origin:a")].into_iter().collect();
        let lease = Arc::new(StrictLease {
            name: MetaString::from("metric"),
            host: None,
            tags: SharedTagSet::default(),
        });
        for (with_origin, with_lease, kind) in [
            (false, false, PrefixKind::None),
            (true, false, PrefixKind::Origin),
            (false, true, PrefixKind::Lease),
            (true, true, PrefixKind::OriginLease),
        ] {
            let origin = if with_origin { origin.clone() } else { TagSet::default() };
            let packed = Packed::new(
                Parts {
                    key: super::super::hash_context("metric", ["env:prod"], &origin).0,
                    name: "metric",
                    host: Some("host"),
                    origin,
                    domain: None,
                    tracked: false,
                    metadata: TagMetadata::default(),
                    lease: with_lease.then_some(&lease),
                },
                ["env:prod"],
            );
            assert_eq!(packed.header().prefix, kind);
            assert_eq!(packed.origin().has_tag("origin:a"), with_origin);
            assert_eq!(packed.lease().is_some_and(|l| Arc::ptr_eq(l, &lease)), with_lease);
            assert_eq!((packed.name(), packed.host()), ("metric", Some("host")));
            assert_eq!(packed.tag_iter().map(TagStr::as_str).collect::<Vec<_>>(), ["env:prod"]);
            let inline = "metric".len() + "host".len() + 1 + "env:prod".len();
            assert_eq!(packed.header().bytes as usize, kind.size() + inline);
        }
        assert_eq!(Arc::strong_count(&lease), 1);
    }
}
