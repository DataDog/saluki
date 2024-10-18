use std::{
    io::Write as _,
    net::{Ipv4Addr, TcpStream, UdpSocket},
    os::unix::net::{UnixDatagram, UnixStream},
};

use saluki_error::{ErrorContext as _, GenericError};

use crate::config::{Config, TargetAddress};

enum TargetBackend {
    Tcp(TcpStream),
    Udp(UdpSocket),
    UnixDatagram(UnixDatagram),
    Unix(UnixStream),
}

pub struct TargetSender {
    backend: TargetBackend,
}

impl TargetSender {
    /// Creates a new `TargetSender` based on the given configuration.
    ///
    /// # Errors
    ///
    /// If an error occurs while creating the socket/stream necessary for the target address, it will be returned.
    pub fn from_config(config: &Config) -> Result<Self, GenericError> {
        let backend = match &config.target {
            TargetAddress::Tcp(addr) => {
                let stream = TcpStream::connect(addr)
                    .with_error_context(|| format!("Failed to connect to TCP target '{}'.", addr))?;
                TargetBackend::Tcp(stream)
            }
            TargetAddress::Udp(addr) => {
                // We have to bind the socket first before we can "connect" it.
                let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).error_context("Failed to bind UDP socket.")?;
                let _ = socket
                    .connect(addr)
                    .with_error_context(|| format!("Failed to connect to UDP target '{}'.", addr))?;

                TargetBackend::Udp(socket)
            }
            TargetAddress::UnixDatagram(path) => {
                let datagram = UnixDatagram::unbound().error_context("Failed to bind Unix datagram socket.")?;
                let _ = datagram.connect(path).with_error_context(|| {
                    format!("Failed to connect to Unix datagram target '{}'.", path.display())
                })?;

                TargetBackend::UnixDatagram(datagram)
            }
            TargetAddress::Unix(path) => {
                let stream = UnixStream::connect(path)
                    .with_error_context(|| format!("Failed to connect to Unix stream target '{}'.", path.display()))?;
                TargetBackend::Unix(stream)
            }
        };

        Ok(Self { backend })
    }

    /// Sends a single payload to the target.
    ///
    /// Attempts to send the entire payload to the the target, but may only partially write a payload if the underlying
    /// target transport does not support ordered delivery of messages and fragmented sends cannot be achieved.
    ///
    /// On success, `Ok(n)` is returned, where `n` is the number of bytes sent.
    ///
    /// # Errors
    ///
    /// If an error occurs while sending the payload, it will be returned.
    pub fn send(&mut self, payload: &[u8]) -> Result<usize, GenericError> {
        let n = match &mut self.backend {
            TargetBackend::Tcp(stream) => stream.write_all(payload).map(|_| payload.len())?,
            TargetBackend::Udp(socket) => socket.send(payload)?,
            TargetBackend::UnixDatagram(datagram) => datagram.send(payload)?,
            TargetBackend::Unix(stream) => stream.write_all(payload).map(|_| payload.len())?,
        };

        Ok(n)
    }
}
