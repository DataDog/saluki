use stringtheory::{interning::Interner, MetaString};

/// A string builder.
///
///
/// This builder is designed to allow building strings incrementally. This can simplify certain patterns of string
/// construction by removing the need to manually manage a temporary string buffer, clearing it after building the
/// resulting string, and so on.
///
/// # Limits
///
/// The builder can be configured to limit the overall length of the strings it builds.
///
/// # Interning
///
/// The builder supports providing an interner that is used to intern the finalized string. This allows for
/// efficiently building strings, reusing the intermediate buffer in between before eventually interning the string.
pub struct StringBuilder<I = ()> {
    buf: String,
    limit: usize,
    interner: I,
}

impl StringBuilder<()> {
    /// Creates a new `StringBuilder`.
    ///
    /// No limit is set for the strings built by this builder.
    pub fn new() -> Self {
        Self {
            buf: String::new(),
            limit: usize::MAX,
            interner: (),
        }
    }

    /// Creates a new `StringBuilder` with the given limit.
    ///
    /// Strings that exceed the limit, in bytes, will be discarded.
    pub fn with_limit(limit: usize) -> Self {
        Self {
            buf: String::new(),
            limit,
            interner: (),
        }
    }
}

impl<I> StringBuilder<I> {
    /// Configures this builder with the given interner.
    pub fn with_interner<I2>(self, interner: I2) -> StringBuilder<I2>
    where
        I2: Interner,
    {
        StringBuilder {
            buf: self.buf,
            limit: self.limit,
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

    /// Returns the available space in the buffer of the builder.
    pub fn available(&self) -> usize {
        self.limit - self.buf.len()
    }

    /// Clears the buffer of the builder.
    pub fn clear(&mut self) {
        self.buf.clear();
    }

    /// Pushes a character into the builder.
    ///
    /// Returns `None` if the buffer limit would be exceeded by writing the character.
    pub fn push(&mut self, c: char) -> Option<()> {
        let char_len = c.len_utf8();
        if self.buf.len() + char_len > self.limit {
            return None;
        }
        self.buf.push(c);
        Some(())
    }

    /// Pushes a string fragment into the builder.
    ///
    /// Returns `None` if the buffer limit would be exceeded by writing the string.
    pub fn push_str(&mut self, s: &str) -> Option<()> {
        if self.buf.len() + s.len() > self.limit {
            return None;
        }
        self.buf.push_str(s);
        Some(())
    }

    /// Returns a references to the current string.
    pub fn as_str(&self) -> &str {
        &self.buf
    }
}

impl<I> StringBuilder<I>
where
    I: Interner,
{
    /// Attempts to build and intern the string.
    ///
    /// Returns `None` if the string exceeds the configured limit or if it cannot be interned.
    pub fn try_intern(&mut self) -> Option<MetaString> {
        self.interner.try_intern(self.as_str()).map(MetaString::from)
    }
}

impl std::fmt::Write for StringBuilder {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        match self.push_str(s) {
            Some(()) => Ok(()),
            None => Err(std::fmt::Error),
        }
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
    use std::{fmt::Write, num::NonZeroUsize};

    use stringtheory::interning::FixedSizeInterner;

    use super::*;

    fn build_string_builder() -> StringBuilder {
        StringBuilder::new()
    }

    fn build_string_builder_with_limit(limit: usize) -> StringBuilder {
        StringBuilder::with_limit(limit)
    }

    fn build_interned_string_builder(interner_capacity: usize) -> StringBuilder<FixedSizeInterner<1>> {
        StringBuilder::new().with_interner(FixedSizeInterner::new(NonZeroUsize::new(interner_capacity).unwrap()))
    }

    fn build_interned_string_builder_with_limit(
        interner_capacity: usize, limit: usize,
    ) -> StringBuilder<FixedSizeInterner<1>> {
        StringBuilder::with_limit(limit)
            .with_interner(FixedSizeInterner::new(NonZeroUsize::new(interner_capacity).unwrap()))
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
    fn string_builder_basic() {
        let mut builder = build_string_builder();

        assert_eq!(builder.push_str("Hello World!"), Some(()));
        assert_eq!(builder.as_str(), "Hello World!");

        builder.clear();

        assert_eq!(builder.push_str("hello"), Some(()));
        assert_eq!(builder.push(' '), Some(()));
        assert_eq!(builder.push_str("world"), Some(()));
        assert_eq!(builder.as_str(), "hello world");
    }

    #[test]
    fn string_builder_basic_with_interner() {
        let mut builder = build_interned_string_builder(128);

        assert_eq!(builder.push_str("Hello World!"), Some(()));
        assert_eq!(builder.try_intern(), Some(MetaString::from("Hello World!")));

        builder.clear();

        assert_eq!(builder.push_str("hello"), Some(()));
        assert_eq!(builder.push_str(" "), Some(()));
        assert_eq!(builder.push_str("world"), Some(()));
        assert_eq!(builder.try_intern(), Some(MetaString::from("hello world")));
    }

    #[test]
    fn string_builder_clear() {
        let mut builder = build_string_builder();

        assert_eq!(builder.push_str("hello"), Some(()));
        builder.clear();
        assert_eq!(builder.as_str(), "");
    }

    #[test]
    fn string_builder_is_empty_len_available() {
        const LIMIT: usize = 32;

        let mut builder = build_string_builder_with_limit(LIMIT);

        // Starts out empty:
        assert!(builder.is_empty());
        assert_eq!(builder.len(), 0);
        assert_eq!(builder.available(), LIMIT);

        // After pushing "hello":
        assert_eq!(builder.push_str("hello"), Some(()));
        assert!(!builder.is_empty());
        assert_eq!(builder.len(), 5);
        assert_eq!(builder.available(), LIMIT - 5);
        assert_eq!(builder.as_str(), "hello");

        // After pushing " world":
        builder.push_str(" world");
        assert!(!builder.is_empty());
        assert_eq!(builder.len(), 11);
        assert_eq!(builder.available(), LIMIT - 11);
        assert_eq!(builder.as_str(), "hello world");

        // Manually clearing the buffer:
        builder.clear();
        assert!(builder.is_empty());
        assert_eq!(builder.len(), 0);
        assert_eq!(builder.available(), LIMIT);
    }

    #[test]
    fn string_builder_with_limit() {
        const LIMIT: usize = 16;

        let mut builder = build_string_builder_with_limit(LIMIT);

        // Under the limit:
        let string_one = "hello, world!";
        assert!(string_one.len() < LIMIT);
        assert_eq!(builder.push_str(string_one), Some(()));
        assert_eq!(builder.as_str(), string_one);

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
        assert_eq!(builder.as_str(), string_three);
    }

    #[test]
    fn string_builder_under_limit_interner_full() {
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
        assert_eq!(builder.try_intern(), None);
    }

    #[test]
    fn string_builder_fmt_write() {
        let mut builder = build_string_builder();

        let name = "steve from blues clues";
        let num_apples = 5;

        write!(builder, "hello, world!").unwrap();
        write!(builder, " it's me, {}.", name).unwrap();
        write!(builder, " i've got {} apples.", num_apples).unwrap();

        assert_eq!(
            builder.as_str(),
            "hello, world! it's me, steve from blues clues. i've got 5 apples."
        );
    }
}
