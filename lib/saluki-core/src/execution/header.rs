//! Batch headers.

use std::{fmt, time::Duration};

use super::generation::{GenerationId, GenerationRef};

/// The time at which a batch was admitted, as nanoseconds since the Unix epoch.
///
/// An admission time is stamped once per batch, at the point where the batch enters plan-driven processing, and is
/// never changed afterwards by anything downstream of that point. Every decision that depends on host time uses it
/// rather than the time at which the processing runs, so that processing a batch early, late, or in a different
/// grouping makes the same decisions.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AdmissionTime(u64);

impl AdmissionTime {
    /// The Unix epoch.
    pub const UNIX_EPOCH: Self = Self(0);

    /// Creates an admission time from nanoseconds since the Unix epoch.
    pub const fn from_unix_nanos(nanos: u64) -> Self {
        Self(nanos)
    }

    /// Creates an admission time from whole seconds since the Unix epoch.
    ///
    /// Saturates at the largest representable admission time, which is in the year 2554.
    pub const fn from_unix_secs(secs: u64) -> Self {
        Self(secs.saturating_mul(1_000_000_000))
    }

    /// Returns the number of nanoseconds since the Unix epoch.
    pub const fn as_unix_nanos(self) -> u64 {
        self.0
    }

    /// Returns the number of whole seconds since the Unix epoch, rounding down.
    pub const fn as_unix_secs(self) -> u64 {
        self.0 / 1_000_000_000
    }

    /// Returns this admission time advanced by `duration`, saturating at the largest representable admission time.
    pub fn saturating_add(self, duration: Duration) -> Self {
        let nanos = u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);
        Self(self.0.saturating_add(nanos))
    }
}

impl fmt::Display for AdmissionTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let secs = self.0 / 1_000_000_000;
        let nanos = self.0 % 1_000_000_000;
        write!(f, "{}.{:09}", secs, nanos)
    }
}

/// The identifier of an admission point.
///
/// An admission point is a place where batches enter plan-driven processing and are stamped with an admission time,
/// such as a source's output or the output of a component whose processing the plan cannot see into. Each admission
/// point keeps its own admission clock.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AdmissionPointId(u32);

impl AdmissionPointId {
    /// Creates an admission point identifier.
    pub const fn new(id: u32) -> Self {
        Self(id)
    }

    /// Returns the numeric value of this identifier.
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// The identifier of a logical entry into a region's program.
///
/// A region can receive batches for several logical inputs over one physical edge. When one region dispatches a batch
/// to another, it sets the entry so that the consumer knows which of its logical inputs the batch belongs to under the
/// batch's generation. Entry identifiers are only meaningful within one generation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EntryId(u16);

impl EntryId {
    /// Creates an entry identifier.
    pub const fn new(id: u16) -> Self {
        Self(id)
    }

    /// Returns the numeric value of this identifier.
    pub const fn get(self) -> u16 {
        self.0
    }
}

/// Per-batch metadata that travels with a batch of events through plan-driven processing.
///
/// An admission point stamps the header once when the batch enters: its admission time, which admission point stamped
/// it, that point's clock epoch, and the generation that processes the batch. Processing carries the header unchanged
/// and in order. The only field set downstream of the admission point is [`entry`][Self::entry].
///
/// A header with no admission time is **unstamped**, which is also the default. An empty batch whose header is stamped
/// carries no events, only progress: a heartbeat from an idle admission point, for example.
///
/// Cloning a header is cheap: the generation is reference-counted, and holding a header keeps its generation alive.
#[derive(Clone, Debug, Default)]
pub struct BatchHeader {
    /// Admission time, stamped once at the admission point.
    ///
    /// `None` means the batch is unstamped.
    pub admitted_at: Option<AdmissionTime>,

    /// The admission point that stamped the admission time.
    pub admission_point: Option<AdmissionPointId>,

    /// The admission point's clock epoch.
    ///
    /// The epoch increments every time the admission point's clock is reset after a large backward jump in wall-clock
    /// time, so admission times are only comparable within one epoch of one admission point.
    pub epoch: u32,

    /// The generation that processes this batch, stamped at the admission point.
    pub generation: Option<GenerationRef>,

    /// The consumer's logical entry, set by a region when it dispatches the batch to another region.
    pub entry: Option<EntryId>,
}

impl BatchHeader {
    /// Returns `true` if this header carries an admission time.
    pub fn is_stamped(&self) -> bool {
        self.admitted_at.is_some()
    }

    /// Returns the identifier of the generation that processes this batch, if one was stamped.
    pub fn generation_id(&self) -> Option<GenerationId> {
        self.generation.as_ref().map(|generation| generation.id())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::execution::generation::GenerationToken;

    #[test]
    fn admission_time_converts_between_units() {
        let time = AdmissionTime::from_unix_secs(1_700_000_000);
        assert_eq!(time.as_unix_nanos(), 1_700_000_000_000_000_000);
        assert_eq!(time.as_unix_secs(), 1_700_000_000);

        // Seconds round down.
        let time = AdmissionTime::from_unix_nanos(1_999_999_999);
        assert_eq!(time.as_unix_secs(), 1);

        // Conversion from seconds saturates instead of overflowing.
        assert_eq!(AdmissionTime::from_unix_secs(u64::MAX).as_unix_nanos(), u64::MAX);
    }

    #[test]
    fn admission_time_adds_durations_with_saturation() {
        let time = AdmissionTime::from_unix_secs(10);
        assert_eq!(
            time.saturating_add(Duration::from_millis(1500)),
            AdmissionTime::from_unix_nanos(11_500_000_000)
        );
        assert_eq!(
            AdmissionTime::from_unix_nanos(u64::MAX - 1).saturating_add(Duration::from_secs(1)),
            AdmissionTime::from_unix_nanos(u64::MAX)
        );
        assert_eq!(
            time.saturating_add(Duration::MAX),
            AdmissionTime::from_unix_nanos(u64::MAX)
        );
    }

    #[test]
    fn admission_time_orders_and_displays_by_instant() {
        assert!(AdmissionTime::UNIX_EPOCH < AdmissionTime::from_unix_nanos(1));
        assert_eq!(
            AdmissionTime::from_unix_nanos(12_000_000_345).to_string(),
            "12.000000345"
        );
        assert_eq!(AdmissionTime::UNIX_EPOCH.to_string(), "0.000000000");
    }

    #[test]
    fn default_header_is_unstamped() {
        let header = BatchHeader::default();
        assert!(!header.is_stamped());
        assert_eq!(header.admission_point, None);
        assert_eq!(header.epoch, 0);
        assert_eq!(header.generation_id(), None);
        assert_eq!(header.entry, None);
    }

    #[test]
    fn stamped_header_reports_its_generation_and_shares_it_when_cloned() {
        let generation = Arc::new(GenerationToken::new(GenerationId::new(7)));
        let header = BatchHeader {
            admitted_at: Some(AdmissionTime::from_unix_secs(100)),
            admission_point: Some(AdmissionPointId::new(2)),
            epoch: 1,
            generation: Some(Arc::clone(&generation)),
            entry: Some(EntryId::new(3)),
        };
        assert!(header.is_stamped());
        assert_eq!(header.generation_id(), Some(GenerationId::new(7)));

        // Cloning shares the generation rather than copying it, so every copy keeps it alive.
        let copy = header.clone();
        assert_eq!(Arc::strong_count(&generation), 3);
        drop(header);
        drop(copy);
        assert_eq!(Arc::strong_count(&generation), 1);
    }
}
