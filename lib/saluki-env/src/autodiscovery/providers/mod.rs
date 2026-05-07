//! Autodiscovery provider implementations.

mod boxed;
pub use self::boxed::BoxedAutodiscoveryProvider;

mod local;
pub use self::local::LocalAutodiscoveryProvider;
