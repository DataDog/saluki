//! A trait object-based error type based on `anyhow` for ergonomic error handling.
//!
//! This crate is a thin wrapper around `anyhow` that provides a trait object-based error type, `GenericError`, along
//! with helper extension traits and macros for ergonomically converting errors into `GenericError`.
//!
//! This crate is still interoperable with `anyhow`, but is meant to be used as part of the comprehensive set of
//! Saluki-specific crates so that engineers do not need to necessarily know or care deeply about which specific
//! third-party crate to use for error handling.
#![deny(warnings)]
#![deny(missing_docs)]

/// A wrapper around a dynamic error type.
///
/// `GenericError` works a lot like `Box<dyn std::error::Error>`, but with these differences:
///
/// - `GenericError` requires that the error is `Send`, `Sync`, and `'static`.
/// - `GenericError` guarantees that a backtrace is available, even if the underlying error type does not provide one.
/// - `GenericError` is represented as a narrow pointer — exactly one word in size instead of two.
pub type GenericError = anyhow::Error;

/// Macro for constructing a generic error.
///
/// The resulting value evaulates to [`GenericError`], and can be construct from a string literal, a format string (with
/// arguments accepted, in the same order as `std::format!`), or a value which implements `Debug` and `Display`, such as
/// an existing error that implements `std::error::Error`.
///
/// When the value given implements `std::error::Error`, the source of the existing error value will be used as the
/// source of the error created by this macro.
#[macro_export]
macro_rules! generic_error {
    // This macro forwards to the [`anyhow::anyhow`] macro, and is intended to be used in place of that macro. We simply
    // use our own macro, instead of re-exporting it, so that we can provide better documentation that isn't
    // `anyhow`-specific.
    ($msg:literal $(,)?) => { $crate::_anyhow!($msg) };
    ($err:expr $(,)?) => { $crate::_anyhow!($err) };
    ($fmt:expr, $($arg:tt)*) => { $crate::_anyhow!($fmt, $($arg)*) };
}

use std::fmt::Display;

#[doc(hidden)]
pub use anyhow::anyhow as _anyhow;

mod private {
    pub trait Sealed {}

    impl<T, E> Sealed for Result<T, E> {}
}

/// Helper methods for providing context on errors.
///
/// Slightly different than `anyhow::Context` as the methods on this trait are named to avoid conflicting with
/// similarly-named methods provide by `snafu`.
pub trait ErrorContext<T, E>: private::Sealed {
    /// Wrap the error value with additional context.
    fn error_context<C>(self, context: C) -> Result<T, GenericError>
    where
        C: Display + Send + Sync + 'static;

    /// Wrap the error value with additional context that is evaluated lazily only once an error does occur.
    fn with_error_context<C, F>(self, f: F) -> Result<T, GenericError>
    where
        C: Display + Send + Sync + 'static,
        F: FnOnce() -> C;
}

impl<T, E> ErrorContext<T, E> for Result<T, E>
where
    Result<T, E>: anyhow::Context<T, E>,
{
    fn error_context<C>(self, context: C) -> Result<T, GenericError>
    where
        C: Display + Send + Sync + 'static,
    {
        <Self as anyhow::Context<T, E>>::context(self, context)
    }

    fn with_error_context<C, F>(self, context: F) -> Result<T, GenericError>
    where
        C: Display + Send + Sync + 'static,
        F: FnOnce() -> C,
    {
        <Self as anyhow::Context<T, E>>::with_context(self, context)
    }
}
