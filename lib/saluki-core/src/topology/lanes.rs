//! Edges into components with lanes.
//!
//! A transform may read several input channels, one per **lane** ([`TransformBuilder::input_lanes`]), each read by a
//! task of its own. Every edge into such a component names how its sender picks a lane ([`LaneEdge`]), and a sender
//! names the lane of each send ([`LaneRoute`]) when it dispatches. An edge into a component with one lane always leads
//! to that lane, whatever it names.
//!
//! Every lane's channel holds the component's input capacity, so a component with `n` lanes holds `n` times as many
//! buffers as it would with one. A send names one route, so an output routes by key, by affinity, or by spreading to
//! the lanes of at most one component; it may feed any number of components with one lane, and lanes it is fixed to,
//! besides.
//!
//! [`TransformBuilder::input_lanes`]: crate::components::transforms::TransformBuilder::input_lanes

use std::fmt;

/// How the sender of an edge into a component with lanes picks the lane of each buffer it sends.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum LaneEdge {
    /// Every buffer goes to this lane: an output that feeds one lane, such as one of a source's outputs, each of which
    /// feeds its own lane.
    Fixed(u16),

    /// The sender chooses the lane of each buffer, as a region's keyed exit does when it routes each event to the lane
    /// that owns its key ([`LaneRoute::Target`]).
    Keyed,

    /// Every buffer goes to the lane with the sender's own lane index, taken modulo the target's lanes
    /// ([`LaneRoute::From`]): lane `i` of a sender with lanes always feeds the same lane of the target.
    Affinity,

    /// Each buffer goes to the first lane with room, trying the target's lanes in rotation from a counter the sender
    /// keeps ([`LaneRoute::Spread`]), and waits only when every lane is full. Nothing downstream may need the sender's
    /// buffers in order.
    Spread,
}

impl LaneEdge {
    /// Returns `true` if the sender picks the lane of each send, by key, by affinity, or by spreading, and `false` if
    /// every buffer goes to one fixed lane.
    pub fn is_routed(&self) -> bool {
        !matches!(self, Self::Fixed(_))
    }
}

impl fmt::Display for LaneEdge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fixed(lane) => write!(f, "fixed to lane {}", lane),
            Self::Keyed => f.write_str("keyed"),
            Self::Affinity => f.write_str("affinity"),
            Self::Spread => f.write_str("spread"),
        }
    }
}

/// Which lane of an output's target one send goes to, as the edge's routing ([`LaneEdge`]) asks of the sender.
///
/// A send that names no route, such as an ordinary dispatch, is the send of a sender without lanes: an affinity edge
/// leads it to lane 0, as the only lane of such a sender is lane 0, and a spread edge spreads it from a counter the
/// output keeps. A keyed edge needs the lane the sender chose, so a send on it must name one.
#[derive(Debug, Eq, PartialEq)]
pub enum LaneRoute<'a> {
    /// The lane the sender chose, for a [keyed][LaneEdge::Keyed] edge.
    Target(u16),

    /// The sender's own lane index, taken modulo the target's lanes, for an [affinity][LaneEdge::Affinity] edge.
    From(u16),

    /// The sender's rotation counter, for a [spread][LaneEdge::Spread] edge: the send tries the target's lanes from it,
    /// and leaves it at the lane after the one it sent to.
    Spread(&'a mut u16),
}
