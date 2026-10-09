//! Ports: what an operator accepts on each input and produces on each output.

use crate::{
    ir::PortName,
    model::{Kind, KindSet, PortType, TimestampForm},
};

/// The name of the input port of every operator that has one input.
pub const IN: &str = "in";

/// The name of the output port of every operator that has one output, other than sources.
pub const OUT: &str = "out";

/// An input port: its name, and the records it accepts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InputPort {
    /// The port's name.
    pub name: PortName,

    /// The records the port accepts. Validation rejects an edge into the port that can carry anything else.
    pub accepts: PortType,
}

/// An output port: its name, and how its records relate to the operator's input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutputPort {
    /// The port's name.
    pub name: PortName,

    /// How the kinds and timestamps on the port follow from the operator's input.
    pub mapping: PortMapping,
}

/// How the kinds of an operator's input map to the kinds on one output port.
///
/// Each input kind maps to the set of kinds it can become. An input kind mapped to the empty set never reaches the
/// port.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KindMap([KindSet; 6]);

impl KindMap {
    /// The mapping that keeps every kind.
    pub const IDENTITY: Self = Self([
        KindSet::of(&[Kind::Counter]),
        KindSet::of(&[Kind::Rate]),
        KindSet::of(&[Kind::Gauge]),
        KindSet::of(&[Kind::Set]),
        KindSet::of(&[Kind::Histogram]),
        KindSet::of(&[Kind::Distribution]),
    ]);

    /// Returns this mapping with input kind `from` mapped to the kinds `to`.
    pub const fn with(mut self, from: Kind, to: KindSet) -> Self {
        self.0[from as usize] = to;
        self
    }

    /// Returns the kinds input kind `kind` can become.
    pub const fn get(&self, kind: Kind) -> KindSet {
        self.0[kind as usize]
    }

    /// Returns the kinds that inputs of the kinds `kinds` can become.
    pub fn apply(&self, kinds: KindSet) -> KindSet {
        kinds
            .iter()
            .fold(KindSet::EMPTY, |mapped, kind| mapped.union(self.get(kind)))
    }
}

/// How the timestamp form of an operator's input maps to the timestamp form on one output port.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimestampMap {
    /// The port carries points in the form they arrived in.
    Preserve,

    /// The port carries only the input's points of the given form, `timestamped` or `untimestamped`. When the input
    /// has no points of that form, the port carries nothing.
    Select(TimestampForm),

    /// Every point on the port has the given form, whatever the input's form.
    Set(TimestampForm),
}

/// How the records on one output port follow from an operator's input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PortMapping {
    /// How kinds map.
    pub kinds: KindMap,

    /// How timestamp forms map.
    pub timestamps: TimestampMap,
}

impl PortMapping {
    /// The mapping of a port that carries its input unchanged.
    pub const IDENTITY: Self = Self {
        kinds: KindMap::IDENTITY,
        timestamps: TimestampMap::Preserve,
    };

    /// Returns the type of the port, given the type of the operator's input.
    pub fn apply(&self, input: PortType) -> PortType {
        let (kinds, timestamps) = match self.timestamps {
            TimestampMap::Preserve => (input.kinds, input.timestamps),
            TimestampMap::Set(form) => (input.kinds, form),
            TimestampMap::Select(form) => {
                if input.timestamps == form || input.timestamps == TimestampForm::Mixed {
                    (input.kinds, form)
                } else {
                    (KindSet::EMPTY, form)
                }
            }
        };
        PortType {
            kinds: self.kinds.apply(kinds),
            timestamps,
        }
    }
}

/// The ports of one configured operator.
///
/// An operator's ports follow from its parameters: a `tee` has one output port per copy, and a `merge` one input port
/// per incoming edge. Every port of every node has exactly one edge.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Ports {
    /// The input ports, in order.
    pub inputs: Vec<InputPort>,

    /// The output ports, in order.
    pub outputs: Vec<OutputPort>,
}

impl Ports {
    /// Returns the ports of an operator with one input port named [`IN`], accepting `accepts`, and one output port
    /// named [`OUT`] with the given mapping.
    pub fn single(accepts: PortType, mapping: PortMapping) -> Self {
        Self {
            inputs: vec![InputPort {
                name: PortName::from_static(IN),
                accepts,
            }],
            outputs: vec![OutputPort {
                name: PortName::from_static(OUT),
                mapping,
            }],
        }
    }

    /// Returns the ports of an operator with one input port named [`IN`] that accepts anything, and no output port.
    pub fn sink() -> Self {
        Self {
            inputs: vec![InputPort {
                name: PortName::from_static(IN),
                accepts: PortType::ANY,
            }],
            outputs: Vec::new(),
        }
    }

    /// Returns the position of the input port named `name`.
    pub fn input(&self, name: &PortName) -> Option<usize> {
        self.inputs.iter().position(|port| &port.name == name)
    }

    /// Returns the position of the output port named `name`.
    pub fn output(&self, name: &PortName) -> Option<usize> {
        self.outputs.iter().position(|port| &port.name == name)
    }

    /// Returns the type of each output port, given the type of each input port, by applying each output port's mapping
    /// to the union of the inputs.
    pub fn map_types(&self, inputs: &[PortType]) -> Vec<PortType> {
        let input = union_of(inputs);
        self.outputs.iter().map(|port| port.mapping.apply(input)).collect()
    }
}

/// Returns the type of a port that carries the records of every given port: the type of their merge.
///
/// The union of no ports carries nothing.
pub fn union_of(types: &[PortType]) -> PortType {
    types
        .iter()
        .copied()
        .reduce(|merged, port_type| merged.union(&port_type))
        .unwrap_or(PortType {
            kinds: KindSet::EMPTY,
            timestamps: TimestampForm::Mixed,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn port(kinds: &[Kind], timestamps: TimestampForm) -> PortType {
        PortType {
            kinds: KindSet::of(kinds),
            timestamps,
        }
    }

    #[test]
    fn kind_maps_rewrite_each_kind() {
        let to_rate = KindMap::IDENTITY.with(Kind::Counter, KindSet::of(&[Kind::Rate]));
        assert_eq!(to_rate.get(Kind::Counter), KindSet::of(&[Kind::Rate]));
        assert_eq!(to_rate.get(Kind::Gauge), KindSet::of(&[Kind::Gauge]));
        assert_eq!(
            to_rate.apply(KindSet::of(&[Kind::Counter, Kind::Gauge])),
            KindSet::of(&[Kind::Rate, Kind::Gauge])
        );
        assert_eq!(KindMap::IDENTITY.apply(KindSet::ALL), KindSet::ALL);

        let drop_histograms = KindMap::IDENTITY.with(Kind::Histogram, KindSet::EMPTY);
        assert_eq!(drop_histograms.apply(KindSet::of(&[Kind::Histogram])), KindSet::EMPTY);
    }

    #[test]
    fn timestamp_maps_select_preserve_or_set_the_form() {
        use TimestampForm::{Mixed, Timestamped, Untimestamped};

        let select = |form| PortMapping {
            kinds: KindMap::IDENTITY,
            timestamps: TimestampMap::Select(form),
        };
        let set = |form| PortMapping {
            kinds: KindMap::IDENTITY,
            timestamps: TimestampMap::Set(form),
        };
        let gauges = |form| port(&[Kind::Gauge], form);
        let nothing = |form| port(&[], form);

        // (mapping, input, expected)
        let cases = [
            (PortMapping::IDENTITY, gauges(Mixed), gauges(Mixed)),
            (select(Timestamped), gauges(Mixed), gauges(Timestamped)),
            (select(Timestamped), gauges(Timestamped), gauges(Timestamped)),
            // A port that selects timestamped points from untimestamped input carries nothing.
            (select(Timestamped), gauges(Untimestamped), nothing(Timestamped)),
            (select(Untimestamped), gauges(Timestamped), nothing(Untimestamped)),
            (set(Timestamped), gauges(Untimestamped), gauges(Timestamped)),
        ];
        for (mapping, input, expected) in cases {
            assert_eq!(mapping.apply(input), expected, "{:?} applied to {:?}", mapping, input);
        }
    }

    #[test]
    fn output_types_map_the_union_of_the_inputs() {
        let ports = Ports::single(PortType::ANY, PortMapping::IDENTITY);
        assert_eq!(ports.input(&PortName::from_static(IN)), Some(0));
        assert_eq!(ports.output(&PortName::from_static(OUT)), Some(0));
        assert_eq!(ports.output(&PortName::from_static(IN)), None);

        let inputs = [
            port(&[Kind::Rate], TimestampForm::Timestamped),
            port(&[Kind::Gauge], TimestampForm::Untimestamped),
        ];
        assert_eq!(
            ports.map_types(&inputs),
            vec![port(&[Kind::Rate, Kind::Gauge], TimestampForm::Mixed)]
        );
        assert_eq!(union_of(&[]).kinds, KindSet::EMPTY);
        assert!(Ports::sink().outputs.is_empty());
    }
}
