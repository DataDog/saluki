//! Workload provider implementations.

mod noop;
pub use self::noop::NoopWorkloadProvider;

#[cfg(any(test, feature = "test-util"))]
mod testing;
#[cfg(any(test, feature = "test-util"))]
pub use self::testing::TestWorkloadProvider;
