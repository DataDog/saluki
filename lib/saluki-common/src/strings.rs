use lexical_core::{ToLexical, ToLexicalWithOptions as _, WriteFloatOptions};
use stringtheory::{interning::Interner, MetaString};

const FLOAT_FORMAT: u128 = lexical_core::format::STANDARD;
static WRITE_FLOAT_OPTS: WriteFloatOptions = WriteFloatOptions::builder()
    .trim_floats(true)
    .inf_string(Some(b"Inf"))
    .nan_string(Some(b"NaN"))
    .build_unchecked();

/// A numeric type that can be written to `StringBuilder`.
#[allow(private_bounds)]
pub trait Numeric: ToLexical + private::Sealed {
    /// Formats the numeric value in the given buffer.
    ///
    /// Returns a range within the provided buffer that constitutes the formatted string.
    fn format<'buf>(&self, buf: &'buf mut [u8; lexical_core::BUFFER_SIZE]) -> &'buf str {
        let buf_len = {
            let num_buf = self.to_lexical(buf);
            num_buf.len()
        };

        // Reslice the original buffer to the length of the formatted number buffer, since `lexical_core` always writes
        // from the beginning of the buffer. This lets us derive our string reference from `buf` rather than the
        // local `num_buf`.
        //
        // SAFETY: `lexical_core::write` only generates valid UTF-8 output.
        unsafe { std::str::from_utf8_unchecked(&buf[..buf_len]) }
    }
}

impl Numeric for u8 {}
impl Numeric for u16 {}
impl Numeric for u32 {}
impl Numeric for u64 {}
impl Numeric for u128 {}
impl Numeric for usize {}
impl Numeric for i8 {}
impl Numeric for i16 {}
impl Numeric for i32 {}
impl Numeric for i64 {}
impl Numeric for i128 {}
impl Numeric for isize {}

impl Numeric for f32 {
    fn format<'buf>(&self, buf: &'buf mut [u8; lexical_core::BUFFER_SIZE]) -> &'buf str {
        let buf_len = {
            let num_buf = self.to_lexical_with_options::<FLOAT_FORMAT>(buf, &WRITE_FLOAT_OPTS);
            num_buf.len()
        };

        // Reslice the original buffer to the length of the formatted number buffer, since `lexical_core` always writes
        // from the beginning of the buffer. This lets us derive our string reference from `buf` rather than the
        // local `num_buf`.
        //
        // SAFETY: `lexical_core::write` only generates valid UTF-8 output.
        unsafe { std::str::from_utf8_unchecked(&buf[..buf_len]) }
    }
}

impl Numeric for f64 {
    fn format<'buf>(&self, buf: &'buf mut [u8; lexical_core::BUFFER_SIZE]) -> &'buf str {
        let buf_len = {
            let num_buf = self.to_lexical_with_options::<FLOAT_FORMAT>(buf, &WRITE_FLOAT_OPTS);
            num_buf.len()
        };

        // Reslice the original buffer to the length of the formatted number buffer, since `lexical_core` always writes
        // from the beginning of the buffer. This lets us derive our string reference from `buf` rather than the
        // local `num_buf`.
        //
        // SAFETY: `lexical_core::write` only generates valid UTF-8 output.
        unsafe { std::str::from_utf8_unchecked(&buf[..buf_len]) }
    }
}

mod private {
    pub(super) trait Sealed {}

    impl Sealed for u8 {}
    impl Sealed for u16 {}
    impl Sealed for u32 {}
    impl Sealed for u64 {}
    impl Sealed for u128 {}
    impl Sealed for usize {}
    impl Sealed for i8 {}
    impl Sealed for i16 {}
    impl Sealed for i32 {}
    impl Sealed for i64 {}
    impl Sealed for i128 {}
    impl Sealed for isize {}
    impl Sealed for f32 {}
    impl Sealed for f64 {}
}

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
    num_buf: [u8; lexical_core::BUFFER_SIZE],
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
            num_buf: [0; lexical_core::BUFFER_SIZE],
        }
    }

    /// Creates a new `StringBuilder` with the given limit.
    ///
    /// Strings that exceed the limit will be discarded.
    pub fn with_limit(limit: usize) -> Self {
        Self {
            buf: String::new(),
            limit,
            interner: (),
            num_buf: [0; lexical_core::BUFFER_SIZE],
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
            num_buf: self.num_buf,
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

    /// Pushes a character into the builder.
    ///
    /// Returns `None` if the resulting string would exceed the configured limit.
    pub fn push(&mut self, c: char) -> Option<()> {
        if self.buf.len() + 1 > self.limit {
            return None;
        }
        self.buf.push(c);
        Some(())
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

    /// Pushes a numeric value into the builder.
    ///
    /// This method supports all signed and unsigned integer types, as well as single- and double-precision
    /// floating-point numbers.
    ///
    /// Returns `None` if the resulting string would exceed the configured limit.
    pub fn push_numeric<N: Numeric>(&mut self, value: N) -> Option<()> {
        let num_str = value.format(&mut self.num_buf);
        if self.buf.len() + num_str.len() > self.limit {
            return None;
        }
        self.buf.push_str(num_str);
        Some(())
    }

    /// Returns a references to the current string.
    pub fn string(&self) -> &str {
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
        let interned = self.interner.try_intern(self.string());
        self.clear();

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
        assert_eq!(builder.string(), "Hello World!");

        builder.clear();

        assert_eq!(builder.push_str("hello"), Some(()));
        assert_eq!(builder.push_str(" "), Some(()));
        assert_eq!(builder.push_str("world"), Some(()));
        assert_eq!(builder.string(), "hello world");
    }

    #[test]
    fn string_builder_basic_with_interner() {
        let mut builder = build_interned_string_builder(128);

        assert_eq!(builder.push_str("Hello World!"), Some(()));
        assert_eq!(builder.try_intern(), Some(MetaString::from("Hello World!")));

        assert_eq!(builder.push_str("hello"), Some(()));
        assert_eq!(builder.push_str(" "), Some(()));
        assert_eq!(builder.push_str("world"), Some(()));
        assert_eq!(builder.try_intern(), Some(MetaString::from("hello world")));
    }

    #[test]
    fn string_builder_numerics() {
        let mut builder = build_string_builder();

        assert_eq!(builder.push_numeric(1u8), Some(()));
        assert_eq!(builder.string(), "1");
        assert_eq!(builder.push_numeric(2u16), Some(()));
        assert_eq!(builder.string(), "12");
        assert_eq!(builder.push_numeric(3u32), Some(()));
        assert_eq!(builder.string(), "123");
        assert_eq!(builder.push_numeric(4u64), Some(()));
        assert_eq!(builder.string(), "1234");
        assert_eq!(builder.push_numeric(5u128), Some(()));
        assert_eq!(builder.string(), "12345");
        assert_eq!(builder.push_numeric(6usize), Some(()));
        assert_eq!(builder.string(), "123456");

        builder.clear();

        assert_eq!(builder.push_numeric(-1i8), Some(()));
        assert_eq!(builder.string(), "-1");
        assert_eq!(builder.push_numeric(-2i16), Some(()));
        assert_eq!(builder.string(), "-1-2");
        assert_eq!(builder.push_numeric(-3i32), Some(()));
        assert_eq!(builder.string(), "-1-2-3");
        assert_eq!(builder.push_numeric(-4i64), Some(()));
        assert_eq!(builder.string(), "-1-2-3-4");
        assert_eq!(builder.push_numeric(-5i128), Some(()));
        assert_eq!(builder.string(), "-1-2-3-4-5");
        assert_eq!(builder.push_numeric(-6isize), Some(()));
        assert_eq!(builder.string(), "-1-2-3-4-5-6");

        builder.clear();

        assert_eq!(builder.push_numeric(0.0f32), Some(()));
        assert_eq!(builder.string(), "0");
        assert_eq!(builder.push_numeric(1.0f32), Some(()));
        assert_eq!(builder.string(), "01");
        assert_eq!(builder.push_numeric(-2.0f32), Some(()));
        assert_eq!(builder.string(), "01-2");
        assert_eq!(builder.push_numeric(3.5f32), Some(()));
        assert_eq!(builder.string(), "01-23.5");

        builder.clear();

        assert_eq!(builder.push_numeric(0.0f64), Some(()));
        assert_eq!(builder.string(), "0");
        assert_eq!(builder.push_numeric(1.0f64), Some(()));
        assert_eq!(builder.string(), "01");
        assert_eq!(builder.push_numeric(-2.0f64), Some(()));
        assert_eq!(builder.string(), "01-2");
        assert_eq!(builder.push_numeric(3.5f64), Some(()));
        assert_eq!(builder.string(), "01-23.5");
    }

    #[test]
    fn string_builder_clear() {
        let mut builder = build_string_builder();

        assert_eq!(builder.push_str("hello"), Some(()));
        builder.clear();
        assert_eq!(builder.string(), "");
    }

    #[test]
    fn string_builder_is_empty_len() {
        let mut builder = build_string_builder();

        // Starts out empty:
        assert!(builder.is_empty());
        assert_eq!(builder.len(), 0);

        // After pushing "hello":
        assert_eq!(builder.push_str("hello"), Some(()));
        assert!(!builder.is_empty());
        assert_eq!(builder.len(), 5);
        assert_eq!(builder.string(), "hello");

        // After pushing " world":
        builder.push_str(" world");
        assert!(!builder.is_empty());
        assert_eq!(builder.len(), 11);
        assert_eq!(builder.string(), "hello world");

        // Manually clearing the buffer:
        builder.clear();
        assert!(builder.is_empty());
        assert_eq!(builder.len(), 0);
    }

    #[test]
    fn string_builder_with_limit() {
        const LIMIT: usize = 16;

        let mut builder = build_string_builder_with_limit(LIMIT);

        // Under the limit:
        let string_one = "hello, world!";
        assert!(string_one.len() < LIMIT);
        assert_eq!(builder.push_str(string_one), Some(()));
        assert_eq!(builder.string(), string_one);

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
        assert_eq!(builder.string(), string_three);
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
}
