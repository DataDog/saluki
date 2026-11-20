//! Building blocks for reverse proxies that stream requests to an upstream server.
//!
//! These are distinct from `common::datadog::proxy`, which configures the HTTP proxy that outgoing requests go through.

mod destination;
pub(crate) use self::destination::{listener_reached_by, Destination};

mod forwarding;
pub(crate) use self::forwarding::{strip_hop_by_hop_headers, ForwardError, UpstreamClient};
