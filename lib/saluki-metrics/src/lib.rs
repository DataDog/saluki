mod macros;

/// A type that can be converted into a `SharedString`.
///
/// This is a blanket trait used to generically support converting any type which already supports conversion to
/// `String` into a `SharedString`. This is purely used by the `static_metrics!` macro to allow for ergonomic handling
/// of labels, and should generally not need to be implemented manually.
pub trait Stringable {
    fn to_shared_string(&self) -> ::metrics::SharedString;
}

impl<T> Stringable for T
where
    T: std::fmt::Display,
{
    fn to_shared_string(&self) -> ::metrics::SharedString {
        std::string::ToString::to_string(&self).into()
    }
}
