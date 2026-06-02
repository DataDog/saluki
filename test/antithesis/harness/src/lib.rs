//! Shared helpers for the Antithesis harness, used by the `src/bin/*` test
//! commands.

#[cfg(unix)]
pub mod driver;
pub mod payload;
pub mod rand;
