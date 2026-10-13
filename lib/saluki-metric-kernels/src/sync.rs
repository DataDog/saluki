//! The synchronization primitives of the records lanes share, which are `loom`'s in the crate's `loom` models.
//!
//! Code that the `loom` models run takes its primitives from here, so that the models exercise the code itself rather
//! than a transcription of it. Outside the crate's own tests with the `loom` feature, these are `std`'s.

#[cfg(not(all(test, feature = "loom")))]
pub(crate) use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex, MutexGuard,
};

#[cfg(all(test, feature = "loom"))]
pub(crate) use loom::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex, MutexGuard,
};
