//! Host provider implementations.

mod boxed;
pub use self::boxed::BoxedHostProvider;

mod fixed;
pub use self::fixed::FixedHostProvider;
