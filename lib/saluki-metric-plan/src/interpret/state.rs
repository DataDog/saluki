//! Isolated state owners in the interpreter: the instances of their state, which generations bind, and the cut in
//! flight.
//!
//! An isolated state owner's state belongs to a **lineage**, keyed by its node and its state contract
//! ([`LineageKey`]), not to a generation. Each lineage has one **instance**: its kernel, with the progress of the
//! admission points upstream of it, its tick at the gate, and, while a cut replaces it, the generations it retires
//! between. Programs bind instances by their index here: a `retain` transition binds the same instance in the new
//! program, and a `cut` gives the new program a new instance while the old one retires.

use std::collections::{BTreeMap, BTreeSet};

use saluki_core::execution::{AdmissionPointId, AdmissionTime, GenerationRef, Kernel};

use crate::{
    compile::LineageKey,
    runtime::state::rules::{RetiringRefs, WatermarkTracker},
};

/// The generations a lineage retires between, both held for as long as it retires: the one it served, whose program
/// its last output flows through, and the one replacing it, whose cut boundary says when it retires.
pub(super) struct Retiring {
    pub owner: GenerationRef,
    pub by: GenerationRef,
}

/// One instance of an isolated state owner's state.
pub(super) struct Instance {
    /// The lineage the instance holds.
    pub key: LineageKey,

    /// The state owner's kernel.
    pub kernel: Box<dyn Kernel>,

    /// The progress of the admission points upstream of the state owner, from the stamps it saw.
    pub tracker: WatermarkTracker,

    /// The flush tick waiting at the tick gate, if one is: its wall-clock second, and the clock its grace ends at.
    pub gated: Option<(u64, AdmissionTime)>,

    /// The flush time of the instance's previous tick that fired in this epoch.
    pub last_flush: Option<AdmissionTime>,

    /// The generations it retires between, while a cut replaces its lineage.
    pub retiring: Option<Retiring>,

    /// Whether the instance made its final flush at a cut: it holds nothing, and never ticks or flushes again.
    pub retired: bool,
}

impl Instance {
    /// Creates the instance of a new lineage, which has seen nothing.
    pub fn new(key: LineageKey, kernel: Box<dyn Kernel>, upstream: &[AdmissionPointId]) -> Self {
        Self {
            key,
            kernel,
            tracker: WatermarkTracker::new(upstream.iter().copied()),
            gated: None,
            last_flush: None,
            retiring: None,
            retired: false,
        }
    }

    /// Returns the generations the instance retires between, for the shared rules.
    pub fn retiring_refs(&self) -> Option<RetiringRefs<'_>> {
        self.retiring.as_ref().map(|retiring| RetiringRefs {
            owner: &retiring.owner,
            by: &retiring.by,
        })
    }

    /// Returns `true` if the instance observes the stamps of `point`: if the point is upstream of it.
    pub fn observes(&self, point: AdmissionPointId) -> bool {
        !self.retired && self.tracker.upstream().any(|upstream| upstream == point)
    }

    /// Returns the key of the output clock the instance stamps its time-driven output by: the admission points
    /// upstream of it, in ID order, which an isolation slot is defined by.
    ///
    /// The interpreter knows no frame: a frame computed from a plan with two state owners fed by the same points runs
    /// them in two isolation slots, each with an output clock of its own, where the interpreter latches their stamps on
    /// one. A state owner a later plan adds runs in the boot frame's isolation slot, whose clock it shares, as here.
    pub fn output_clock(&self) -> Vec<AdmissionPointId> {
        self.tracker.upstream().collect()
    }
}

/// The instances of every lineage of the run, by index.
///
/// Indices are never reused, so a program's binding names one instance for as long as the program lives.
#[derive(Default)]
pub(super) struct Instances {
    slots: Vec<Option<Instance>>,
}

impl Instances {
    /// Adds an instance, returning its index.
    pub fn insert(&mut self, instance: Instance) -> usize {
        self.slots.push(Some(instance));
        self.slots.len() - 1
    }

    /// Returns the instance at `index`, if it is live.
    pub fn get(&self, index: usize) -> Option<&Instance> {
        self.slots.get(index).and_then(Option::as_ref)
    }

    /// Returns the instance at `index` mutably, if it is live.
    pub fn get_mut(&mut self, index: usize) -> Option<&mut Instance> {
        self.slots.get_mut(index).and_then(Option::as_mut)
    }

    /// Removes the instance at `index`, dropping its kernel, which gives back its state budget.
    pub fn remove(&mut self, index: usize) -> Option<Instance> {
        self.slots.get_mut(index).and_then(Option::take)
    }

    /// Returns the index of every live instance, in creation order.
    pub fn indices(&self) -> Vec<usize> {
        (0..self.slots.len())
            .filter(|&index| self.slots[index].is_some())
            .collect()
    }
}

/// A cut that an installation armed and that has not finalized: batches stamped before `at` belong to `old`, later
/// ones to `new`.
pub(super) struct CutInFlight {
    /// The generation the cut replaces.
    pub old: GenerationRef,

    /// The generation that takes over, whose token carries the cut's boundary.
    pub new: GenerationRef,

    /// The cut's time.
    pub at: AdmissionTime,

    /// The admission points upstream of the state owners whose lineages the cut starts or ends: the cut finalizes once
    /// every one of them stamps only `new`.
    pub affected: Vec<AdmissionPointId>,

    /// The epoch every admission point's clock was in when the cut was published, which a point that had stamped
    /// nothing yet first stamps in: 0.
    pub epochs_at_publish: BTreeMap<AdmissionPointId, u32>,

    /// The admission points that stamped a batch or heartbeat with `new`: once one has, the cut cannot be cancelled.
    pub admitted_to_new: BTreeSet<AdmissionPointId>,
}

impl CutInFlight {
    /// Returns the epoch a point's clock was in when the cut was published.
    pub fn epoch_at_publish(&self, point: AdmissionPointId) -> u32 {
        self.epochs_at_publish.get(&point).copied().unwrap_or(0)
    }
}
