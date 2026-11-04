//! Lane counts: how many lanes the slots of the metrics path run on, fixed at boot.
//!
//! The DogStatsD source's metrics split into producer lanes, one per decoder up to a maximum; DogStatsD aggregation
//! runs on state lanes; and post-aggregation runs on its own lanes. The counts come from the Saluki-only
//! `experimental.programmable_metrics.lanes.*` keys, with defaults derived from the host's workers and DogStatsD's
//! decoders, and fit the memory grant in `Strict` mode ([`LaneSettings::fit`]). Every count is read at startup and
//! changes at restart.
//!
//! # Automatic counts
//!
//! With `lanes.mode: auto`, the default, the counts are, for a primary runtime of `W` workers and `decoders` DogStatsD
//! decoders (`dogstatsd_workers_count`, or the source's default from the host's vCPUs):
//!
//! - producer lanes `D = min(decoders, PRODUCER_LANES_MAX)`, and one below three workers, so decoder `d` feeds lane
//!   `d mod D` (see [`automatic_producer_lanes`]);
//! - state lanes `P = 1` below four workers, otherwise `clamp(W / 2, 2, AUTOMATIC_STATE_LANES_MAX)`;
//! - post-aggregation lanes `M = min(D, AUTOMATIC_POST_LANES_MAX)`.
//!
//! Where every automatic count is one, as below three workers, and the configuration sets no count, the metrics path
//! runs without lanes, as with `lanes.mode: off`: one lane per slot gains no parallelism, and the layout without lanes
//! costs less per metric there (see [`LaneSettings::from_config`]).
//!
//! The automatic counts stop at what the first measurements support: state lanes at
//! [`AUTOMATIC_STATE_LANES_MAX`] (4) until per-batch budget reservations and a worker-time sweep raise it, and
//! post-aggregation lanes at [`AUTOMATIC_POST_LANES_MAX`] (8). An explicit `lanes.state` or `lanes.post` above
//! [`MAX_LANES`][saluki_metric_plan::limits::values::MAX_LANES] (64) is lowered to it, with a warning. With
//! `lanes.mode: off`, every slot runs one lane and R1's cost model pipelines the slots: the layout before lanes.
//!
//! # The memory grant
//!
//! Lanes are fixed at boot, so the boot verification of memory bounds counts every lane's kernels, memos, and buffers.
//! In `strict` memory mode, `run.rs` measures the firm bound of the topology it built, and of the topology on fewer
//! lanes, and [`LaneSettings::fit`] lowers the automatic counts until the bound fits the grant, if the layout without
//! lanes would: the post-aggregation lanes first, then the state lanes, then the producer lanes. Counts the
//! configuration sets are never lowered; when they keep the bound above the grant, ADP refuses to boot, naming them.
//!
//! What a live change builds is admitted against what the boot bound leaves of the grant, and a replaced generation's
//! instances hold their credits until it retires, so the automatic counts also leave room for one generation's
//! overlap ([`LaneBound::overlap`]) whenever the layout without lanes leaves it: lanes never take the room a live
//! change needs, where a process without lanes would have it.
//!
//! # CPU-time sampling
//!
//! On lanes, resource groups are entered more often, since lanes split batches: [`cpu_time_sampling`] gives the rate
//! of CPU-time sampling `run.rs` sets at startup, [`CPU_TIME_SAMPLING`] on Linux, unless
//! [`CPU_TIME_SAMPLING_ENV`] sets it.

use std::num::{NonZeroU16, NonZeroU32};

use agent_data_plane_config::{control::LanesMode, SalukiConfiguration};
use saluki_components::sources::dogstatsd_decoder_workers;
use saluki_error::{generic_error, GenericError};
use saluki_metric_plan::{
    compile::{FrameOptions, LanePolicy},
    limits,
};
use tracing::{info, warn};

/// The most state lanes the automatic count gives.
///
/// Decision log: the first measurements put two compare-and-swap operations of the shared context budget above 5% of
/// a state lane's time per new context at every count above one, and key skew under power-law traffic at eight lanes
/// costs more than an eighth lane gains, so the automatic count stops at 4 until per-batch budget reservations land
/// and the sweep of worker time per metric decides it.
pub const AUTOMATIC_STATE_LANES_MAX: u16 = 4;

/// The most post-aggregation lanes the automatic count gives.
///
/// Decision log: post-aggregation lanes share no state, and nothing measured up to eight workers argues against
/// eight. Nothing above eight workers has been measured, so eight is provisional until a gate host runs 16 workers.
pub const AUTOMATIC_POST_LANES_MAX: u16 = 8;

/// The fewest primary runtime workers on which the automatic counts give more than one producer lane.
///
/// Decision log: a producer lane runs the whole pre-aggregation chain beside the decoders and the rest of the metrics
/// path. On two workers, one producer lane fed by both decoders ran at 1.03 to 1.09 of legacy's throughput and 0.96 to
/// 1.00 of its CPU per metric, where two ran at 0.93 to 0.98 and 1.09 to 1.10, and broke the scaling the stage gate
/// asks for (macOS, five alternating rounds; the Linux VM likewise, 0.93 and 1.09). From four workers on, a lane per
/// decoder scales as legacy does; three is unmeasured, and keeps a lane per decoder.
pub const PRODUCER_LANES_MIN_WORKERS: usize = 3;

/// The rate of CPU-time sampling ADP sets on Linux when the metrics path runs on lanes: a resource group reads its
/// thread's CPU time on one entry in this many, at random, and counts the time it reads this many times.
///
/// Decision log: entering a resource group reads the thread's CPU time twice, a system call on Linux, on every task
/// poll and every kernel call, which lanes multiply as they split batches; one read in 16 keeps the estimates unbiased
/// at a sixteenth of the cost. Measurements set the rate with [`CPU_TIME_SAMPLING_ENV`] to compare at equal rates.
pub const CPU_TIME_SAMPLING: NonZeroU32 = NonZeroU32::new(16).unwrap();

/// The environment variable that sets the rate of CPU-time sampling, a positive integer, whatever the lanes.
pub const CPU_TIME_SAMPLING_ENV: &str = "SALUKI_CPU_TIME_SAMPLING";

/// The configuration key of the state lanes.
const STATE_KEY: &str = "experimental.programmable_metrics.lanes.state";

/// The configuration key of the post-aggregation lanes.
const POST_KEY: &str = "experimental.programmable_metrics.lanes.post";

/// The lanes of the metrics path, and how its frame is laid out.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LaneCounts {
    /// The lanes of each kind of slot.
    pub lanes: LanePolicy,

    /// Whether R1's cost model pipelines the slots, as it does with lanes off: the stage 6 layout.
    pub pipelining: bool,
}

impl LaneCounts {
    /// Lanes off: one lane per slot, and the slots R1's cost model pipelines.
    pub const OFF: Self = Self {
        lanes: LanePolicy::ONE,
        pipelining: true,
    };

    /// Returns the counts of a metrics path on lanes, `lanes`, laid out without R1's pipelining, so that fusion is
    /// maximal between fences.
    pub const fn on(lanes: LanePolicy) -> Self {
        Self {
            lanes,
            pipelining: false,
        }
    }

    /// Returns the options the frame is laid out with.
    pub fn frame_options(&self) -> FrameOptions {
        FrameOptions {
            pipelining: self.pipelining,
        }
    }

    /// Returns `true` if the metrics path runs on lanes, whatever their number: the DogStatsD source's producer lanes
    /// then stamp their own admission points, and R1's cost model is off.
    pub fn lanes_on(&self) -> bool {
        !self.pipelining
    }

    /// Describes the counts for logs and errors: the lanes of each kind, or that the metrics path runs without lanes.
    pub fn describe(&self) -> String {
        if !self.lanes_on() {
            return "without lanes (one lane per slot, R1's pipelining)".to_string();
        }
        let lanes = &self.lanes;
        format!(
            "{} producer, {} state, {} post-aggregation lanes",
            lanes.dogstatsd, lanes.state, lanes.post
        )
    }
}

/// The lane counts the configuration asks for, and which of them it sets explicitly: what boot starts from, and what
/// the `Strict` fit may lower.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LaneSettings {
    /// The counts the configuration asks for, the automatic ones resolved.
    pub counts: LaneCounts,

    /// Whether `lanes.state` sets the state lanes, rather than the automatic count.
    pub explicit_state: bool,

    /// Whether `lanes.post` sets the post-aggregation lanes, rather than the automatic count.
    pub explicit_post: bool,
}

/// What the `Strict` fit decided about the lane counts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaneFit {
    /// The counts asked for fit the grant, or nothing can be decided: keep them.
    Keep,

    /// The automatic counts do not fit the grant while fewer lanes do: boot with these.
    Lowered(LaneCounts),
}

/// What a process booted on some lane counts holds against the memory grant, as the `Strict` fit reads it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LaneBound {
    /// The firm memory bound of the whole process, which boot verification checks against the grant.
    pub firm: u64,

    /// The room one generation's overlap takes beside it: what a live change that rebuilds the kernels of one slot
    /// adds while the generation it replaces still holds its own, the largest kernel-instance credit of one slot of the
    /// boot generation, on all of the slot's lanes.
    pub overlap: u64,
}

impl LaneBound {
    /// Returns the firm bound with room for one generation's overlap.
    fn with_overlap(self) -> u64 {
        self.firm.saturating_add(self.overlap)
    }

    /// Returns the firm bound alone.
    fn firm(self) -> u64 {
        self.firm
    }
}

/// The bounds the fit measured, by lane counts, so that it measures each candidate once.
struct Measured<F> {
    bound_of: F,
    measured: Vec<(LaneCounts, LaneBound)>,
}

impl<F> Measured<F>
where
    F: AsyncFnMut(LaneCounts) -> Result<LaneBound, GenericError>,
{
    async fn of(&mut self, counts: LaneCounts) -> Result<LaneBound, GenericError> {
        if let Some((_, bound)) = self.measured.iter().find(|(measured, _)| *measured == counts) {
            return Ok(*bound);
        }
        let bound = (self.bound_of)(counts).await?;
        self.measured.push((counts, bound));
        Ok(bound)
    }
}

impl LaneSettings {
    /// Returns the lane counts the configuration `config` asks for on a primary runtime of `workers` workers.
    ///
    /// Logs a warning for an explicit count above [`MAX_LANES`][limits::values::MAX_LANES], which is lowered to it,
    /// and for explicit counts that `lanes.mode: off` ignores.
    pub fn from_config(config: &SalukiConfiguration, workers: usize) -> Self {
        let lanes = &config.control.programmable_metrics.lanes;
        let decoders = dogstatsd_decoder_workers(config.domains.dogstatsd.listeners.workers_count).get();
        match lanes.mode {
            LanesMode::Off => {
                if lanes.state.is_some() || lanes.post.is_some() {
                    warn!(
                        "`{}` and `{}` are ignored, since `experimental.programmable_metrics.lanes.mode` is `off`: \
                         every slot of the metrics path runs on one lane. Set the mode to `auto` to use them.",
                        STATE_KEY, POST_KEY
                    );
                }
                Self {
                    counts: LaneCounts::OFF,
                    explicit_state: false,
                    explicit_post: false,
                }
            }
            LanesMode::Auto => {
                let dogstatsd = automatic_producer_lanes(decoders, workers);
                let state = lanes
                    .state
                    .map_or_else(|| automatic_state_lanes(workers), |count| capped(STATE_KEY, count));
                let post = lanes
                    .post
                    .map_or_else(|| automatic_post_lanes(dogstatsd), |count| capped(POST_KEY, count));
                let settings = Self {
                    counts: LaneCounts::on(LanePolicy { dogstatsd, state, post }),
                    explicit_state: lanes.state.is_some(),
                    explicit_post: lanes.post.is_some(),
                };
                Self {
                    counts: settings.settled(settings.counts),
                    ..settings
                }
            }
        }
    }

    /// Returns `counts` as the metrics path runs them: without lanes when every count is one and the configuration
    /// sets none of them, as below three workers.
    ///
    /// Decision log: on two workers the default profile ran 3 to 10% slower and took 4 to 9% more CPU per metric on
    /// one lane per slot than without lanes, the cost of the lane runtime itself (the lane stamping its admission
    /// point, the lane statistics, the lane telemetry), since one lane per slot runs nothing in parallel; counts the
    /// configuration sets keep the lanes asked for.
    fn settled(&self, counts: LaneCounts) -> LaneCounts {
        let automatic = !self.explicit_state && !self.explicit_post;
        if automatic && counts.lanes_on() && counts.lanes.is_one() {
            LaneCounts::OFF
        } else {
            counts
        }
    }

    /// Fits the lane counts to a `Strict` memory grant of `grant` bytes, given `bound_of`, which returns what the whole
    /// process booted with the counts it is given holds against the grant.
    ///
    /// When the counts asked for fit the grant with room for one generation's overlap ([`LaneBound::overlap`]), or the
    /// metrics path runs without lanes, nothing changes. Otherwise, while the layout without lanes
    /// ([`LaneCounts::OFF`]) fits, the automatic counts are lowered: the post-aggregation lanes first, then the state
    /// lanes, then the producer lanes, each to the most lanes that fit with the others, down to one, and the first
    /// counts that fit are returned. They fit with room for the overlap whenever the layout without lanes leaves that
    /// room, and otherwise by their firm bound alone, as they do when counts the configuration sets keep the room from
    /// them. A count the configuration sets is never lowered. Automatic counts lowered to one lane each are the layout
    /// without lanes, as at boot ([`from_config`][Self::from_config]). When the layout without lanes does not fit
    /// either, the counts are lowered to the smallest layout the fit can reach, every automatic count at one (the
    /// layout without lanes when the configuration sets no count), so that boot verification refuses the grant
    /// against the bound of the smallest layout that would boot, and a warning names both bounds.
    ///
    /// # Errors
    ///
    /// Returns an error naming the keys that set counts when the counts do not fit even with every automatic count at
    /// one, while the layout without lanes fits, or if `bound_of` fails.
    pub async fn fit<F>(&self, grant: u64, bound_of: F) -> Result<LaneFit, GenericError>
    where
        F: AsyncFnMut(LaneCounts) -> Result<LaneBound, GenericError>,
    {
        let asked = self.counts;
        if !asked.lanes_on() {
            return Ok(LaneFit::Keep);
        }
        let mut bounds = Measured {
            bound_of,
            measured: Vec::new(),
        };
        let asked_bound = bounds.of(asked).await?;
        if asked_bound.with_overlap() <= grant {
            return Ok(LaneFit::Keep);
        }
        let without_lanes = bounds.of(LaneCounts::OFF).await?;
        if without_lanes.firm > grant {
            // Nothing fits: verification refuses the grant, against the bound of the smallest layout the fit can reach,
            // so that the limit it names is one that boots.
            let smallest = self.smallest();
            warn!(
                asked = %asked.describe(),
                asked_bytes = asked_bound.firm,
                without_lanes_bytes = without_lanes.firm,
                grant,
                "The metrics path does not fit the memory grant in `strict` memory mode, even without lanes: the \
                 smallest layout the lanes can take ({}) is verified against the grant, and refused. Raise \
                 `memory_limit`.",
                smallest.describe()
            );
            return Ok(if smallest == asked {
                LaneFit::Keep
            } else {
                LaneFit::Lowered(smallest)
            });
        }

        // Lanes keep the room the layout without lanes leaves for a live change, when it leaves it.
        if without_lanes.with_overlap() <= grant {
            if let Some(lowered) = self.lower(&mut bounds, grant, LaneBound::with_overlap).await? {
                let lowered = self.settled(lowered);
                info!(
                    asked = %asked.describe(),
                    chosen = %lowered.describe(),
                    grant,
                    "The automatic lane counts of the metrics path do not fit the memory grant in `strict` memory \
                     mode with room for a live change: booting with fewer lanes."
                );
                return Ok(LaneFit::Lowered(lowered));
            }
            warn!(
                asked = %asked.describe(),
                grant,
                "The lane counts of the metrics path leave no room in the memory grant for a live change that \
                 rebuilds a slot's kernels, even with every automatic count at one lane, while the metrics path \
                 without lanes would: in `strict` memory mode, such a change can be refused until restart."
            );
        }
        if asked_bound.firm <= grant {
            return Ok(LaneFit::Keep);
        }
        if let Some(lowered) = self.lower(&mut bounds, grant, LaneBound::firm).await? {
            let lowered = self.settled(lowered);
            info!(
                asked = %asked.describe(),
                chosen = %lowered.describe(),
                grant,
                "The automatic lane counts of the metrics path do not fit the memory grant in `strict` memory mode: \
                 booting with fewer lanes."
            );
            return Ok(LaneFit::Lowered(lowered));
        }

        let explicit = [(self.explicit_state, STATE_KEY), (self.explicit_post, POST_KEY)]
            .into_iter()
            .filter_map(|(explicit, key)| explicit.then_some(key))
            .collect::<Vec<_>>();
        if explicit.is_empty() {
            // Every count is automatic, and one lane each, the layout without lanes, fits: it boots.
            return Ok(LaneFit::Lowered(self.smallest()));
        }
        Err(generic_error!(
            "The lane counts of the metrics path ({}) do not fit the memory grant of {} bytes in `strict` memory mode, \
             even with every automatic count at one lane, while the metrics path without lanes would fit. Lower {}, or \
             set it to 0 for the automatic count, or raise `memory_limit`.",
            asked.describe(),
            grant,
            explicit.iter().map(|key| format!("`{}`", key)).collect::<Vec<_>>().join(" and ")
        ))
    }

    /// Returns the most lanes of the automatic kinds, lowered in turn from the counts asked for, whose bound as `cost`
    /// reads it fits `grant`, or `None` if even every automatic count at one does not.
    async fn lower<F>(
        &self, bounds: &mut Measured<F>, grant: u64, cost: fn(LaneBound) -> u64,
    ) -> Result<Option<LaneCounts>, GenericError>
    where
        F: AsyncFnMut(LaneCounts) -> Result<LaneBound, GenericError>,
    {
        let mut candidate = self.counts;
        for kind in self.automatic_kinds() {
            let most = kind.count(&candidate.lanes);
            if most > 1 && cost(bounds.of(kind.with(candidate, 1)).await?) <= grant {
                // Bounds grow with the lanes of each kind, so the most lanes that fit are found by bisection: `low`
                // fits and `high` does not.
                let (mut low, mut high) = (1, most);
                while high - low > 1 {
                    let middle = low + (high - low) / 2;
                    if cost(bounds.of(kind.with(candidate, middle)).await?) <= grant {
                        low = middle;
                    } else {
                        high = middle;
                    }
                }
                return Ok(Some(kind.with(candidate, low)));
            }
            candidate = kind.with(candidate, 1);
        }
        Ok(None)
    }

    /// Returns the kinds of lanes the fit lowers, in the order it lowers them: the ones the configuration does not set.
    fn automatic_kinds(&self) -> impl Iterator<Item = LaneKind> {
        [
            (!self.explicit_post).then_some(LaneKind::Post),
            (!self.explicit_state).then_some(LaneKind::State),
            Some(LaneKind::Producer),
        ]
        .into_iter()
        .flatten()
    }

    /// Returns the smallest layout the fit can reach: the counts asked for with every automatic count at one lane,
    /// which is the layout without lanes when the configuration sets no count.
    fn smallest(&self) -> LaneCounts {
        let at_one = self
            .automatic_kinds()
            .fold(self.counts, |counts, kind| kind.with(counts, 1));
        self.settled(at_one)
    }
}

/// One kind of lane the `Strict` fit lowers.
#[derive(Clone, Copy, Debug)]
enum LaneKind {
    Producer,
    State,
    Post,
}

impl LaneKind {
    fn count(self, lanes: &LanePolicy) -> u16 {
        match self {
            Self::Producer => lanes.dogstatsd,
            Self::State => lanes.state,
            Self::Post => lanes.post,
        }
    }

    fn with(self, mut counts: LaneCounts, count: u16) -> LaneCounts {
        match self {
            Self::Producer => counts.lanes.dogstatsd = count,
            Self::State => counts.lanes.state = count,
            Self::Post => counts.lanes.post = count,
        }
        counts
    }
}

/// Returns an explicit count of `key`, lowered to [`MAX_LANES`][limits::values::MAX_LANES] with a warning if above it.
fn capped(key: &str, count: NonZeroU16) -> u16 {
    let max = max_lanes();
    if count.get() > max {
        warn!(
            "`{}` is {}, above the {} lanes a slot runs on at most: the slot runs on {} lanes.",
            key, count, max, max
        );
        return max;
    }
    count.get()
}

/// Returns the most lanes one slot runs on.
fn max_lanes() -> u16 {
    u16::try_from(limits::values::MAX_LANES).unwrap_or(u16::MAX)
}

/// Returns the automatic producer lanes of `decoders` DogStatsD decoders on a primary runtime of `workers` workers: one
/// per decoder, up to [`PRODUCER_LANES_MAX`][limits::values::PRODUCER_LANES_MAX], and one below
/// [`PRODUCER_LANES_MIN_WORKERS`] workers.
pub fn automatic_producer_lanes(decoders: usize, workers: usize) -> u16 {
    if workers < PRODUCER_LANES_MIN_WORKERS {
        return 1;
    }
    let most = usize::try_from(limits::values::PRODUCER_LANES_MAX).unwrap_or(usize::MAX);
    // At most `PRODUCER_LANES_MAX`, which fits.
    u16::try_from(decoders.clamp(1, most)).unwrap_or(1)
}

/// Returns the automatic state lanes of a primary runtime of `workers` workers: one below four workers, otherwise half
/// of them, between 2 and [`AUTOMATIC_STATE_LANES_MAX`].
pub fn automatic_state_lanes(workers: usize) -> u16 {
    if workers < 4 {
        return 1;
    }
    let half = (workers / 2).clamp(2, usize::from(AUTOMATIC_STATE_LANES_MAX));
    u16::try_from(half).unwrap_or(AUTOMATIC_STATE_LANES_MAX)
}

/// Returns the automatic post-aggregation lanes of `producer` producer lanes: as many, up to
/// [`AUTOMATIC_POST_LANES_MAX`].
pub fn automatic_post_lanes(producer: u16) -> u16 {
    producer.clamp(1, AUTOMATIC_POST_LANES_MAX)
}

/// Logs, once at startup, the lanes the metrics path of `config` booted on, `counts`, with what decided them: the
/// mode, the primary runtime's `workers`, and the DogStatsD decoders; and the rate of CPU-time sampling, `sampling`.
///
/// The counts depend on the host's cores, the decoders, the `Strict` fit, and `SALUKI_WORKER_THREADS`, so a log alone
/// says which layout ran.
pub fn log_booted_lanes(config: &SalukiConfiguration, counts: &LaneCounts, workers: usize, sampling: NonZeroU32) {
    let decoders = dogstatsd_decoder_workers(config.domains.dogstatsd.listeners.workers_count).get();
    let mode = match config.control.programmable_metrics.lanes.mode {
        LanesMode::Auto => "auto",
        LanesMode::Off => "off",
    };
    info!(
        lanes = %counts.describe(),
        mode,
        pipelining = counts.pipelining,
        workers,
        decoders,
        cpu_time_sampling = sampling.get(),
        "The metrics path runs {}.",
        counts.describe()
    );
}

/// Returns the rate of CPU-time sampling to set at startup: the value of [`CPU_TIME_SAMPLING_ENV`], `configured`, when
/// it is a positive integer; otherwise [`CPU_TIME_SAMPLING`] on Linux when the metrics path runs on lanes, and one,
/// which reads the CPU time on every entry, everywhere else.
///
/// Logs a warning for a configured value that is not a positive integer, which is ignored.
pub fn cpu_time_sampling(configured: Option<&str>, lanes_on: bool) -> NonZeroU32 {
    if let Some(configured) = configured {
        match configured.trim().parse::<NonZeroU32>() {
            Ok(rate) => return rate,
            Err(error) => warn!(
                value = configured,
                %error,
                "`{}` is not a positive integer, and is ignored.",
                CPU_TIME_SAMPLING_ENV
            ),
        }
    }
    if lanes_on && cfg!(target_os = "linux") {
        CPU_TIME_SAMPLING
    } else {
        NonZeroU32::MIN
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use agent_data_plane_config::control::ProgrammableMetricsLanes;

    use super::*;

    fn config(mode: LanesMode, state: u16, post: u16, decoders: usize) -> SalukiConfiguration {
        let mut config = SalukiConfiguration::default();
        config.control.programmable_metrics.lanes = ProgrammableMetricsLanes {
            mode,
            state: NonZeroU16::new(state),
            post: NonZeroU16::new(post),
        };
        config.domains.dogstatsd.listeners.workers_count = decoders;
        config
    }

    fn on(dogstatsd: u16, state: u16, post: u16) -> LaneCounts {
        LaneCounts::on(LanePolicy { dogstatsd, state, post })
    }

    #[test]
    fn lanes_are_automatic_by_default_and_off_with_mode_off_whatever_the_counts_say() {
        // By default the counts are automatic: at eight workers, four state lanes, and the producer and
        // post-aggregation lanes of the host's default decoder count.
        let defaults = SalukiConfiguration::default();
        let settings = LaneSettings::from_config(&defaults, 8);
        let decoders = dogstatsd_decoder_workers(defaults.domains.dogstatsd.listeners.workers_count).get();
        let producer = automatic_producer_lanes(decoders, 8);
        assert_eq!(settings.counts, on(producer, 4, automatic_post_lanes(producer)));
        assert!(settings.counts.lanes_on());
        assert_eq!(settings.counts.frame_options(), FrameOptions { pipelining: false });
        assert!(!settings.explicit_state && !settings.explicit_post);

        let counts = LaneSettings::from_config(&config(LanesMode::Off, 0, 0, 6), 8).counts;
        assert_eq!(counts, LaneCounts::OFF);
        assert!(counts.lanes.is_one());
        assert!(!counts.lanes_on());
        assert_eq!(counts.frame_options(), FrameOptions::default());

        let settings = LaneSettings::from_config(&config(LanesMode::Off, 4, 3, 6), 8);
        assert_eq!(settings.counts, LaneCounts::OFF);
        assert!(!settings.explicit_state && !settings.explicit_post);
    }

    #[test]
    fn automatic_counts_follow_the_decoders_and_the_workers_with_the_measured_caps() {
        // Written from the stage 9 design's configuration table, with the caps of the measurements: producer lanes one
        // per decoder up to 16 and one below three workers, state lanes 1 below four workers and otherwise half the
        // workers between 2 and 4, post-aggregation lanes as many as producer lanes up to 8; and the layout without
        // lanes where every count comes to one, as below three workers.
        for (workers, decoders, expected) in [
            (1, 2, LaneCounts::OFF),
            (2, 2, LaneCounts::OFF),
            (2, 6, LaneCounts::OFF),
            (3, 1, LaneCounts::OFF),
            (3, 2, on(2, 1, 2)),
            (3, 4, on(4, 1, 4)),
            (4, 2, on(2, 2, 2)),
            (6, 4, on(4, 3, 4)),
            (8, 6, on(6, 4, 6)),
            (16, 14, on(14, 4, 8)),
            (32, 30, on(16, 4, 8)),
            (64, 62, on(16, 4, 8)),
            (8, 1, on(1, 4, 1)),
        ] {
            let settings = LaneSettings::from_config(&config(LanesMode::Auto, 0, 0, decoders), workers);
            assert_eq!(settings.counts, expected, "W = {workers}, {decoders} decoders");
            assert!(!settings.explicit_state && !settings.explicit_post);
        }
    }

    #[test]
    fn one_lane_per_slot_runs_without_lanes_unless_the_configuration_sets_a_count() {
        // Two workers give one lane of every kind: the layout without lanes, stamped by the source's dispatcher.
        let settings = LaneSettings::from_config(&config(LanesMode::Auto, 0, 0, 2), 2);
        assert_eq!(settings.counts, LaneCounts::OFF);
        assert!(!settings.counts.lanes_on());
        assert_eq!(settings.counts.frame_options(), FrameOptions::default());
        // A count the configuration sets, even of one, keeps the lanes asked for.
        for (state, post) in [(1, 0), (0, 1), (1, 1)] {
            let settings = LaneSettings::from_config(&config(LanesMode::Auto, state, post, 2), 2);
            assert_eq!(settings.counts, on(1, 1, 1), "state {state}, post {post}");
            assert!(settings.counts.lanes_on());
        }
        // Lanes of more than one, of any kind, stay on lanes.
        assert_eq!(
            LaneSettings::from_config(&config(LanesMode::Auto, 0, 0, 1), 8).counts,
            on(1, 4, 1)
        );
    }

    #[test]
    fn explicit_counts_are_kept_and_capped_at_the_most_lanes_a_slot_runs_on() {
        let settings = LaneSettings::from_config(&config(LanesMode::Auto, 3, 5, 4), 8);
        assert_eq!(settings.counts, on(4, 3, 5));
        assert!(settings.explicit_state && settings.explicit_post);

        let settings = LaneSettings::from_config(&config(LanesMode::Auto, 1_000, 65, 4), 8);
        assert_eq!(settings.counts, on(4, 64, 64));

        // An explicit state count says nothing about the post-aggregation lanes, which stay automatic.
        let settings = LaneSettings::from_config(&config(LanesMode::Auto, 2, 0, 12), 8);
        assert_eq!(settings.counts, on(12, 2, 8));
        assert!(settings.explicit_state && !settings.explicit_post);
    }

    #[test]
    fn cpu_time_sampling_follows_the_environment_and_otherwise_the_lanes_on_linux() {
        assert_eq!(cpu_time_sampling(Some("4"), false).get(), 4);
        assert_eq!(cpu_time_sampling(Some(" 64 "), true).get(), 64);
        let on_lanes = if cfg!(target_os = "linux") { 16 } else { 1 };
        assert_eq!(cpu_time_sampling(None, true).get(), on_lanes);
        assert_eq!(cpu_time_sampling(None, false).get(), 1);
        // A value that is not a positive integer is ignored.
        for ignored in ["0", "-3", "often", ""] {
            assert_eq!(cpu_time_sampling(Some(ignored), true).get(), on_lanes, "{ignored}");
        }
    }

    #[test]
    fn the_default_decoder_count_sets_the_producer_lanes() {
        // One producer lane per decoder of the host's default, up to 16, on eight workers; one on two.
        let decoders = dogstatsd_decoder_workers(0).get();
        let settings = LaneSettings::from_config(&config(LanesMode::Auto, 0, 0, 0), 8);
        assert_eq!(usize::from(settings.counts.lanes.dogstatsd), decoders.min(16));
        let settings = LaneSettings::from_config(&config(LanesMode::Auto, 0, 0, 0), 2);
        assert_eq!(settings.counts.lanes.dogstatsd, 1);
    }

    /// A firm bound that grows by fixed bytes per lane of each kind, over a base: what the fit sees of a real process,
    /// in which every lane of a kind adds the same kernels, memos, and buffers.
    fn linear_bound(base: u64, producer: u64, state: u64, post: u64) -> impl Fn(LaneCounts) -> u64 {
        move |counts| {
            if counts.pipelining {
                return base;
            }
            let lanes = counts.lanes;
            base + producer * u64::from(lanes.dogstatsd - 1)
                + state * u64::from(lanes.state - 1)
                + post * u64::from(lanes.post - 1)
        }
    }

    /// Fits `settings` to `grant` over firm bounds `bound` gives, with no room taken by a generation's overlap.
    fn fit(settings: LaneSettings, grant: u64, bound: impl Fn(LaneCounts) -> u64) -> Result<LaneFit, GenericError> {
        fit_with_overlap(settings, grant, bound, |_| 0)
    }

    /// Fits `settings` to `grant` over firm bounds `bound` gives and generation overlaps `overlap` gives.
    fn fit_with_overlap(
        settings: LaneSettings, grant: u64, bound: impl Fn(LaneCounts) -> u64, overlap: impl Fn(LaneCounts) -> u64,
    ) -> Result<LaneFit, GenericError> {
        futures::executor::block_on(settings.fit(grant, async |counts| {
            Ok(LaneBound {
                firm: bound(counts),
                overlap: overlap(counts),
            })
        }))
    }

    /// The overlap of a live change that rebuilds the producer lanes' slot: `per_lane` bytes for each of its lanes,
    /// the one slot of the layout without lanes that holds them included.
    fn producer_overlap(per_lane: u64) -> impl Fn(LaneCounts) -> u64 {
        move |counts| per_lane * u64::from(counts.lanes.dogstatsd)
    }

    #[test]
    fn the_fit_keeps_room_for_one_generations_overlap_while_the_layout_without_lanes_has_it() {
        // 1,000 bytes and 10 per producer lane, and a live change that rebuilds 30 bytes on each producer lane.
        let bound = linear_bound(1_000, 10, 100, 1);
        let overlap = producer_overlap(30);
        let settings = automatic(on(6, 1, 1));
        // The firm bound of the counts asked for, 1,050, fits a grant of 1,100, but not with the 180 bytes of a live
        // change beside it, which the layout without lanes, 1,030 with its change, has room for: the producer lanes
        // are lowered to the most that keep the room, 2 (1,010 and 60).
        assert_eq!(fit(settings, 1_100, &bound).unwrap(), LaneFit::Keep);
        assert_eq!(
            fit_with_overlap(settings, 1_100, &bound, &overlap).unwrap(),
            LaneFit::Lowered(on(2, 1, 1))
        );
        // With room for the change at the counts asked for, nothing changes; with room for it only on one producer
        // lane, as without lanes, it is the layout without lanes.
        assert_eq!(
            fit_with_overlap(settings, 1_230, &bound, &overlap).unwrap(),
            LaneFit::Keep
        );
        assert_eq!(
            fit_with_overlap(settings, 1_050, &bound, &overlap).unwrap(),
            LaneFit::Lowered(LaneCounts::OFF)
        );
        // Where the layout without lanes has no room for a change either, the counts fit by their firm bound alone, as
        // without the overlap.
        assert_eq!(
            fit_with_overlap(settings, 1_020, &bound, &overlap).unwrap(),
            LaneFit::Lowered(on(3, 1, 1))
        );
        assert_eq!(
            fit_with_overlap(automatic(on(2, 1, 1)), 1_020, &bound, &overlap).unwrap(),
            LaneFit::Keep
        );
    }

    #[test]
    fn counts_the_configuration_sets_that_take_the_room_for_a_change_are_kept_while_their_firm_bound_fits() {
        // Explicit state lanes of 100 bytes each: no automatic count makes room for a change beside them, so the fit
        // falls back to the firm bound, which the counts asked for fit.
        let bound = linear_bound(1_000, 10, 100, 1);
        let overlap = producer_overlap(30);
        let settings = LaneSettings {
            counts: on(2, 4, 1),
            explicit_state: true,
            explicit_post: false,
        };
        assert_eq!(
            fit_with_overlap(settings, 1_320, &bound, &overlap).unwrap(),
            LaneFit::Keep
        );
        // A grant their firm bound does not fit either lowers the automatic counts, by the firm bound alone.
        assert_eq!(
            fit_with_overlap(settings, 1_300, &bound, &overlap).unwrap(),
            LaneFit::Lowered(on(1, 4, 1))
        );
    }

    fn automatic(counts: LaneCounts) -> LaneSettings {
        LaneSettings {
            counts,
            explicit_state: false,
            explicit_post: false,
        }
    }

    #[test]
    fn counts_that_fit_the_grant_are_kept() {
        let bound = linear_bound(1_000, 10, 100, 1);
        let settings = automatic(on(6, 4, 6));
        // 1,000 + 50 + 300 + 5.
        assert_eq!(fit(settings, 1_355, &bound).unwrap(), LaneFit::Keep);
        // Without lanes there is nothing to fit, whatever the grant.
        assert_eq!(fit(automatic(LaneCounts::OFF), 0, &bound).unwrap(), LaneFit::Keep);
    }

    #[test]
    fn automatic_counts_that_do_not_fit_are_lowered_post_aggregation_first_then_state_then_producer() {
        let bound = linear_bound(1_000, 10, 100, 1);
        let settings = automatic(on(6, 4, 6));
        // One byte short: the post-aggregation lanes give one up.
        assert_eq!(fit(settings, 1_354, &bound).unwrap(), LaneFit::Lowered(on(6, 4, 5)));
        // Post-aggregation lanes alone cannot make room for 100 bytes: they go to one, and state lanes give up one.
        assert_eq!(fit(settings, 1_250, &bound).unwrap(), LaneFit::Lowered(on(6, 3, 1)));
        // Room for producer lanes only: state and post-aggregation lanes at one, and as many producer lanes as fit.
        assert_eq!(fit(settings, 1_030, &bound).unwrap(), LaneFit::Lowered(on(4, 1, 1)));
        // Exactly the base: one lane each, which is the layout without lanes.
        assert_eq!(fit(settings, 1_000, &bound).unwrap(), LaneFit::Lowered(LaneCounts::OFF));
    }

    #[test]
    fn automatic_counts_that_do_not_fit_even_at_one_lane_each_boot_without_lanes_when_that_fits() {
        // The lane runtime adds 5 bytes even at one lane per slot: a grant of 1,002 fits the layout without lanes only,
        // which the automatic counts come to, rather than one lane each on lanes, which verification would refuse.
        let on_lanes = linear_bound(1_005, 10, 100, 1);
        let bound = |counts: LaneCounts| if counts.pipelining { 1_000 } else { on_lanes(counts) };
        assert_eq!(
            fit(automatic(on(6, 4, 6)), 1_002, bound).unwrap(),
            LaneFit::Lowered(LaneCounts::OFF)
        );
    }

    #[test]
    fn when_the_layout_without_lanes_does_not_fit_either_the_smallest_layout_is_the_one_verified() {
        // Verification refuses the grant either way, against the bound of the smallest layout the fit can reach, so
        // that the limit it names is one that boots: every automatic count at one, the layout without lanes when the
        // configuration sets no count.
        let bound = linear_bound(1_000, 10, 100, 1);
        assert_eq!(
            fit(automatic(on(6, 4, 6)), 999, &bound).unwrap(),
            LaneFit::Lowered(LaneCounts::OFF)
        );
        let explicit = LaneSettings {
            counts: on(6, 4, 6),
            explicit_state: true,
            explicit_post: false,
        };
        assert_eq!(fit(explicit, 999, &bound).unwrap(), LaneFit::Lowered(on(1, 4, 1)));
        // Counts already at the smallest are kept.
        let smallest = LaneSettings {
            counts: on(1, 4, 6),
            explicit_state: true,
            explicit_post: true,
        };
        assert_eq!(fit(smallest, 999, &bound).unwrap(), LaneFit::Keep);
    }

    #[test]
    fn explicit_counts_are_never_lowered_and_refuse_to_boot_when_nothing_else_makes_room() {
        let bound = linear_bound(1_000, 10, 100, 1);
        let settings = LaneSettings {
            counts: on(6, 4, 6),
            explicit_state: true,
            explicit_post: false,
        };
        // The automatic post-aggregation and producer lanes make room for 50 bytes; the explicit state lanes stay.
        assert_eq!(fit(settings, 1_310, &bound).unwrap(), LaneFit::Lowered(on(2, 4, 1)));
        let error = fit(settings, 1_200, &bound).unwrap_err().to_string();
        assert!(
            error.contains("`experimental.programmable_metrics.lanes.state`"),
            "{error}"
        );
        assert!(!error.contains("lanes.post"), "{error}");

        let both = LaneSettings {
            explicit_post: true,
            ..settings
        };
        let error = fit(both, 1_100, &bound).unwrap_err().to_string();
        assert!(error.contains(&format!("`{STATE_KEY}` and `{POST_KEY}`")), "{error}");
    }

    #[test]
    fn the_fit_asks_for_few_bounds() {
        let bound = linear_bound(1_000, 10, 100, 1);
        let asked = RefCell::new(Vec::new());
        let settings = automatic(on(16, 4, 8));
        let fitted = futures::executor::block_on(settings.fit(1_100, async |counts| {
            asked.borrow_mut().push(counts);
            Ok(LaneBound {
                firm: bound(counts),
                overlap: 0,
            })
        }))
        .unwrap();
        assert_eq!(fitted, LaneFit::Lowered(on(11, 1, 1)));
        // The counts asked for, the layout without lanes, one probe per kind at one lane, and a bisection of the
        // producer lanes: never one bound per lane count.
        assert!(asked.borrow().len() <= 10, "{:?}", asked.borrow());

        // When the counts cannot keep room for a change, the fit lowers them again by their firm bound alone, and
        // measures no counts twice.
        let asked = RefCell::new(Vec::new());
        let fitted = futures::executor::block_on(settings.fit(1_100, async |counts| {
            asked.borrow_mut().push(counts);
            Ok(LaneBound {
                firm: bound(counts),
                overlap: if counts.pipelining { 0 } else { 1_000 },
            })
        }))
        .unwrap();
        assert_eq!(fitted, LaneFit::Lowered(on(11, 1, 1)));
        let asked = asked.borrow();
        for (index, counts) in asked.iter().enumerate() {
            assert!(
                !asked[..index].contains(counts),
                "{:?} measured twice: {:?}",
                counts,
                asked
            );
        }
    }
}
