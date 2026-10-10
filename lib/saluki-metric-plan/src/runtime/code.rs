//! A region program translated for execution: every reference resolved to an index, checked once when a generation
//! is prepared rather than on every batch.

use saluki_core::execution::{BatchHeader, EntryId, ExitId};

use super::{telemetry::Layout, RegionError};
use crate::compile::{
    DecisionExit, DecisionFunction, EntryKey, ExitSet, KernelIndex, RegionProgram, RegisterId, Route, Step,
};

/// Where one output of a step leads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Target {
    /// Into a register.
    Register(usize),

    /// Into a `drop` node, by its index in the program's [`Layout`].
    Drop(usize),
}

/// One step, with indices in place of references.
#[derive(Clone, Debug)]
pub(super) enum Op {
    /// A single-port kernel over a register, in place.
    Map { kernel: usize, register: usize },

    /// A multi-port kernel, each port routed.
    Expand {
        kernel: usize,
        input: usize,
        ports: Vec<Target>,
    },

    /// A decision: each event goes to the target of every exit in its exit set.
    Decide {
        decision: usize,
        input: usize,
        /// Where each exit leads, in exit order.
        targets: Vec<Target>,
        /// The exits whose target is a register.
        registers: ExitSet,
        /// The exits whose target is a `drop`.
        drops: ExitSet,
    },

    /// A `merge` of several registers.
    Concat { inputs: Vec<usize>, output: usize },

    /// Buffers handed to an exit, stamped with the batch's header and `entry`.
    Exit {
        register: usize,
        exit: ExitId,
        entry: Option<EntryId>,
    },

    /// Buffers discarded as policy loss of a `drop` node, by its index in the program's [`Layout`].
    Drop { register: usize, drop: usize },
}

/// The ops of one entry.
#[derive(Clone, Debug)]
pub(super) struct EntryCode {
    pub key: EntryKey,
    pub registers: usize,
    pub ops: Vec<Op>,
}

/// One decision of a program, compiled to its function.
#[derive(Clone, Debug)]
pub(super) struct DecisionCode {
    pub function: DecisionFunction,
    /// Whether the region memoizes the function (R4).
    pub memoized: bool,
    /// Each exit that leads to a `drop`, with the drop's index in the program's [`Layout`]: the policy loss derived
    /// from the decision's counts per exit set.
    pub drops: Vec<(DecisionExit, usize)>,
}

/// The ops of every entry of a program, and its decisions' functions.
#[derive(Clone, Debug)]
pub(super) struct ProgramCode {
    pub entries: Vec<EntryCode>,
    pub registers: usize,
    /// Every decision of the program that a step runs, by decision index.
    pub decisions: Vec<Option<DecisionCode>>,
}

impl ProgramCode {
    /// Translates a program for a region with `exit_count` exits.
    ///
    /// # Errors
    ///
    /// Returns an error if the program refers to a kernel, decision, register, or exit it does not have, if a
    /// decision is malformed, or if a decision's targets do not match its exits: a program the region cannot run.
    pub fn translate(program: &RegionProgram, layout: &Layout, exit_count: usize) -> Result<Self, RegionError> {
        let malformed = |entry: &EntryKey, message: String| RegionError::MalformedProgram {
            slot: program.slot(),
            entry: *entry,
            message,
        };

        // Every decision a step runs compiles to its function once, here, rather than for every batch.
        let mut decisions = vec![None; program.decisions().len()];
        let mut entries = Vec::with_capacity(program.entries().len());
        for entry in program.entries() {
            let key = &entry.key;
            let register = |register: RegisterId| {
                let index = register.get();
                if index < entry.registers {
                    Ok(index)
                } else {
                    Err(malformed(
                        key,
                        format!(
                            "a step uses register {}, but the entry has {} registers",
                            index, entry.registers
                        ),
                    ))
                }
            };
            let target = |route: &Route| match route {
                Route::Register(id) => register(*id).map(Target::Register),
                Route::Drop(node) => layout
                    .drop_index(node)
                    .map(Target::Drop)
                    .ok_or_else(|| malformed(key, format!("drop node `{}` has no counts", node))),
            };
            let kernel = |index: KernelIndex, ports: Option<usize>| {
                let kernel = program
                    .kernel(index)
                    .ok_or_else(|| malformed(key, format!("a step uses kernel {}, which the program lacks", index)))?;
                match ports {
                    Some(ports) if ports != kernel.ports.len() => Err(malformed(
                        key,
                        format!(
                            "kernel `{}` has {} ports, but its step routes {}",
                            kernel.key.node,
                            kernel.ports.len(),
                            ports
                        ),
                    )),
                    _ => Ok(index.get()),
                }
            };

            let mut ops = Vec::with_capacity(entry.steps.len());
            for step in &entry.steps {
                let op = match step {
                    Step::Map {
                        kernel: index,
                        register: id,
                    } => Op::Map {
                        kernel: kernel(*index, Some(1))?,
                        register: register(*id)?,
                    },
                    Step::Expand {
                        kernel: index,
                        input,
                        ports,
                    } => Op::Expand {
                        kernel: kernel(*index, Some(ports.len()))?,
                        input: register(*input)?,
                        ports: ports.iter().map(target).collect::<Result<_, _>>()?,
                    },
                    Step::Decide {
                        decision,
                        input,
                        targets,
                    } => {
                        let found = program.decision(*decision).ok_or_else(|| {
                            malformed(
                                key,
                                format!("a step uses decision {}, which the program lacks", decision),
                            )
                        })?;
                        let root = &found.root().node;
                        if targets.len() != found.exits.len() {
                            return Err(malformed(
                                key,
                                format!(
                                    "decision `{}` has {} exits, but its step routes {}",
                                    root,
                                    found.exits.len(),
                                    targets.len()
                                ),
                            ));
                        }
                        let targets = targets.iter().map(target).collect::<Result<Vec<_>, _>>()?;
                        let mut registers = ExitSet::EMPTY;
                        let mut drops = ExitSet::EMPTY;
                        let mut drop_targets = Vec::new();
                        for (exit, target) in targets.iter().enumerate() {
                            let Ok(exit) = u16::try_from(exit) else {
                                return Err(malformed(key, format!("decision `{}` has too many exits", root)));
                            };
                            let exit = DecisionExit::new(exit);
                            match target {
                                Target::Register(_) => registers.insert(exit),
                                Target::Drop(drop) => {
                                    drops.insert(exit);
                                    drop_targets.push((exit, *drop));
                                }
                            }
                        }
                        // A decision's exits lead to the same `drop`s in every entry, so that its loss can be
                        // derived from its counts alone.
                        match &decisions[decision.get()] {
                            None => {
                                let function =
                                    DecisionFunction::new(found).map_err(|error| malformed(key, error.to_string()))?;
                                decisions[decision.get()] = Some(DecisionCode {
                                    function,
                                    memoized: found.memoized,
                                    drops: drop_targets,
                                });
                            }
                            Some(known) if known.drops != drop_targets => {
                                return Err(malformed(
                                    key,
                                    format!(
                                        "decision `{}` leads to other `drop`s here than in an earlier entry",
                                        root
                                    ),
                                ));
                            }
                            Some(_) => {}
                        }
                        Op::Decide {
                            decision: decision.get(),
                            input: register(*input)?,
                            targets,
                            registers,
                            drops,
                        }
                    }
                    Step::Concat { inputs, output } => Op::Concat {
                        inputs: inputs.iter().map(|id| register(*id)).collect::<Result<_, _>>()?,
                        output: register(*output)?,
                    },
                    Step::Exit {
                        register: id,
                        exit,
                        entry: consumer,
                    } => {
                        if exit.get() >= exit_count {
                            return Err(malformed(
                                key,
                                format!(
                                    "a step writes to exit {}, but the region has {} exits",
                                    exit.get(),
                                    exit_count
                                ),
                            ));
                        }
                        Op::Exit {
                            register: register(*id)?,
                            exit: ExitId::new(exit.get() as u16),
                            entry: *consumer,
                        }
                    }
                    Step::Drop { register: id, node } => Op::Drop {
                        register: register(*id)?,
                        drop: layout
                            .drop_index(node)
                            .ok_or_else(|| malformed(key, format!("drop node `{}` has no counts", node)))?,
                    },
                };
                ops.push(op);
            }
            if entry.registers == 0 {
                return Err(malformed(key, "the entry has no register for its batch".to_string()));
            }
            entries.push(EntryCode {
                key: entry.key,
                registers: entry.registers,
                ops,
            });
        }

        let registers = entries.iter().map(|entry| entry.registers).max().unwrap_or(0);
        Ok(Self {
            entries,
            registers,
            decisions,
        })
    }

    /// Returns the position of the entry a batch belongs to: by the entry its header carries, set by the slot that
    /// sent it, or otherwise by the admission point that stamped it.
    pub fn entry_index(&self, header: &BatchHeader) -> Option<usize> {
        let key = match (header.entry, header.admission_point) {
            (Some(entry), _) => EntryKey::Region(entry),
            (None, Some(point)) => EntryKey::Admission(point),
            (None, None) => return None,
        };
        self.entries.iter().position(|entry| entry.key == key)
    }
}
