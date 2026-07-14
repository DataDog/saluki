/// A log template skeleton parser.
///
/// This parser extracts a template "skeleton" from an input log message, essentially tokenizing the message which in
/// turns allows for storing the template and variables separately. This is designed to facilitate efficient storage of
/// duplicate instances of a log message while factoring out the static and dynamic components.
#[derive(Default)]
pub struct SkeletonParser {
    buf: String,
}

impl SkeletonParser {
    /// Extracts a template skeleton from a log message.
    ///
    /// Returns `(skeleton, var_count)` where `skeleton` is a borrowed view of the internal buffer
    /// with `\x00` placeholders in place of variable tokens, and `var_count` is how many variables
    /// were found.
    pub fn extract(&mut self, message: &str) -> (&str, usize) {
        self.buf.clear();
        let mut var_count: usize = 0;
        let mut first = true;
        for token in message.split_ascii_whitespace() {
            if !first {
                self.buf.push(' ');
            }
            first = false;
            if is_variable_token(token) {
                self.buf.push('\x00');
                var_count += 1;
            } else {
                self.buf.push_str(token);
            }
        }
        (&self.buf, var_count)
    }

    /// Returns an iterator over variable tokens in a message (tokens containing ASCII digits).
    pub fn variable_tokens<'a>(&self, message: &'a str) -> impl Iterator<Item = &'a str> {
        message
            .split_ascii_whitespace()
            .filter(|token| is_variable_token(token))
    }
}

/// Returns `true` if a whitespace-delimited token should be treated as a variable.
fn is_variable_token(token: &str) -> bool {
    token.bytes().any(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_message() {
        let mut parser = SkeletonParser::default();
        let (skeleton, var_count) = parser.extract("");
        assert_eq!(skeleton, "");
        assert_eq!(var_count, 0);
        assert_eq!(parser.variable_tokens("").count(), 0);
    }

    #[test]
    fn all_static_tokens() {
        let mut parser = SkeletonParser::default();
        let (skeleton, var_count) = parser.extract("hello world foo");
        assert_eq!(skeleton, "hello world foo");
        assert_eq!(var_count, 0);
        assert_eq!(parser.variable_tokens("hello world foo").count(), 0);
    }

    #[test]
    fn all_variable_tokens() {
        let mut parser = SkeletonParser::default();
        let (skeleton, var_count) = parser.extract("123 456 789");
        assert_eq!(skeleton, "\x00 \x00 \x00");
        assert_eq!(var_count, 3);
        assert_eq!(
            parser.variable_tokens("123 456 789").collect::<Vec<_>>(),
            vec!["123", "456", "789"]
        );
    }

    #[test]
    fn mixed_static_and_variable() {
        let mut parser = SkeletonParser::default();
        let (skeleton, var_count) = parser.extract("Processed 1234 events in 56ms");
        assert_eq!(skeleton, "Processed \x00 events in \x00");
        assert_eq!(var_count, 2);
        assert_eq!(
            parser
                .variable_tokens("Processed 1234 events in 56ms")
                .collect::<Vec<_>>(),
            vec!["1234", "56ms"]
        );
    }

    #[test]
    fn single_static_token() {
        let mut parser = SkeletonParser::default();
        let (skeleton, var_count) = parser.extract("hello");
        assert_eq!(skeleton, "hello");
        assert_eq!(var_count, 0);
    }

    #[test]
    fn single_variable_token() {
        let mut parser = SkeletonParser::default();
        let (skeleton, var_count) = parser.extract("42");
        assert_eq!(skeleton, "\x00");
        assert_eq!(var_count, 1);
        assert_eq!(parser.variable_tokens("42").collect::<Vec<_>>(), vec!["42"]);
    }

    #[test]
    fn whitespace_normalization() {
        let mut parser = SkeletonParser::default();
        // Extra whitespace, tabs, leading/trailing whitespace are all collapsed by
        // split_ascii_whitespace.
        let (skeleton, var_count) = parser.extract("  hello   world  ");
        assert_eq!(skeleton, "hello world");
        assert_eq!(var_count, 0);
    }

    #[test]
    fn digits_embedded_in_word() {
        let mut parser = SkeletonParser::default();
        // Tokens like "v2", "x86_64", "127.0.0.1" contain digits → treated as variables.
        let (skeleton, var_count) = parser.extract("connecting to v2 at 127.0.0.1");
        assert_eq!(skeleton, "connecting to \x00 at \x00");
        assert_eq!(var_count, 2);
    }

    #[test]
    fn buffer_reuse_across_calls() {
        let mut parser = SkeletonParser::default();

        let (s1, c1) = parser.extract("hello 123");
        assert_eq!(s1, "hello \x00");
        assert_eq!(c1, 1);

        // Second call reuses the buffer (previous content is cleared).
        let (s2, c2) = parser.extract("world");
        assert_eq!(s2, "world");
        assert_eq!(c2, 0);
    }

    #[test]
    fn variable_tokens_independent_of_extract() {
        // variable_tokens does not depend on extract having been called first.
        let parser = SkeletonParser::default();
        let vars: Vec<_> = parser.variable_tokens("sent 500 bytes in 12ms ok").collect();
        assert_eq!(vars, vec!["500", "12ms"]);
    }
}
