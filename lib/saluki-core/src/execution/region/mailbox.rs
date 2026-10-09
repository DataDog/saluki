//! The control mailbox of a region host.

use std::{
    fmt,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use snafu::Snafu;
use tokio::sync::{mpsc, oneshot, watch};

use super::driver::RegionFailure;

/// Creates a control mailbox for one region host.
///
/// The sender belongs to whoever controls the region, such as an installer, and can be cloned. The mailbox belongs to
/// the region host, and is given to it through its builder.
pub fn control_mailbox<C>() -> (ControlSender<C>, ControlMailbox<C>) {
    let (requests, receiver) = mpsc::unbounded_channel();
    let (running, running_rx) = watch::channel(false);
    (
        ControlSender {
            requests,
            running: running_rx,
        },
        ControlMailbox { receiver, running },
    )
}

/// A control message on its way to a region host, with the channel its acknowledgment goes back on.
pub(super) struct ControlRequest<C> {
    pub(super) message: C,
    pub(super) reply: oneshot::Sender<Result<(), ControlError>>,
}

/// The sending side of a region host's control mailbox.
///
/// Sending never blocks and never waits for the region: a region that is busy, or blocked on a slow downstream
/// component, still accepts messages, and handles them as soon as it can, including while it waits for downstream.
/// Each message is acknowledged separately through the [`ControlAck`] that [`send`][Self::send] returns.
///
/// The mailbox queues messages without a bound, so the controller bounds them by its protocol: for example, by waiting
/// for the acknowledgment of one step before sending the next.
///
/// A region host is built well before it runs: a topology builds every component, then waits for its environment to
/// be ready before it starts them, which can take a long time. Messages sent meanwhile wait in the mailbox, so a
/// controller that times its messages out waits for [`running`][Self::running] first.
pub struct ControlSender<C> {
    requests: mpsc::UnboundedSender<ControlRequest<C>>,
    running: watch::Receiver<bool>,
}

impl<C> ControlSender<C> {
    /// Sends a control message to the region, and returns its acknowledgment.
    ///
    /// The message is queued immediately. If the region has already stopped, the acknowledgment reports
    /// [`ControlError::Stopped`].
    pub fn send(&self, message: C) -> ControlAck {
        let (reply, ack) = oneshot::channel();

        // If the region has stopped, sending fails and drops the request with its reply sender, which completes the
        // acknowledgment with `Stopped`.
        let _ = self.requests.send(ControlRequest { message, reply });

        ControlAck { ack }
    }

    /// Returns `true` if the region has stopped, so that no message sent from now on can be handled.
    pub fn is_closed(&self) -> bool {
        self.requests.is_closed()
    }

    /// Returns `true` once the region host has started running, and reads its mailbox.
    pub fn is_running(&self) -> bool {
        *self.running.borrow()
    }

    /// Waits until the region host has started running, and reads its mailbox.
    ///
    /// Returns `true` once it has, or `false` if its mailbox was dropped without a host ever running it, as when the
    /// topology fails to build.
    pub async fn running(&self) -> bool {
        let mut running = self.running.clone();
        let started = running.wait_for(|running| *running).await.is_ok();
        started
    }
}

impl<C> Clone for ControlSender<C> {
    fn clone(&self) -> Self {
        Self {
            requests: self.requests.clone(),
            running: self.running.clone(),
        }
    }
}

impl<C> fmt::Debug for ControlSender<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ControlSender")
            .field("running", &self.is_running())
            .field("closed", &self.is_closed())
            .finish()
    }
}

/// The receiving side of a region host's control mailbox.
///
/// A mailbox is handed to exactly one region host, through its builder.
pub struct ControlMailbox<C> {
    receiver: mpsc::UnboundedReceiver<ControlRequest<C>>,
    running: watch::Sender<bool>,
}

impl<C> ControlMailbox<C> {
    /// Marks the mailbox's region as running, as its host does when it starts.
    ///
    /// This is for tests that stand in for a region host which runs but never reads its mailbox.
    #[cfg(any(test, feature = "test-util"))]
    pub fn mark_running(&self) {
        self.running.send_replace(true);
    }

    pub(super) fn into_parts(self) -> (mpsc::UnboundedReceiver<ControlRequest<C>>, watch::Sender<bool>) {
        (self.receiver, self.running)
    }
}

impl<C> fmt::Debug for ControlMailbox<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ControlMailbox").finish_non_exhaustive()
    }
}

/// The acknowledgment of one control message.
///
/// This resolves once the region has handled the message: to `Ok(())` if its driver applied it, or to the
/// [`ControlError`] that says why not. Dropping the acknowledgment does not withdraw the message.
#[must_use = "an acknowledgment reports whether the region applied the control message"]
#[derive(Debug)]
pub struct ControlAck {
    ack: oneshot::Receiver<Result<(), ControlError>>,
}

impl Future for ControlAck {
    type Output = Result<(), ControlError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // A reply channel closed without a reply means the region stopped before it handled the message.
        Pin::new(&mut self.ack)
            .poll(cx)
            .map(|reply| reply.unwrap_or(Err(ControlError::Stopped)))
    }
}

/// Why a region did not apply a control message.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum ControlError {
    /// The region's driver could not apply the message.
    #[snafu(display("The region could not apply the control message: {failure}"))]
    Rejected {
        /// Why the driver could not apply the message.
        failure: RegionFailure,
    },

    /// The region's driver panicked while applying the message.
    ///
    /// The region recovered from the panic, or failed because of it; in either case, the message was not applied.
    #[snafu(display("The region panicked while applying the control message: {message}"))]
    Panicked {
        /// The panic's message.
        message: String,
    },

    /// The region stopped before it handled the message.
    #[snafu(display(
        "The region stopped before it handled the control message. The region is shutting down, or it failed; see its \
         logs for the reason."
    ))]
    Stopped,
}
