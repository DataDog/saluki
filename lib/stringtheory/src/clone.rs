use crate::MetaString;

/// A string type that can be cheaply cloned into a `MetaString`.
///
/// While callers working directly with `MetaString` already have access to determine if it is cheaply cloneable, there
/// are a number of use cases where `MetaString` is not directly accessible. This trait allows types wrapping
/// `MetaString` to expose the ability to cheaply clone the inner `MetaString` when possible, without having to expose
/// it directly.
pub trait CheapMetaString {
    /// Attempts to cheaply clone the string.
    ///
    /// If the string is not cheaply cloneable, `None` is returned.
    fn try_cheap_clone(&self) -> Option<MetaString>;
}

impl CheapMetaString for MetaString {
    fn try_cheap_clone(&self) -> Option<MetaString> {
        if self.is_cheaply_cloneable() {
            Some(self.clone())
        } else {
            None
        }
    }
}

impl CheapMetaString for &str {
    fn try_cheap_clone(&self) -> Option<MetaString> {
        MetaString::try_inline(self)
    }
}

impl CheapMetaString for String {
    fn try_cheap_clone(&self) -> Option<MetaString> {
        MetaString::try_inline(self)
    }
}

impl<T> CheapMetaString for &T
where
    T: CheapMetaString,
{
    fn try_cheap_clone(&self) -> Option<MetaString> {
        (*self).try_cheap_clone()
    }
}
