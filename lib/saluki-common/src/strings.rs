use stringtheory::{interning::Interner, MetaString};

/// A string builder that interns strings using an interner.
///
/// This builder is designed to allow building strings incrementally, and then interning them using a provided
/// interner. This can simplify certain patterns of string construction by removing the need to manually manage
/// the temporary string buffer and interner, clearing the buffer after interning, and so on.
///
/// # Limiting by length
///
/// The builder can also be configured to limit the overall length of the strings it builds.
pub struct InternedStringBuilder<I> {
    buf: String,
    limit: usize,
    interner: I,
}

impl<I> InternedStringBuilder<I>
where
    I: Interner,
{
    /// Creates a new `InternedStringBuilder` with the given interner.
    ///
    /// No limit is set for the strings built by this builder, and are only limited by the interner's capacity.
    pub fn new(interner: I) -> Self {
        InternedStringBuilder {
            buf: String::new(),
            limit: usize::MAX,
            interner,
        }
    }

    /// Creates a new `InternedStringBuilder` with the given interner and limit.
    ///
    /// Strings that exceed the limit will be discarded.
    pub fn with_limit(interner: I, limit: usize) -> Self {
        InternedStringBuilder {
            buf: String::new(),
            limit,
            interner,
        }
    }

    /// Returns `true` if the buffer of the builder is empty.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Returns the length of the buffer of the builder.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Clears the buffer of the builder.
    pub fn clear(&mut self) {
        self.buf.clear();
    }

    /// Pushes a string fragment into the builder.
    ///
    /// Returns `None` if the resulting string would exceed the configured limit.
    pub fn push_str(&mut self, s: &str) -> Option<()> {
        if self.buf.len() + s.len() > self.limit {
            return None;
        }
        self.buf.push_str(s);
        Some(())
    }

    /// Builds and interns the string.
    ///
    /// Returns `None` if the string exceeds the configured limit or if it cannot be interned.
    pub fn build(&mut self) -> Option<MetaString> {
        if self.buf.len() > self.limit {
            return None;
        }

        let interned = self.interner.try_intern(&self.buf);
        self.buf.clear();

        interned.map(MetaString::from)
    }
}

/// Sanitizes the input string by ensuring all characters are lowercase ASCII alphanumeric or underscores.
///
/// All characters that are not ASCII alphanumeric or underscores are replaced with underscores, and alphanumerics will
/// be lowercased.
pub fn lower_alphanumeric(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use stringtheory::interning::FixedSizeInterner;

    use super::*;

    fn build_interned_string_builder(interner_capacity: usize) -> InternedStringBuilder<FixedSizeInterner<1>> {
        InternedStringBuilder::new(FixedSizeInterner::new(NonZeroUsize::new(interner_capacity).unwrap()))
    }

    fn build_interned_string_builder_with_limit(
        interner_capacity: usize, limit: usize,
    ) -> InternedStringBuilder<FixedSizeInterner<1>> {
        InternedStringBuilder::with_limit(
            FixedSizeInterner::new(NonZeroUsize::new(interner_capacity).unwrap()),
            limit,
        )
    }

    #[test]
    fn lower_alphanumeric_basic() {
        assert_eq!(lower_alphanumeric("Hello World!"), "hello_world_");
        assert_eq!(lower_alphanumeric("1234"), "1234");
        assert_eq!(lower_alphanumeric("abc_def"), "abc_def");
        assert_eq!(lower_alphanumeric("abc-def"), "abc_def");
        assert_eq!(lower_alphanumeric("abc def"), "abc_def");
    }

    #[test]
    fn interned_string_builder_basic() {
        let mut builder = build_interned_string_builder(128);

        assert_eq!(builder.push_str("Hello World!"), Some(()));
        assert_eq!(builder.build(), Some(MetaString::from("Hello World!")));

        assert_eq!(builder.push_str("hello"), Some(()));
        assert_eq!(builder.push_str(" "), Some(()));
        assert_eq!(builder.push_str("world"), Some(()));
        assert_eq!(builder.build(), Some(MetaString::from("hello world")));
    }

    #[test]
    fn interned_string_builder_clear() {
        let mut builder = build_interned_string_builder(128);

        assert_eq!(builder.push_str("hello"), Some(()));
        builder.clear();
        assert_eq!(builder.build(), Some(MetaString::empty()));
    }

    #[test]
    fn interned_string_builder_is_empty_len() {
        let mut builder = build_interned_string_builder(128);

        // Starts out empty:
        assert!(builder.is_empty());
        assert_eq!(builder.len(), 0);

        // After pushing "hello":
        assert_eq!(builder.push_str("hello"), Some(()));
        assert!(!builder.is_empty());
        assert_eq!(builder.len(), 5);

        // Building the string should clear the internal buffer:
        assert_eq!(builder.build(), Some(MetaString::from("hello")));
        assert!(builder.is_empty());
        assert_eq!(builder.len(), 0);

        // After pushing "world":
        builder.push_str("world");
        assert!(!builder.is_empty());
        assert_eq!(builder.len(), 5);

        // Manually clearing the buffer:
        builder.clear();
        assert!(builder.is_empty());
        assert_eq!(builder.len(), 0);
    }

    #[test]
    fn interned_string_builder_with_limit() {
        const LIMIT: usize = 16;

        let mut builder = build_interned_string_builder_with_limit(128, LIMIT);

        // Under the limit:
        let string_one = "hello, world!";
        assert!(string_one.len() < LIMIT);
        assert_eq!(builder.push_str(string_one), Some(()));
        assert_eq!(builder.build(), Some(MetaString::from(string_one)));

        // Over the limit:
        let string_two = "definitely way too long";
        assert!(string_two.len() > LIMIT);
        assert_eq!(builder.push_str(string_two), None);

        builder.clear();

        // Under the limit, but we build it piecemeal:
        let string_three_parts = vec!["hello", " ", "world"];
        let string_three = string_three_parts.join("");
        assert!(string_three.len() < LIMIT);
        for string_three_part in string_three_parts {
            assert_eq!(builder.push_str(string_three_part), Some(()));
        }
        assert_eq!(builder.build(), Some(MetaString::from(string_three)));
    }

    #[test]
    fn interned_string_builder_under_limit_interner_full() {
        const INTERNER_CAPACITY: usize = 24;
        const LIMIT: usize = 64;

        let mut builder = build_interned_string_builder_with_limit(INTERNER_CAPACITY, LIMIT);

        // Under the limit but over the interner capacity.
        //
        // The pushes should succeed, but we should not be able to build the string due to
        // the interner not having enough space:
        let string_one = "are you there, god? it's me, margaret";
        assert!(string_one.len() < LIMIT);
        assert!(string_one.len() > INTERNER_CAPACITY);
        assert_eq!(builder.push_str(string_one), Some(()));
        assert_eq!(builder.build(), None);
    }
}
