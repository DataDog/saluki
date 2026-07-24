//! This is a place for re-usable Saluki-only defaults.
//!
//! Important: this is *not* a place where we should restate Datadog Agent schema-derived defaults.
//! Defaults that flow from the Datadog schema are defined by codegen in the appropriate crate. We
//! only want the definitive codegen defaults rather than restating values that can drift (see
//! #1802).

use std::num::NonZeroUsize;

use saluki_io::net::ListenAddress;

/// Default OTLP trace string interner capacity: 512 KiB.
pub const DEFAULT_STRING_INTERNER_SIZE_BYTES: NonZeroUsize = NonZeroUsize::new(512 * 1024).unwrap();

/// Maximum OTLP trace string interner capacity: 1 GiB. Arbitrary to stop @blt from blowing us up.
pub const MAX_STRING_INTERNER_SIZE_BYTES: NonZeroUsize = NonZeroUsize::new(1024 * 1024 * 1024).unwrap();

/// Placeholder used to satisfy `Default` before source translation overwrites listen addresses.
///
/// This is not a viable configuration default and must not reach a runtime consumer.
pub const FAKE_LISTEN_ADDRESS: ListenAddress = ListenAddress::any_tcp(0);

/// The default IPC endpoint for checks: `tcp://0.0.0.0:5105`.
pub const DEFAULT_CHECKS_IPC_ENDPOINT: ListenAddress = ListenAddress::any_tcp(5105);
