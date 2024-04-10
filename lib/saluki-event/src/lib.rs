use std::fmt;

use bitmask_enum::bitmask;

pub mod metric;
use self::metric::Metric;

#[bitmask(u8)]
#[bitmask_config(vec_debug)]
pub enum DataType {
    Metric,
    Log,
    Trace,
}

impl Default for DataType {
    fn default() -> Self {
        Self::all()
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut types = Vec::new();

        if self.contains(Self::Metric) {
            types.push("Metric");
        }

        if self.contains(Self::Log) {
            types.push("Log");
        }

        if self.contains(Self::Trace) {
            types.push("Trace");
        }

        write!(f, "{}", types.join("|"))
    }
}

#[derive(Clone)]
pub enum Event {
    Metric(Metric),
}

impl Event {
    pub fn into_metric(self) -> Option<Metric> {
        match self {
            Event::Metric(metric) => Some(metric),
        }
    }
}
