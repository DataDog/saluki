//! Registry-managed network listeners.
//!
//! A bound listen address is the canonical scarce resource: only one thing in the process can hold it, releasing it
//! hands it back to the operating system, and re-binding can fail because something else took it in the meantime. The
//! specifications here let a [`ResourceRegistry`] own listeners on behalf of the whole process and lend them out, so a
//! component can be torn down and rebuilt without its sockets ever being released.
//!
//! Both specifications are [`ResourceKind::Socket`] and key on the listen address, so a listen address can only be held
//! once no matter which listener type is asking for it. Asking for the same address as both a [`Listener`] and a
//! [`ConnectionOrientedListener`] is an [`AcquireError::TypeMismatch`][saluki_core::runtime::state::AcquireError], since
//! one bound socket can't be reinterpreted as the other.

use std::num::NonZeroUsize;

use async_trait::async_trait;
use saluki_core::runtime::state::{ResourceKind, ResourceSpecification};
use saluki_error::GenericError;
use stringtheory::MetaString;

use super::{
    listener::{ConnectionOrientedListener, Listener},
    ListenAddress,
};

/// Specification for a general-purpose network listener.
///
/// Creates a [`Listener`], which handles both connection-oriented and connectionless address families.
///
/// Only the listen address identifies the resource. Tuning -- the UDP socket count, the receive buffer size -- is
/// applied when the listener is first created and has no effect on a later acquisition of the same address, since the
/// sockets are already bound. The registry logs a warning if a subsequent specification disagrees.
#[derive(Clone, Debug)]
pub struct SocketSpecification {
    address: ListenAddress,
    udp_streams: Option<NonZeroUsize>,
    receive_buffer_size: Option<usize>,
}

impl SocketSpecification {
    /// Creates a specification for the given listen address.
    pub fn new(address: ListenAddress) -> Self {
        Self {
            address,
            udp_streams: None,
            receive_buffer_size: None,
        }
    }

    /// Sets how many UDP sockets to bind to the address.
    ///
    /// Each socket is bound independently with `SO_REUSEPORT` so the kernel load-balances incoming datagrams across
    /// them, and the listener yields one stream per socket. Ignored for non-UDP addresses. See
    /// [`Listener::from_listen_address`] for the platform caveats.
    pub fn with_udp_streams(mut self, udp_streams: Option<NonZeroUsize>) -> Self {
        self.udp_streams = udp_streams;
        self
    }

    /// Sets the receive buffer size applied to streams accepted from this listener.
    ///
    /// `None` keeps the operating system default.
    pub fn with_receive_buffer_size(mut self, receive_buffer_size: Option<usize>) -> Self {
        self.receive_buffer_size = receive_buffer_size;
        self
    }

    /// Returns the listen address this specification names.
    pub fn address(&self) -> &ListenAddress {
        &self.address
    }
}

impl From<ListenAddress> for SocketSpecification {
    fn from(address: ListenAddress) -> Self {
        Self::new(address)
    }
}

#[async_trait]
impl ResourceSpecification for SocketSpecification {
    type Resource = Listener;

    const KIND: ResourceKind = ResourceKind::Socket;

    fn key(&self) -> MetaString {
        MetaString::from(self.address.to_string())
    }

    async fn create(&self) -> Result<Self::Resource, GenericError> {
        let listener = Listener::from_listen_address(self.address.clone(), self.udp_streams).await?;

        Ok(listener.with_receive_buffer_size(self.receive_buffer_size))
    }

    fn reset(listener: &mut Self::Resource) {
        // A listener hands out each of its pre-bound connectionless sockets once, so without this the next holder would
        // find it exhausted. The sockets themselves are untouched.
        listener.rearm();
    }
}

/// Specification for a connection-oriented network listener.
///
/// Creates a [`ConnectionOrientedListener`], which accepts connections on TCP or Unix stream addresses only.
#[derive(Clone, Debug)]
pub struct ConnectionOrientedSocketSpecification {
    address: ListenAddress,
}

impl ConnectionOrientedSocketSpecification {
    /// Creates a specification for the given listen address.
    pub fn new(address: ListenAddress) -> Self {
        Self { address }
    }

    /// Returns the listen address this specification names.
    pub fn address(&self) -> &ListenAddress {
        &self.address
    }
}

impl From<ListenAddress> for ConnectionOrientedSocketSpecification {
    fn from(address: ListenAddress) -> Self {
        Self::new(address)
    }
}

#[async_trait]
impl ResourceSpecification for ConnectionOrientedSocketSpecification {
    type Resource = ConnectionOrientedListener;

    const KIND: ResourceKind = ResourceKind::Socket;

    fn key(&self) -> MetaString {
        MetaString::from(self.address.to_string())
    }

    async fn create(&self) -> Result<Self::Resource, GenericError> {
        Ok(ConnectionOrientedListener::from_listen_address(self.address.clone()).await?)
    }

    // No `reset`: `accept` draws from the kernel's connection backlog rather than a fixed supply of pre-bound sockets,
    // so a connection-oriented listener carries nothing that belongs to a single holder.
}

#[cfg(test)]
mod tests;
