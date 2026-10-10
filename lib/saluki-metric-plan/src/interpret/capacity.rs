//! Output capacity: what `capacity(output, n)` trace events mean to the interpreter.

use std::collections::BTreeMap;

use crate::ir::OutputId;

/// The capacity bounds a trace sets on outputs, and what each output can still accept.
///
/// # Semantics
///
/// `capacity(output, n)` bounds what `output` accepts from that event on: at most `n` more records, until the next
/// `capacity` event for the same output replaces the bound. An output the trace never bounds accepts everything.
/// Bounds belong to outputs, not to plans, so they hold across installations, and a trace may bound an output before
/// any plan that declares it is installed.
///
/// An output accepts the records delivered to it in the interpreter's [delivery order][super#delivery-order], while
/// its bound lasts. A record it does not accept is **capacity loss**: the interpreter counts it, with its points, as
/// `capacity` loss of the `emit` node that delivered it, and keeps it in
/// [`RunOutput::refused`][super::RunOutput::refused], so that
/// [`compare_under_capacity`][crate::equivalence::compare_under_capacity] can compare what two executions lost.
///
/// Capacity counts records, as a delivery policy that sheds whole records does: the specification leaves how an output
/// sheds under overload to its delivery policy, and the bound only has to make loss deterministic for a trace.
#[derive(Debug, Default)]
pub(super) struct OutputCapacity {
    /// How many more records each bounded output accepts.
    remaining: BTreeMap<OutputId, u64>,
}

impl OutputCapacity {
    /// Applies a `capacity(output, limit)` event: `output` accepts at most `limit` more records from now on, whatever
    /// it had left of an earlier bound.
    pub fn bound(&mut self, output: &OutputId, limit: u64) {
        self.remaining.insert(output.clone(), limit);
    }

    /// Returns how many of `records` records delivered to `output`, in order, it accepts now: the first that many are
    /// delivered, and the rest are capacity loss. Counts the accepted records against the output's bound.
    pub fn accept(&mut self, output: &OutputId, records: usize) -> usize {
        match self.remaining.get_mut(output) {
            None => records,
            Some(remaining) => {
                let accepted = usize::try_from(*remaining).map_or(records, |remaining| records.min(remaining));
                *remaining -= accepted as u64;
                accepted
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(value: &str) -> OutputId {
        OutputId::new(value).unwrap()
    }

    #[test]
    fn an_output_accepts_up_to_its_bound_until_the_next_bound_replaces_it() {
        let mut capacity = OutputCapacity::default();
        let (a, b) = (out("a"), out("b"));

        // Unbounded outputs accept everything.
        assert_eq!(capacity.accept(&a, 7), 7);

        // (records delivered, records accepted) in order, after a bound of 5 on `a`.
        capacity.bound(&a, 5);
        for (records, accepted) in [(2, 2), (4, 3), (1, 0), (0, 0)] {
            assert_eq!(capacity.accept(&a, records), accepted, "{} records", records);
        }
        // Another output's bound is its own.
        assert_eq!(capacity.accept(&b, 9), 9);

        // A new bound replaces what was left, whether it is larger or smaller.
        capacity.bound(&a, 3);
        assert_eq!(capacity.accept(&a, 2), 2);
        capacity.bound(&a, 0);
        assert_eq!(capacity.accept(&a, 2), 0);
        capacity.bound(&a, u64::MAX);
        assert_eq!(capacity.accept(&a, 1_000), 1_000);
    }
}
