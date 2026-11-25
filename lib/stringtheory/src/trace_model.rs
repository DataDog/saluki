//! `libdd-trace-model` support.

use libdd_trace_model::TraceText;

use crate::MetaString;

/// Lets the `libdd-trace-model` traits use `MetaString` as their text type.
///
/// `from_static` doesn't allocate, and `From<String>` keeps the allocation of the `String`.
impl TraceText for MetaString {
    fn from_static(s: &'static str) -> Self {
        MetaString::from_static(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_text() {
        let s = <MetaString as TraceText>::from_static("static");
        assert!(s.is_cheaply_cloneable());
        assert_eq!(TraceText::as_str(&s), "static");

        let owned = <MetaString as From<String>>::from(String::from("a string that is too long to inline"));
        assert_eq!(TraceText::as_str(&owned), "a string that is too long to inline");
    }
}
