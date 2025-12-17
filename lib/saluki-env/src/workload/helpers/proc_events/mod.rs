//! Netlink process connector client.
//!
//! This module provides a client for receiving process lifecycle events from the Linux kernel
//! via the netlink process connector interface.
//!
//! The process connector is a netlink-based mechanism that allows userspace programs to receive
//! notifications about process events such as fork, exec, and exit. It requires `CAP_NET_ADMIN`
//! capability or root privileges to subscribe to events.

use std::pin::Pin;

use futures::Stream;
use neli::{
    connector::{CnMsg, CnMsgBuilder, ProcEvent, ProcEventHeader},
    consts::{
        connector::{CnMsgIdx, CnMsgVal, ProcCnMcastOp},
        nl::Nlmsg,
        socket::NlFamily,
    },
    nl::{NlPayload, NlmsghdrBuilder},
    socket::asynchronous::NlSocketHandle,
    utils::Groups,
};
use snafu::{ResultExt as _, Snafu};
use tracing::{debug, error, trace, warn};

/// A [`ProcEventsClient`] error.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum ClientError {
    /// Failed to create or connect netlink socket.
    #[snafu(display("failed to connect netlink socket: {}", source))]
    SocketConnect { source: neli::err::SocketError },

    /// Failed to subscribe to process events.
    #[snafu(display("failed to subscribe to process events: {}", source))]
    Subscribe { source: neli::err::SocketError },

    /// Failed to build subscription message.
    #[snafu(display("failed to build subscription message"))]
    BuildMessage,

    /// Failed to receive from netlink socket.
    #[snafu(display("failed to receive from netlink socket: {}", source))]
    Receive { source: neli::err::SocketError },
}

/// Client for receiving process events from the kernel via netlink.
///
/// This client subscribes to process connector events and provides an async stream
/// of process lifecycle events (exec, exit, fork, etc.).
///
/// ## Requirements
///
/// - Linux kernel with `CONFIG_PROC_EVENTS` enabled
/// - `CAP_NET_ADMIN` capability or root privileges
pub struct ProcEventsClient {
    socket: NlSocketHandle,
}

impl ProcEventsClient {
    /// Creates a new `ProcEventsClient` and subscribes to process events.
    ///
    /// ## Errors
    ///
    /// Returns an error if the netlink socket cannot be created, bound, or if
    /// subscription fails. Common failure reasons include:
    /// - Insufficient privileges (need `CAP_NET_ADMIN` or root)
    /// - Kernel not compiled with `CONFIG_PROC_EVENTS`
    pub async fn new() -> Result<Self, ClientError> {
        let pid = std::process::id();

        // Connect to the netlink connector socket, joining the process connector multicast group
        let socket = NlSocketHandle::connect(
            NlFamily::Connector,
            Some(pid),
            Groups::new_bitmask(CnMsgIdx::Proc.into()),
        )
        .context(SocketConnect)?;

        // Build and send subscription message to start receiving process events
        let subscribe = NlmsghdrBuilder::default()
            .nl_type(Nlmsg::Done)
            .nl_pid(pid)
            .nl_payload(NlPayload::Payload(
                CnMsgBuilder::default()
                    .idx(CnMsgIdx::Proc)
                    .val(CnMsgVal::Proc)
                    .payload(ProcCnMcastOp::Listen)
                    .build()
                    .map_err(|_| ClientError::BuildMessage)?,
            ))
            .build()
            .map_err(|_| ClientError::BuildMessage)?;

        socket.send(&subscribe).await.context(Subscribe)?;

        debug!("Successfully subscribed to process events via netlink connector.");

        Ok(Self { socket })
    }

    /// Returns a stream of process events.
    ///
    /// The stream yields `ProcEvent` items as processes fork, exec, exit, etc.
    /// The collector typically filters for exec and exit events.
    ///
    /// The stream will continue indefinitely until an error occurs or the client is dropped.
    pub fn into_event_stream(self) -> Pin<Box<dyn Stream<Item = Result<ProcEvent, ClientError>> + Send>> {
        Box::pin(async_stream::stream! {
            loop {
                match self.socket.recv::<Nlmsg, CnMsg<ProcEventHeader>>().await {
                    Ok((messages, _groups)) => {
                        for msg_result in messages {
                            match msg_result {
                                Ok(msg) => {
                                    if let Some(payload) = msg.get_payload() {
                                        let event = payload.payload().event.clone();
                                        trace!(?event, "Received process event");
                                        yield Ok(event);
                                    }
                                }
                                Err(e) => {
                                    warn!(error = %e, "Failed to parse netlink message");
                                    // Continue processing other messages
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!(error = %e, "Error receiving from netlink socket");
                        yield Err(ClientError::Receive { source: e });
                    }
                }
            }
        })
    }
}

impl Drop for ProcEventsClient {
    fn drop(&mut self) {
        debug!("Dropping ProcEventsClient");
        // Note: The socket will be closed automatically when dropped.
        // We could send an Ignore message to unsubscribe, but it's not strictly necessary
        // since closing the socket will also stop event delivery.
    }
}
