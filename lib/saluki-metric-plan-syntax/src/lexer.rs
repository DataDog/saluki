//! The lexer: source text to tokens.
//!
//! Every word is an identifier token, which the parser gives meaning from its position, so the grammar has no reserved
//! words: a node can be named `emit` and a parameter `true`.

use saluki_metric_plan::limits::values::ARTIFACT_SIZE;
use serde_json::Number;

use crate::diagnostic::{Error, Span};

/// The most tokens a source may have: one per byte of the largest artifact.
///
/// Printed text has fewer tokens than the artifact's JSON has bytes, since every token stands for at least one byte of
/// JSON and node IDs, member names, and quotes take more, so the printed text of any artifact within the size limit
/// stays under this limit.
pub(crate) const MAX_TOKENS: usize = ARTIFACT_SIZE as usize;

/// The units a duration literal may have.
pub(crate) const DURATION_UNITS: [&str; 6] = ["h", "m", "s", "ms", "us", "ns"];

/// What a token is.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TokenKind {
    /// A word of ASCII letters, digits, and underscores, starting with a letter or underscore.
    Ident,
    /// A string literal, with its escapes decoded.
    Str(String),
    /// A number.
    Number(Number),
    /// A duration literal, such as `10s`, kept as written.
    Duration,
    /// `->`
    Arrow,
    /// `==`
    EqEq,
    /// `=`
    Eq,
    /// `:`
    Colon,
    /// `,`
    Comma,
    /// `.`
    Dot,
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `{`
    LBrace,
    /// `}`
    RBrace,
    /// `[`
    LBracket,
    /// `]`
    RBracket,
    /// The end of the source.
    Eof,
}

/// One token, with its span in the source.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Token {
    pub(crate) kind: TokenKind,
    pub(crate) span: Span,
}

/// Splits `source` into tokens, ending with [`TokenKind::Eof`].
///
/// # Errors
///
/// Returns an error at the first character that does not start a token, the first malformed literal, or the first
/// token past [`MAX_TOKENS`].
pub(crate) fn lex(source: &str) -> Result<Vec<Token>, Error> {
    let mut lexer = Lexer { source, pos: 0 };
    let mut tokens = Vec::new();
    loop {
        let token = lexer.next_token()?;
        let done = token.kind == TokenKind::Eof;
        if !done && tokens.len() == MAX_TOKENS {
            return Err(Error::new(
                token.span,
                format!(
                    "The source has more than {} tokens, the most a policy may have: the text of an artifact within \
                     the artifact size limit has fewer.",
                    MAX_TOKENS
                ),
            ));
        }
        tokens.push(token);
        if done {
            return Ok(tokens);
        }
    }
}

struct Lexer<'s> {
    source: &'s str,
    pos: usize,
}

impl Lexer<'_> {
    fn peek(&self) -> Option<char> {
        self.source[self.pos..].chars().next()
    }

    fn peek_byte_at(&self, offset: usize) -> Option<u8> {
        self.source.as_bytes().get(self.pos + offset).copied()
    }

    fn token(&self, kind: TokenKind, start: usize) -> Token {
        Token {
            kind,
            span: Span::new(start, self.pos),
        }
    }

    fn skip_trivia(&mut self) -> Result<(), Error> {
        while let Some(c) = self.peek() {
            match c {
                ' ' | '\t' | '\n' | '\r' => self.pos += 1,
                '#' => {
                    let end = self.source[self.pos..]
                        .find('\n')
                        .map_or(self.source.len(), |newline| self.pos + newline);
                    // A comment may say anything, but must not hide what the text around it says.
                    if let Some((offset, c)) = self.source[self.pos..end]
                        .char_indices()
                        .find(|(_, c)| is_invisible(*c))
                    {
                        let at = self.pos + offset;
                        return Err(Error::new(
                            Span::new(at, at + c.len_utf8()),
                            format!(
                                "A comment cannot contain the invisible formatting character {}, which changes how \
                                 the text around it displays.",
                                describe_char(c)
                            ),
                        ));
                    }
                    self.pos = end;
                }
                _ => break,
            }
        }
        Ok(())
    }

    fn next_token(&mut self) -> Result<Token, Error> {
        self.skip_trivia()?;
        let start = self.pos;
        let Some(c) = self.peek() else {
            return Ok(self.token(TokenKind::Eof, start));
        };

        let single = |kind| (kind, 1);
        let (kind, len) = match c {
            '(' => single(TokenKind::LParen),
            ')' => single(TokenKind::RParen),
            '{' => single(TokenKind::LBrace),
            '}' => single(TokenKind::RBrace),
            '[' => single(TokenKind::LBracket),
            ']' => single(TokenKind::RBracket),
            ',' => single(TokenKind::Comma),
            ':' => single(TokenKind::Colon),
            '.' => single(TokenKind::Dot),
            '=' if self.peek_byte_at(1) == Some(b'=') => (TokenKind::EqEq, 2),
            '=' => single(TokenKind::Eq),
            '-' if self.peek_byte_at(1) == Some(b'>') => (TokenKind::Arrow, 2),
            '"' => return self.string(start),
            '-' | '0'..='9' if c != '-' || self.peek_byte_at(1).is_some_and(|b| b.is_ascii_digit()) => {
                return self.number(start)
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                self.pos += self.word_len();
                return Ok(self.token(TokenKind::Ident, start));
            }
            c => {
                let hint = match c {
                    '-' => " Identifiers use underscores, not hyphens; `->` is the only operator that starts with `-`.",
                    '\'' => " Strings are written in double quotes.",
                    '!' => " Negate a predicate with `not`, as in `not name == \"a\"`; there is no `!=`.",
                    '&' => " Combine predicates with `and`.",
                    '|' => " Combine predicates with `or`.",
                    '>' => " Edges are written `->`.",
                    _ => "",
                };
                return Err(Error::new(
                    Span::new(start, start + c.len_utf8()),
                    format!("Unexpected character {}.{}", describe_char(c), hint),
                ));
            }
        };
        self.pos += len;
        Ok(self.token(kind, start))
    }

    /// Returns the length of the run of identifier characters at the current position.
    fn word_len(&self) -> usize {
        self.source.as_bytes()[self.pos..]
            .iter()
            .take_while(|b| b.is_ascii_alphanumeric() || **b == b'_')
            .count()
    }

    fn number(&mut self, start: usize) -> Result<Token, Error> {
        let negative = self.peek() == Some('-');
        if negative {
            self.pos += 1;
        }
        let digits_start = self.pos;
        while self.peek_byte_at(0).is_some_and(|b| b.is_ascii_digit()) {
            self.pos += 1;
        }
        let digits = &self.source[digits_start..self.pos];

        // A duration: digits followed directly by a unit, such as `10s`.
        if self
            .peek_byte_at(0)
            .is_some_and(|b| b.is_ascii_alphabetic() && b != b'e' && b != b'E')
            || (self.peek_byte_at(0).is_some_and(|b| b == b'e' || b == b'E') && !self.exponent_follows())
        {
            let unit_start = self.pos;
            self.pos += self.word_len();
            let unit = &self.source[unit_start..self.pos];
            let span = Span::new(start, self.pos);
            if negative {
                return Err(Error::new(span, "A duration cannot be negative."));
            }
            if !DURATION_UNITS.contains(&unit) {
                return Err(Error::new(
                    span,
                    format!(
                        "Invalid duration `{}`: write an integer followed by one of the units h, m, s, ms, us, or ns, \
                         such as `10s`.",
                        &self.source[start..self.pos]
                    ),
                ));
            }
            return Ok(self.token(TokenKind::Duration, start));
        }

        let mut is_float = false;
        if self.peek_byte_at(0) == Some(b'.') && self.peek_byte_at(1).is_some_and(|b| b.is_ascii_digit()) {
            is_float = true;
            self.pos += 1;
            while self.peek_byte_at(0).is_some_and(|b| b.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        if self.peek_byte_at(0).is_some_and(|b| b == b'e' || b == b'E') && self.exponent_follows() {
            is_float = true;
            self.pos += 1;
            if self.peek_byte_at(0).is_some_and(|b| b == b'+' || b == b'-') {
                self.pos += 1;
            }
            while self.peek_byte_at(0).is_some_and(|b| b.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        let span = Span::new(start, self.pos);
        let text = &self.source[start..self.pos];
        if digits.len() > 1 && digits.starts_with('0') {
            return Err(Error::new(
                span,
                format!("Invalid number `{}`: numbers are written without leading zeros.", text),
            ));
        }
        if self
            .peek_byte_at(0)
            .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            let end = self.pos + self.word_len();
            let written = &self.source[start..end];
            let message = if DURATION_UNITS.contains(&&self.source[self.pos..end]) {
                format!(
                    "Invalid duration `{}`: a duration is a whole number followed by a unit, such as `1500ms`.",
                    written
                )
            } else {
                format!("Invalid number `{}`.", written)
            };
            return Err(Error::new(Span::new(start, end), message));
        }

        // Fractions are rounded correctly, so the shortest text of a number, which printing writes, reads back as the
        // same number.
        let number = if is_float {
            text.parse::<f64>()
                .ok()
                .filter(|number| number.is_finite())
                .and_then(Number::from_f64)
        } else if negative {
            text.parse::<i64>().ok().map(Number::from)
        } else {
            text.parse::<u64>().ok().map(Number::from)
        };
        match number {
            Some(number) => Ok(self.token(TokenKind::Number(number), start)),
            None => Err(Error::new(span, format!("The number `{}` is out of range.", text))),
        }
    }

    /// Returns `true` if the `e` or `E` at the current position starts an exponent: it is followed by a digit, or by a
    /// sign and a digit.
    fn exponent_follows(&self) -> bool {
        match self.peek_byte_at(1) {
            Some(b'+' | b'-') => self.peek_byte_at(2).is_some_and(|b| b.is_ascii_digit()),
            Some(b) => b.is_ascii_digit(),
            None => false,
        }
    }

    fn string(&mut self, start: usize) -> Result<Token, Error> {
        // Skip the opening quote.
        self.pos += 1;
        let mut value = String::new();
        loop {
            let Some(c) = self.peek() else {
                return Err(Error::new(
                    Span::new(start, self.pos),
                    "Unterminated string: add a closing `\"`.",
                ));
            };
            let at = self.pos;
            self.pos += c.len_utf8();
            match c {
                '"' => return Ok(self.token(TokenKind::Str(value), start)),
                '\\' => value.push(self.escape(at)?),
                '\n' | '\r' => {
                    return Err(Error::new(
                        Span::new(start, at),
                        "Unterminated string: a string cannot span lines; add a closing `\"`, or write a line break \
                         as `\\n`.",
                    ))
                }
                c if c.is_control() || is_invisible(c) => {
                    return Err(Error::new(
                        Span::new(at, self.pos),
                        format!(
                            "A string cannot contain the {} character {} as is; write it as `\\u{{{:x}}}`.",
                            if c.is_control() {
                                "control"
                            } else {
                                "invisible formatting"
                            },
                            describe_char(c),
                            c as u32
                        ),
                    ))
                }
                c => value.push(c),
            }
        }
    }

    /// Decodes the escape sequence whose backslash is at `at`; the current position is just past the backslash.
    fn escape(&mut self, at: usize) -> Result<char, Error> {
        let Some(c) = self.peek() else {
            return Err(Error::new(
                Span::new(at, self.pos),
                "Unterminated string: add a closing `\"`.",
            ));
        };
        self.pos += c.len_utf8();
        let decoded = match c {
            '"' => '"',
            '\\' => '\\',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            'u' => return self.unicode_escape(at),
            other => {
                return Err(Error::new(
                    Span::new(at, self.pos),
                    format!(
                        "Unknown escape sequence `\\{}` in a string. The escapes are `\\\"`, `\\\\`, `\\n`, `\\r`, \
                         `\\t`, and `\\u{{...}}` with one to six hexadecimal digits.",
                        other.escape_debug()
                    ),
                ))
            }
        };
        Ok(decoded)
    }

    fn unicode_escape(&mut self, at: usize) -> Result<char, Error> {
        let invalid = |end: usize| {
            Error::new(
                Span::new(at, end),
                "Invalid escape sequence: a Unicode escape is `\\u{...}` with one to six hexadecimal digits naming a \
                 Unicode scalar value, such as `\\u{1f600}`.",
            )
        };
        if self.peek() != Some('{') {
            return Err(invalid(self.pos));
        }
        self.pos += 1;
        let digits_start = self.pos;
        while self.peek_byte_at(0).is_some_and(|b| b.is_ascii_hexdigit()) {
            self.pos += 1;
        }
        let digits = &self.source[digits_start..self.pos];
        if self.peek() != Some('}') || digits.is_empty() || digits.len() > 6 {
            return Err(invalid(self.pos));
        }
        self.pos += 1;
        u32::from_str_radix(digits, 16)
            .ok()
            .and_then(char::from_u32)
            .ok_or_else(|| invalid(self.pos))
    }
}

/// Describes a character for a message: in backticks, or by code point if it does not print.
fn describe_char(c: char) -> String {
    if c.is_control() || c.is_whitespace() || is_invisible(c) {
        format!("U+{:04X}", c as u32)
    } else {
        format!("`{}`", c)
    }
}

/// Returns `true` for the formatting characters that change how text around them displays without showing
/// themselves: bidirectional controls, and zero-width characters.
pub(crate) fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{200B}'..='\u{200F}' | '\u{2028}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}' | '\u{00AD}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(source: &str) -> Vec<TokenKind> {
        lex(source).unwrap().into_iter().map(|token| token.kind).collect()
    }

    #[test]
    fn tokens_of_every_kind() {
        let source = "node a_1 = op(x: \"s\", d: 10s, n: -3, f: 1.5e3) { -> b.0 } == [] # comment\n";
        assert_eq!(
            kinds(source),
            vec![
                TokenKind::Ident,
                TokenKind::Ident,
                TokenKind::Eq,
                TokenKind::Ident,
                TokenKind::LParen,
                TokenKind::Ident,
                TokenKind::Colon,
                TokenKind::Str("s".to_string()),
                TokenKind::Comma,
                TokenKind::Ident,
                TokenKind::Colon,
                TokenKind::Duration,
                TokenKind::Comma,
                TokenKind::Ident,
                TokenKind::Colon,
                TokenKind::Number(Number::from(-3)),
                TokenKind::Comma,
                TokenKind::Ident,
                TokenKind::Colon,
                TokenKind::Number(Number::from_f64(1500.0).unwrap()),
                TokenKind::RParen,
                TokenKind::LBrace,
                TokenKind::Arrow,
                TokenKind::Ident,
                TokenKind::Dot,
                TokenKind::Number(Number::from(0)),
                TokenKind::RBrace,
                TokenKind::EqEq,
                TokenKind::LBracket,
                TokenKind::RBracket,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn string_escapes_decode() {
        let tokens = lex(r#""q\" b\\ n\n r\r t\t u\u{e9}\u{1F600}""#).unwrap();
        assert_eq!(
            tokens[0].kind,
            TokenKind::Str("q\" b\\ n\n r\r t\t u\u{e9}\u{1F600}".to_string())
        );
    }

    #[test]
    fn durations_keep_their_spelling() {
        let source = "0s 10s 500ms 2m 1h 7us 9ns 060s";
        let tokens = lex(source).unwrap();
        let texts = tokens
            .iter()
            .filter(|token| token.kind == TokenKind::Duration)
            .map(|token| &source[token.span.range()])
            .collect::<Vec<_>>();
        assert_eq!(texts, ["0s", "10s", "500ms", "2m", "1h", "7us", "9ns", "060s"]);
    }

    #[test]
    fn malformed_tokens_are_reported_at_the_offending_text() {
        // (source, offending text, message fragment)
        let cases = [
            ("\"abc", "\"abc", "Unterminated string"),
            ("\"ab\ncd\"", "\"ab", "cannot span lines"),
            (r#""a\qb""#, r"\q", "Unknown escape sequence `\\q`"),
            (r#""\u{110000}""#, r"\u{110000}", "Unicode escape"),
            ("\"\\u00e9\"", "\\u", "Unicode escape"),
            ("\"a\u{7}\"", "\u{7}", "control character U+0007"),
            ("\"a\u{202E}b\"", "\u{202E}", "invisible formatting character U+202E"),
            ("# a \u{2066} b\nx", "\u{2066}", "invisible formatting character U+2066"),
            ("10x", "10x", "Invalid duration `10x`"),
            ("-5s", "-5s", "cannot be negative"),
            ("007", "007", "leading zeros"),
            ("12ab3", "12ab3", "Invalid duration"),
            ("1.5x", "1.5x", "Invalid number `1.5x`."),
            (
                "1.5s",
                "1.5s",
                "Invalid duration `1.5s`: a duration is a whole number followed by a unit",
            ),
            (
                "name != \"a\"",
                "!",
                "Unexpected character `!`. Negate a predicate with `not`",
            ),
            ("a && b", "&", "Combine predicates with `and`."),
            ("a || b", "|", "Combine predicates with `or`."),
            (
                "input.metrics => s",
                ">",
                "Unexpected character `>`. Edges are written `->`.",
            ),
            ("99999999999999999999", "99999999999999999999", "out of range"),
            ("-9223372036854775809", "-9223372036854775809", "out of range"),
            ("1e999", "1e999", "out of range"),
            ("a @ b", "@", "Unexpected character `@`"),
            ("my-node", "-", "underscores"),
            ("'s'", "'", "double quotes"),
            ("x\u{202E}", "\u{202E}", "U+202E"),
        ];
        for (source, offending, message) in cases {
            let error = lex(source).unwrap_err();
            assert_eq!(&source[error.span.range()], offending, "{:?}", source);
            assert!(error.message.contains(message), "{:?}: {}", source, error.message);
        }
    }

    #[test]
    fn integers_cover_the_whole_signed_and_unsigned_range() {
        assert_eq!(
            kinds("18446744073709551615 -9223372036854775808"),
            vec![
                TokenKind::Number(Number::from(u64::MAX)),
                TokenKind::Number(Number::from(i64::MIN)),
                TokenKind::Eof
            ]
        );
    }

    #[test]
    fn token_count_is_bounded() {
        let at_limit = "a ".repeat(MAX_TOKENS);
        assert_eq!(lex(&at_limit).unwrap().len(), MAX_TOKENS + 1);

        let over_limit = "a ".repeat(MAX_TOKENS + 1);
        let error = lex(&over_limit).unwrap_err();
        assert_eq!(error.span, Span::new(2 * MAX_TOKENS, 2 * MAX_TOKENS + 1));
        assert!(error.message.contains("more than 262144 tokens"), "{}", error.message);
    }

    #[test]
    fn fractions_read_back_as_the_number_printing_writes() {
        // Printing writes the shortest text of a number. Read without correct rounding, the first of these drifts by
        // one unit in the last place on every round trip.
        let cases = [
            "1.0715660391465826e-75",
            "0.1",
            "2.5e-308",
            "1.7976931348623157e308",
            "-0.0",
            "5e-324",
        ];
        for text in cases {
            let expected = text.parse::<f64>().unwrap();
            let number = match &lex(text).unwrap()[0].kind {
                TokenKind::Number(number) => number.clone(),
                other => panic!("{} lexes as {:?}", text, other),
            };
            assert_eq!(number.as_f64().map(f64::to_bits), Some(expected.to_bits()), "{}", text);
            let printed = number.to_string();
            assert_eq!(lex(&printed).unwrap()[0].kind, TokenKind::Number(number), "{}", printed);
        }
    }
}
