pub type GenericError = anyhow::Error;

/// Macro for constructing a generic error.
///
/// The resulting value evaulates to [`GenericError`], and can be construct from a string literal, a format string (with
/// arguments accepted, in the same order as `std::format!`), or a value which implements `Debug` and `Display`, such as
/// an existing error that implements `std::error::Error`.
///
/// When the value given implements `std::error::Error`, the source of the existing error value will be used as the source of the
/// error created by this macro.
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

pub(crate) mod private {
    pub trait Sealed {}

    impl<T, E> Sealed for Result<T, E> {}
}

// NOTE: We're wrapping `anyhow::Context` because otherwise the extension methods overlap with `snafu::ResultExt`, and
// this is just easier for scenarios where we want both.
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
