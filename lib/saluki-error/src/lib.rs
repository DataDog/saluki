//! A trait object-based error type based on `anyhow` for ergonomic error handling.
//!
//! This crate is a thin wrapper around `anyhow` that provides a trait object-based error type, `GenericError`, along
//! with helper extension traits and macros for ergonomically converting errors into `GenericError`.
//!
//! This crate is still interoperable with `anyhow`, but is meant to be used as part of the comprehensive set of
//! Saluki-specific crates so that engineers don't need to necessarily know or care deeply about which specific
//! third-party crate to use for error handling.
#![deny(warnings)]
#![deny(missing_docs)]

/// A wrapper around a dynamic error type.
///
/// `GenericError` works a lot like `Box<dyn std::error::Error>`, but with these differences:
///
/// - `GenericError` requires that the error is `Send`, `Sync`, and `'static`.
/// - `GenericError` guarantees that a backtrace is available, even if the underlying error type doesn't provide one.
/// - `GenericError` is represented as a narrow pointer—exactly one word in size instead of two.
pub type GenericError = anyhow::Error;

/// Macro for constructing a generic error.
///
/// The resulting value evaluates to [`GenericError`], and can be construct from a string literal, a format string (with
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
/// similarly named methods provide by `snafu`.
pub trait ErrorContext<T, E>: private::Sealed {
    /// Wrap the error value with additional context.
    fn error_context<C>(self, context: C) -> Result<T, GenericError>
    where
        C: Display + Send + Sync + 'static;

    /// Wrap the error value with additional context that's evaluated lazily only once an error does occur.
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

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fmt;

    use super::ErrorContext as _;

    /// A leaf error with no source of its own.
    #[derive(Debug)]
    struct LeafError;

    impl fmt::Display for LeafError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "leaf failure")
        }
    }

    impl std::error::Error for LeafError {}

    /// An error that carries [`LeafError`] as its source.
    #[derive(Debug)]
    struct WrappingError {
        source: LeafError,
    }

    impl fmt::Display for WrappingError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "wrapping failure")
        }
    }

    impl std::error::Error for WrappingError {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.source)
        }
    }

    #[test]
    fn generic_error_from_string_literal_uses_message_as_display() {
        let err = generic_error!("boom");
        assert_eq!(err.to_string(), "boom");
    }

    #[test]
    fn generic_error_from_format_string_interpolates_arguments() {
        let err = generic_error!("value {} out of range {}", 42, "here");
        assert_eq!(err.to_string(), "value 42 out of range here");
    }

    #[test]
    fn generic_error_from_error_value_forwards_source_chain() {
        // Documented contract: when the value implements `std::error::Error`, the source of that error
        // becomes part of the constructed `GenericError`'s chain, so the root cause is the wrapped
        // error's own source rather than the wrapper itself.
        let err = generic_error!(WrappingError { source: LeafError });
        assert_eq!(err.to_string(), "wrapping failure");
        assert_eq!(err.root_cause().to_string(), "leaf failure");
    }

    #[test]
    fn error_context_passes_through_ok_unchanged() {
        let result: Result<i32, std::io::Error> = Ok(7);
        let contextualized = result.error_context("this context should never appear");
        assert_eq!(contextualized.unwrap(), 7);
    }

    #[test]
    fn error_context_wraps_err_with_context_and_preserves_source() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "permission denied");
        let result: Result<(), std::io::Error> = Err(io_err);

        let err = result.error_context("while loading config").unwrap_err();
        assert_eq!(err.to_string(), "while loading config");
        assert_eq!(err.root_cause().to_string(), "permission denied");
    }

    #[test]
    fn with_error_context_is_not_evaluated_on_ok() {
        let called = Cell::new(false);
        let result: Result<i32, std::io::Error> = Ok(1);

        let contextualized = result.with_error_context(|| {
            called.set(true);
            "lazy context"
        });

        assert_eq!(contextualized.unwrap(), 1);
        assert!(!called.get(), "context closure must not run on the Ok path");
    }

    #[test]
    fn with_error_context_is_evaluated_and_applied_on_err() {
        let called = Cell::new(false);
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let result: Result<(), std::io::Error> = Err(io_err);

        let err = result
            .with_error_context(|| {
                called.set(true);
                "lazy context"
            })
            .unwrap_err();

        assert!(called.get(), "context closure must run on the Err path");
        assert_eq!(err.to_string(), "lazy context");
        assert_eq!(err.root_cause().to_string(), "missing");
    }
}
