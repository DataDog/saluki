//! The client for a `datadog-intake` server's dump endpoints, shared by correctness and integration tests.

mod client;
pub use self::client::IntakeClient;

#[cfg(test)]
pub(crate) mod test_server;
