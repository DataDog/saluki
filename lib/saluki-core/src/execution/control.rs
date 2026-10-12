//! Control mailboxes: how a controller sends messages to a long-running component without ever blocking on it.
//!
//! A controller, such as an installer preparing a new generation of a program, holds a [`ControlSender`], and sends
//! each message with [`send`][ControlSender::send], which returns at once with the message's [`ControlAck`]. The
//! component holds the [`ControlMailbox`], turns it into a [`ControlReceiver`] when it is built, marks it running when
//! it starts, and answers each message it receives through the message's [`ControlReply`].
//!
//! The mailbox is generic over the message type, and knows nothing about what the messages mean: a region host
//! ([`RegionHostBuilder`][super::RegionHostBuilder]) serves one, and so can any other component that a controller
//! changes while it runs. A component names what it is when it takes its receiver
//! ([`into_receiver_for`][ControlMailbox::into_receiver_for]), and every [`ControlError`] of its messages says so.

use std::{
    fmt,
    future::Future,
    pin::Pin,
    sync::{Arc, OnceLock},
    task::{Context, Poll},
};

use saluki_error::GenericError;
use snafu::Snafu;
use tokio::sync::{mpsc, oneshot, watch};

/// What a component is called in the errors of its control messages until it names itself.
const UNNAMED_COMPONENT: &str = "component";

/// What a mailbox's component is called in the errors of its control messages, once it took its receiver.
type ComponentName = Arc<OnceLock<&'static str>>;

fn component_name(name: &ComponentName) -> &'static str {
    name.get().copied().unwrap_or(UNNAMED_COMPONENT)
}

/// Creates a control mailbox for one component.
///
/// The sender belongs to whoever controls the component, such as an installer, and can be cloned. The mailbox belongs
/// to the component, and is given to it when it is built.
pub fn control_mailbox<C>() -> (ControlSender<C>, ControlMailbox<C>) {
    let (requests, receiver) = mpsc::unbounded_channel();
    let (running, running_rx) = watch::channel(false);
    let component = ComponentName::default();
    (
        ControlSender {
            requests,
            running: running_rx,
            component: Arc::clone(&component),
        },
        ControlMailbox {
            receiver,
            running,
            component,
        },
    )
}

/// A control message on its way to a component, with the channel its acknowledgment goes back on.
struct ControlRequest<C> {
    message: C,
    reply: oneshot::Sender<Result<(), ControlError>>,
}

/// The sending side of a component's control mailbox.
///
/// Sending never blocks and never waits for the component: a component that is busy, or blocked on a slow downstream
/// component, still accepts messages, and handles them as soon as it can, including while it waits for downstream if
/// it serves its mailbox then. Each message is acknowledged separately through the [`ControlAck`] that
/// [`send`][Self::send] returns.
///
/// The mailbox queues messages without a bound, so the controller bounds them by its protocol: for example, by waiting
/// for the acknowledgment of one step before sending the next.
///
/// A component is built well before it runs: a topology builds every component, then waits for its environment to be
/// ready before it starts them, which can take a long time. Messages sent meanwhile wait in the mailbox, so a
/// controller that times its messages out waits for [`running`][Self::running] first.
pub struct ControlSender<C> {
    requests: mpsc::UnboundedSender<ControlRequest<C>>,
    running: watch::Receiver<bool>,
    component: ComponentName,
}

impl<C> ControlSender<C> {
    /// Sends a control message to the component, and returns its acknowledgment.
    ///
    /// The message is queued immediately. If the component has already stopped, the acknowledgment reports
    /// [`ControlError::Stopped`].
    pub fn send(&self, message: C) -> ControlAck {
        let (reply, ack) = oneshot::channel();

        // If the component has stopped, sending fails and drops the request with its reply sender, which completes the
        // acknowledgment with `Stopped`.
        let _ = self.requests.send(ControlRequest { message, reply });

        ControlAck {
            ack,
            component: Arc::clone(&self.component),
        }
    }

    /// Returns `true` if the component has stopped, so that no message sent from now on can be handled.
    pub fn is_closed(&self) -> bool {
        self.requests.is_closed()
    }

    /// Returns `true` once the component has started running, and reads its mailbox.
    pub fn is_running(&self) -> bool {
        *self.running.borrow()
    }

    /// Waits until the component has started running, and reads its mailbox.
    ///
    /// Returns `true` once it has, or `false` if its mailbox was dropped without the component ever running it, as when
    /// the topology fails to build.
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
            component: Arc::clone(&self.component),
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

/// The receiving side of a component's control mailbox, before the component is built.
///
/// A mailbox is handed to exactly one component, which turns it into a [`ControlReceiver`] with
/// [`into_receiver_for`][Self::into_receiver_for] or [`into_receiver`][Self::into_receiver].
pub struct ControlMailbox<C> {
    receiver: mpsc::UnboundedReceiver<ControlRequest<C>>,
    running: watch::Sender<bool>,
    component: ComponentName,
}

impl<C> ControlMailbox<C> {
    /// Marks the mailbox's component as running, as a component does when it starts reading its mailbox.
    ///
    /// Senders waiting in [`ControlSender::running`] return. A component normally marks its [`ControlReceiver`]
    /// instead; this is for a component, or a test standing in for one, that runs before it takes its receiver.
    pub fn mark_running(&self) {
        self.running.send_replace(true);
    }

    /// Returns the receiver a component reads its messages from, calling the component `component` in the errors of its
    /// messages, such as `"region"` in the message "The region could not apply the control message."
    ///
    /// The receiver is not marked running: the component calls [`ControlReceiver::mark_running`] once it starts.
    pub fn into_receiver_for(self, component: &'static str) -> ControlReceiver<C> {
        // A mailbox is turned into one receiver, so the name is never set already.
        let _ = self.component.set(component);
        ControlReceiver {
            receiver: self.receiver,
            running: self.running,
            component,
        }
    }

    /// Returns the receiver the component reads its messages from, calling the component "component" in the errors of
    /// its messages. See [`into_receiver_for`][Self::into_receiver_for].
    pub fn into_receiver(self) -> ControlReceiver<C> {
        self.into_receiver_for(UNNAMED_COMPONENT)
    }
}

impl<C> fmt::Debug for ControlMailbox<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ControlMailbox").finish_non_exhaustive()
    }
}

/// The receiving side of a component's control mailbox, which the component reads its messages from.
pub struct ControlReceiver<C> {
    receiver: mpsc::UnboundedReceiver<ControlRequest<C>>,
    running: watch::Sender<bool>,
    component: &'static str,
}

impl<C> ControlReceiver<C> {
    /// Marks the component as running: from now on, it reads its mailbox, so controllers can start timing their
    /// messages.
    pub fn mark_running(&self) {
        self.running.send_replace(true);
    }

    /// Waits for the next control message, and returns it with the reply its acknowledgment goes back through.
    ///
    /// Returns `None` once every [`ControlSender`] is gone and every message has been received.
    ///
    /// # Cancel safety
    ///
    /// This method is cancel-safe: a message is taken from the mailbox only when the call returns it, so a component
    /// can wait on it in a `select!` beside its other work without losing one.
    pub async fn recv(&mut self) -> Option<(C, ControlReply)> {
        let ControlRequest { message, reply } = self.receiver.recv().await?;
        let component = self.component;
        Some((message, ControlReply { reply, component }))
    }
}

impl<C> fmt::Debug for ControlReceiver<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ControlReceiver")
            .field("component", &self.component)
            .field("running", &*self.running.borrow())
            .finish_non_exhaustive()
    }
}

/// The reply to one control message, which completes the message's [`ControlAck`].
///
/// Dropping a reply without answering it acknowledges the message with [`ControlError::Stopped`], as if the component
/// had stopped before it handled the message.
#[must_use = "a dropped reply acknowledges the control message as unhandled"]
#[derive(Debug)]
pub struct ControlReply {
    reply: oneshot::Sender<Result<(), ControlError>>,
    component: &'static str,
}

impl ControlReply {
    /// Acknowledges that the component applied the message.
    pub fn ack(self) {
        self.send(Ok(()));
    }

    /// Acknowledges that the component could not apply the message, and why.
    pub fn reject(self, error: GenericError) {
        let component = self.component;
        self.send(Err(ControlError::Rejected { component, error }));
    }

    /// Acknowledges that the component panicked while it applied the message, with the panic's message.
    pub fn panicked(self, message: String) {
        let component = self.component;
        self.send(Err(ControlError::Panicked { component, message }));
    }

    fn send(self, outcome: Result<(), ControlError>) {
        // The sender may have stopped waiting for the acknowledgment, which is fine.
        let _ = self.reply.send(outcome);
    }
}

/// The acknowledgment of one control message.
///
/// This resolves once the component has handled the message: to `Ok(())` if it applied it, or to the [`ControlError`]
/// that says why not. Dropping the acknowledgment does not withdraw the message.
#[must_use = "an acknowledgment reports whether the component applied the control message"]
#[derive(Debug)]
pub struct ControlAck {
    ack: oneshot::Receiver<Result<(), ControlError>>,
    component: ComponentName,
}

impl Future for ControlAck {
    type Output = Result<(), ControlError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // A reply channel closed without a reply means the component stopped before it handled the message.
        let component = component_name(&self.component);
        Pin::new(&mut self.ack)
            .poll(cx)
            .map(|reply| reply.unwrap_or(Err(ControlError::Stopped { component })))
    }
}

/// Why a component did not apply a control message.
///
/// Each error names the component as it named itself when it took its receiver
/// ([`ControlMailbox::into_receiver_for`]), "component" if it did not.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum ControlError {
    /// The component could not apply the message.
    ///
    /// Displayed with the error's chain of causes.
    #[snafu(display("The {component} could not apply the control message: {error:#}"))]
    Rejected {
        /// What the component is, such as `"region"`.
        component: &'static str,

        /// Why the component could not apply the message.
        error: GenericError,
    },

    /// The component panicked while applying the message.
    ///
    /// The component recovered from the panic, or failed because of it; in either case, the message was not applied.
    #[snafu(display("The {component} panicked while applying the control message: {message}"))]
    Panicked {
        /// What the component is, such as `"region"`.
        component: &'static str,

        /// The panic's message.
        message: String,
    },

    /// The component stopped before it handled the message.
    #[snafu(display(
        "The {component} stopped before it handled the control message. The {component} is shutting down, or it \
         failed; see its logs for the reason."
    ))]
    Stopped {
        /// What the component is, such as `"region"`.
        component: &'static str,
    },
}

#[cfg(test)]
mod tests {
    use saluki_error::generic_error;

    use super::*;

    #[tokio::test]
    async fn each_reply_completes_its_own_acknowledgment() {
        let (sender, mailbox) = control_mailbox::<&'static str>();
        let mut receiver = mailbox.into_receiver();

        let applied = sender.send("apply");
        let rejected = sender.send("reject");
        let panicked = sender.send("panic");
        let unanswered = sender.send("drop");

        let (message, reply) = receiver.recv().await.unwrap();
        assert_eq!(message, "apply");
        reply.ack();
        let (_, reply) = receiver.recv().await.unwrap();
        reply.reject(generic_error!("not prepared"));
        let (_, reply) = receiver.recv().await.unwrap();
        reply.panicked("boom".to_string());
        let (_, reply) = receiver.recv().await.unwrap();
        drop(reply);

        assert!(applied.await.is_ok());
        match rejected.await {
            Err(ControlError::Rejected { error, .. }) => assert_eq!(error.to_string(), "not prepared"),
            other => panic!("expected a rejection, got {:?}", other),
        }
        assert!(matches!(panicked.await, Err(ControlError::Panicked { message, .. }) if message == "boom"));
        assert!(matches!(unanswered.await, Err(ControlError::Stopped { .. })));
    }

    #[tokio::test]
    async fn every_error_names_the_component_as_it_named_itself_when_it_took_its_receiver() {
        let (sender, mailbox) = control_mailbox::<&'static str>();
        let mut receiver = mailbox.into_receiver_for("region");
        let rejected = sender.send("reject");
        let panicked = sender.send("panic");
        let unanswered = sender.send("drop");
        let (_, reply) = receiver.recv().await.unwrap();
        reply.reject(generic_error!("Generation 7 is not prepared."));
        let (_, reply) = receiver.recv().await.unwrap();
        reply.panicked("boom".to_string());
        let (_, reply) = receiver.recv().await.unwrap();
        drop(reply);
        let message = |ack: Result<(), ControlError>| ack.unwrap_err().to_string();
        assert_eq!(
            message(rejected.await),
            "The region could not apply the control message: Generation 7 is not prepared."
        );
        assert_eq!(
            message(panicked.await),
            "The region panicked while applying the control message: boom"
        );
        let stopped = "The region stopped before it handled the control message. The region is shutting down, or it \
                       failed; see its logs for the reason.";
        assert_eq!(message(unanswered.await), stopped);

        // A message the component never receives, because its receiver is gone, is acknowledged under the same name.
        drop(receiver);
        assert_eq!(message(sender.send("late").await), stopped);

        // A component that does not name itself is "the component", and so is one whose mailbox was never taken.
        let (sender, mailbox) = control_mailbox::<&'static str>();
        let mut receiver = mailbox.into_receiver();
        let ack = sender.send("reject");
        let (_, reply) = receiver.recv().await.unwrap();
        reply.reject(generic_error!("no"));
        assert_eq!(
            message(ack.await),
            "The component could not apply the control message: no"
        );
        let (sender, mailbox) = control_mailbox::<&'static str>();
        drop(mailbox);
        assert!(message(sender.send("late").await).starts_with("The component stopped"));
    }

    #[tokio::test]
    async fn the_receiver_ends_once_every_sender_is_gone_and_a_dropped_receiver_stops_the_component() {
        let (sender, mailbox) = control_mailbox::<u32>();
        let mut receiver = mailbox.into_receiver();
        let ack = sender.send(1);
        drop(sender);
        let (message, reply) = receiver
            .recv()
            .await
            .expect("a message sent before the senders went is received");
        assert_eq!(message, 1);
        reply.ack();
        assert!(ack.await.is_ok());
        assert!(receiver.recv().await.is_none());

        let (sender, mailbox) = control_mailbox::<u32>();
        drop(mailbox.into_receiver());
        assert!(sender.is_closed());
        assert!(matches!(sender.send(2).await, Err(ControlError::Stopped { .. })));
    }

    #[tokio::test]
    async fn senders_see_the_component_running_once_it_marks_its_mailbox_or_its_receiver() {
        let (sender, mailbox) = control_mailbox::<()>();
        assert!(!sender.is_running());
        let receiver = mailbox.into_receiver();
        assert!(!sender.is_running());
        receiver.mark_running();
        assert!(sender.is_running());
        assert!(sender.running().await);

        let (sender, mailbox) = control_mailbox::<()>();
        mailbox.mark_running();
        assert!(sender.running().await);

        // A mailbox dropped before the component ran it: senders stop waiting.
        let (sender, mailbox) = control_mailbox::<()>();
        drop(mailbox);
        assert!(!sender.running().await);
    }
}
