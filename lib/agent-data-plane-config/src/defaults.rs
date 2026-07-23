//! This is a place for re-usable Saluki-only defaults.
//!
//! Important: this is *not* a place where we should restate Datadog Agent schema-derived defaults.
//! Defaults that flow from the Datadog schema are defined by codegen in the appropriate crate. We
//! only want the definitive codegen defaults rather than restating values that can drift (see
//! #1802).

use std::num::NonZeroUsize;

/// Default OTLP trace string interner capacity: 512 KiB.
pub const DEFAULT_STRING_INTERNER_SIZE_BYTES: NonZeroUsize = NonZeroUsize::new(512 * 1024).unwrap();

/// Maximum OTLP trace string interner capacity: 1 GiB. Arbitrary to stop @blt from blowing us up.
pub const MAX_STRING_INTERNER_SIZE_BYTES: NonZeroUsize = NonZeroUsize::new(1024 * 1024 * 1024).unwrap();
