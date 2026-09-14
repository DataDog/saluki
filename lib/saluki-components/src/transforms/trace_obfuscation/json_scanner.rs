//! JSON scanning state machine.
//!
//! The scanner is fed one character at a time and answers with an [`Op`] describing what just
//! happened, which lets a caller walk a JSON document without building a value tree. Unlike a
//! parser it never rejects trailing content: once a top-level value ends, anything other than
//! whitespace starts a new one, because Elasticsearch bodies hold several documents in a row.
//!
//! This is a port of the Datadog Agent's `pkg/obfuscate/json_scanner.go`, itself a copy of Go's
//! `encoding/json` scanner. Ops and states are kept in one-to-one correspondence with that
//! code so the two can be diffed.

/// What the scanner saw when it consumed a character.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// An uninteresting character, such as one inside a literal.
    Continue,
    /// A literal starts here: a string, number, `true`, `false` or `null`.
    BeginLiteral,
    /// An object starts here.
    BeginObject,
    /// An object key ended before this character.
    ObjectKey,
    /// An object value ended before this character, and another key follows.
    ObjectValue,
    /// An object ends here.
    EndObject,
    /// An array starts here.
    BeginArray,
    /// An array value ended before this character, and another value follows.
    ArrayValue,
    /// An array ends here.
    EndArray,
    /// Whitespace between two tokens.
    SkipSpace,
    /// The top-level value ended before this character.
    End,
    /// A syntax error, described by [`Scanner::err`].
    Error,
}

/// Where the scanner is within the composite value it is reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParseState {
    ObjectKey,
    ObjectValue,
    ArrayValue,
}

/// The transition to apply to the next character.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    BeginValue,
    /// After `[`, where `]` is still allowed.
    BeginValueOrEmpty,
    /// After `{`, where `}` is still allowed.
    BeginStringOrEmpty,
    BeginString,
    EndValue,
    EndTop,
    InString,
    InStringEsc,
    /// After `\u`, holding the number of hex digits read so far.
    InStringEscHex(u8),
    Neg,
    /// After a leading `0`.
    Zero,
    /// After a leading non-zero digit.
    NonZero,
    Dot,
    DotDigits,
    Exp,
    ExpSign,
    ExpDigits,
    LitT,
    LitTr,
    LitTru,
    LitF,
    LitFa,
    LitFal,
    LitFals,
    LitN,
    LitNu,
    LitNul,
    Error,
}

/// A JSON scanning state machine.
pub struct Scanner {
    state: State,
    parse_states: Vec<ParseState>,
    end_top: bool,
    position: usize,

    /// Describes the syntax error that stopped the scan, if one did.
    pub err: Option<String>,
}

impl Scanner {
    /// Creates a scanner ready to read the beginning of a document.
    pub fn new() -> Self {
        Self {
            state: State::BeginValue,
            parse_states: Vec::new(),
            end_top: false,
            position: 0,
            err: None,
        }
    }

    /// Feeds one character to the scanner.
    pub fn step(&mut self, c: char) -> Op {
        self.position += 1;
        self.transition(c)
    }

    /// Tells the scanner that the input ended.
    ///
    /// Returns [`Op::End`] for a complete document and [`Op::Error`] for one that stops early, such
    /// as an object that is never closed.
    pub fn eof(&mut self) -> Op {
        if self.err.is_some() {
            return Op::Error;
        }
        if self.end_top {
            return Op::End;
        }

        // A trailing space closes any value that only ends by running out of characters, such as a
        // number.
        self.transition(' ');
        if self.end_top {
            return Op::End;
        }

        if self.err.is_none() {
            self.err = Some(format!("unexpected end of JSON input at position {}", self.position));
        }
        Op::Error
    }

    fn transition(&mut self, c: char) -> Op {
        match self.state {
            State::BeginValue => self.begin_value(c),
            State::BeginValueOrEmpty => {
                if is_space(c) {
                    Op::SkipSpace
                } else if c == ']' {
                    self.end_value(c)
                } else {
                    self.begin_value(c)
                }
            }
            State::BeginStringOrEmpty => {
                if is_space(c) {
                    Op::SkipSpace
                } else if c == '}' && !self.parse_states.is_empty() {
                    let last = self.parse_states.len() - 1;
                    self.parse_states[last] = ParseState::ObjectValue;
                    self.end_value(c)
                } else {
                    self.begin_string(c)
                }
            }
            State::BeginString => self.begin_string(c),
            State::EndValue => self.end_value(c),
            State::EndTop => self.end_top(c),
            State::InString => match c {
                '"' => {
                    self.state = State::EndValue;
                    Op::Continue
                }
                '\\' => {
                    self.state = State::InStringEsc;
                    Op::Continue
                }
                c if c < '\u{20}' => self.error(c, "in string literal"),
                _ => Op::Continue,
            },
            State::InStringEsc => match c {
                'b' | 'f' | 'n' | 'r' | 't' | '\\' | '/' | '"' => {
                    self.state = State::InString;
                    Op::Continue
                }
                'u' => {
                    self.state = State::InStringEscHex(0);
                    Op::Continue
                }
                _ => self.error(c, "in string escape code"),
            },
            State::InStringEscHex(read) => {
                if !c.is_ascii_hexdigit() {
                    return self.error(c, "in \\u hexadecimal character escape");
                }
                self.state = if read == 3 {
                    State::InString
                } else {
                    State::InStringEscHex(read + 1)
                };
                Op::Continue
            }
            State::Neg => match c {
                '0' => {
                    self.state = State::Zero;
                    Op::Continue
                }
                '1'..='9' => {
                    self.state = State::NonZero;
                    Op::Continue
                }
                _ => self.error(c, "in numeric literal"),
            },
            State::NonZero => {
                if c.is_ascii_digit() {
                    Op::Continue
                } else {
                    self.zero(c)
                }
            }
            State::Zero => self.zero(c),
            State::Dot => {
                if c.is_ascii_digit() {
                    self.state = State::DotDigits;
                    Op::Continue
                } else {
                    self.error(c, "after decimal point in numeric literal")
                }
            }
            State::DotDigits => match c {
                '0'..='9' => Op::Continue,
                'e' | 'E' => {
                    self.state = State::Exp;
                    Op::Continue
                }
                _ => self.end_value(c),
            },
            State::Exp => {
                if c == '+' || c == '-' {
                    self.state = State::ExpSign;
                    Op::Continue
                } else {
                    self.exp_sign(c)
                }
            }
            State::ExpSign => self.exp_sign(c),
            State::ExpDigits => {
                if c.is_ascii_digit() {
                    Op::Continue
                } else {
                    self.end_value(c)
                }
            }
            State::LitT => self.literal(c, 'r', State::LitTr, "in literal true (expecting 'r')"),
            State::LitTr => self.literal(c, 'u', State::LitTru, "in literal true (expecting 'u')"),
            State::LitTru => self.literal(c, 'e', State::EndValue, "in literal true (expecting 'e')"),
            State::LitF => self.literal(c, 'a', State::LitFa, "in literal false (expecting 'a')"),
            State::LitFa => self.literal(c, 'l', State::LitFal, "in literal false (expecting 'l')"),
            State::LitFal => self.literal(c, 's', State::LitFals, "in literal false (expecting 's')"),
            State::LitFals => self.literal(c, 'e', State::EndValue, "in literal false (expecting 'e')"),
            State::LitN => self.literal(c, 'u', State::LitNu, "in literal null (expecting 'u')"),
            State::LitNu => self.literal(c, 'l', State::LitNul, "in literal null (expecting 'l')"),
            State::LitNul => self.literal(c, 'l', State::EndValue, "in literal null (expecting 'l')"),
            State::Error => Op::Error,
        }
    }

    fn begin_value(&mut self, c: char) -> Op {
        if is_space(c) {
            return Op::SkipSpace;
        }

        match c {
            '{' => {
                self.state = State::BeginStringOrEmpty;
                self.parse_states.push(ParseState::ObjectKey);
                Op::BeginObject
            }
            '[' => {
                self.state = State::BeginValueOrEmpty;
                self.parse_states.push(ParseState::ArrayValue);
                Op::BeginArray
            }
            '"' => {
                self.state = State::InString;
                Op::BeginLiteral
            }
            '-' => {
                self.state = State::Neg;
                Op::BeginLiteral
            }
            '0' => {
                self.state = State::Zero;
                Op::BeginLiteral
            }
            '1'..='9' => {
                self.state = State::NonZero;
                Op::BeginLiteral
            }
            't' => {
                self.state = State::LitT;
                Op::BeginLiteral
            }
            'f' => {
                self.state = State::LitF;
                Op::BeginLiteral
            }
            'n' => {
                self.state = State::LitN;
                Op::BeginLiteral
            }
            _ => self.error(c, "looking for beginning of value"),
        }
    }

    fn begin_string(&mut self, c: char) -> Op {
        if is_space(c) {
            return Op::SkipSpace;
        }

        if c == '"' {
            self.state = State::InString;
            return Op::BeginLiteral;
        }

        self.error(c, "looking for beginning of object key string")
    }

    fn end_value(&mut self, c: char) -> Op {
        let Some(&parse_state) = self.parse_states.last() else {
            self.state = State::EndTop;
            self.end_top = true;
            return self.end_top(c);
        };

        if is_space(c) {
            self.state = State::EndValue;
            return Op::SkipSpace;
        }

        let last = self.parse_states.len() - 1;
        match parse_state {
            ParseState::ObjectKey => {
                if c == ':' {
                    self.parse_states[last] = ParseState::ObjectValue;
                    self.state = State::BeginValue;
                    return Op::ObjectKey;
                }
                self.error(c, "after object key")
            }
            ParseState::ObjectValue => {
                if c == ',' {
                    self.parse_states[last] = ParseState::ObjectKey;
                    self.state = State::BeginString;
                    return Op::ObjectValue;
                }
                if c == '}' {
                    self.pop_parse_state();
                    return Op::EndObject;
                }
                self.error(c, "after object key:value pair")
            }
            ParseState::ArrayValue => {
                if c == ',' {
                    self.state = State::BeginValue;
                    return Op::ArrayValue;
                }
                if c == ']' {
                    self.pop_parse_state();
                    return Op::EndArray;
                }
                self.error(c, "after array element")
            }
        }
    }

    /// Handles a character after the top-level value has ended.
    ///
    /// Anything other than whitespace is read as the start of another document rather than as an
    /// error, which is how a body holding several documents in a row keeps being obfuscated.
    fn end_top(&mut self, c: char) -> Op {
        if is_space(c) {
            return Op::End;
        }

        self.reset();
        self.transition(c)
    }

    fn zero(&mut self, c: char) -> Op {
        match c {
            '.' => {
                self.state = State::Dot;
                Op::Continue
            }
            'e' | 'E' => {
                self.state = State::Exp;
                Op::Continue
            }
            _ => self.end_value(c),
        }
    }

    fn exp_sign(&mut self, c: char) -> Op {
        if c.is_ascii_digit() {
            self.state = State::ExpDigits;
            return Op::Continue;
        }

        self.error(c, "in exponent of numeric literal")
    }

    /// Consumes the next expected character of a `true`, `false` or `null` literal.
    fn literal(&mut self, c: char, expected: char, next: State, context: &str) -> Op {
        if c == expected {
            self.state = next;
            return Op::Continue;
        }

        self.error(c, context)
    }

    fn pop_parse_state(&mut self) {
        self.parse_states.pop();
        if self.parse_states.is_empty() {
            self.state = State::EndTop;
            self.end_top = true;
        } else {
            self.state = State::EndValue;
        }
    }

    /// Prepares the scanner for another document. The position is left alone so that an error
    /// still reports where it happened in the whole input.
    fn reset(&mut self) {
        self.state = State::BeginValue;
        self.parse_states.clear();
        self.end_top = false;
        self.err = None;
    }

    fn error(&mut self, c: char, context: &str) -> Op {
        self.state = State::Error;
        self.err = Some(format!(
            "invalid character {:?} {} at position {}",
            c, context, self.position
        ));
        Op::Error
    }
}

fn is_space(c: char) -> bool {
    c == ' ' || c == '\t' || c == '\r' || c == '\n'
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scans a whole document, returning the ops in order plus the result of `eof`.
    fn scan(input: &str) -> (Vec<Op>, Op) {
        let mut scanner = Scanner::new();
        let mut ops = Vec::new();
        for c in input.chars() {
            let op = scanner.step(c);
            ops.push(op);
            if op == Op::Error {
                return (ops, Op::Error);
            }
        }
        let eof = scanner.eof();
        (ops, eof)
    }

    #[test]
    fn ops_for_object() {
        let (ops, eof) = scan(r#"{"a":1}"#);
        assert_eq!(
            ops,
            vec![
                Op::BeginObject,
                Op::BeginLiteral,
                Op::Continue,
                Op::Continue,
                Op::ObjectKey,
                Op::BeginLiteral,
                Op::EndObject,
            ]
        );
        assert_eq!(eof, Op::End);
    }

    #[test]
    fn whitespace_between_tokens_is_reported_as_skippable() {
        let (ops, eof) = scan("{ \"a\" :\t1 }");
        assert_eq!(ops.iter().filter(|op| **op == Op::SkipSpace).count(), 4);
        assert_eq!(eof, Op::End);
    }

    #[test]
    fn numbers_end_at_eof() {
        for input in ["1", "-1", "0.5", "1e10", "1E+10", "-0.5e-10"] {
            let (ops, eof) = scan(input);
            assert!(!ops.contains(&Op::Error), "unexpected error scanning {}", input);
            assert_eq!(eof, Op::End, "{} did not end cleanly", input);
        }
    }

    #[test]
    fn malformed_numbers_are_errors() {
        for input in ["1.", "-", "1e", "1e+", "[01]"] {
            let (_, eof) = scan(input);
            assert_eq!(eof, Op::Error, "{} was accepted", input);
        }
    }

    #[test]
    fn two_top_level_numbers_are_two_documents() {
        // `01` is not a number, but the top-level value ends after `0` and `1` starts another
        // document, so the scanner accepts it.
        let (ops, eof) = scan("01");
        assert_eq!(ops, vec![Op::BeginLiteral, Op::BeginLiteral]);
        assert_eq!(eof, Op::End);
    }

    #[test]
    fn string_escapes_are_accepted() {
        let (ops, eof) = scan(r#""a\u00e9\n\\\/b""#);
        assert!(!ops.contains(&Op::Error));
        assert_eq!(eof, Op::End);
    }

    #[test]
    fn bad_string_escape_is_an_error() {
        let (_, eof) = scan(r#""a\xb""#);
        assert_eq!(eof, Op::Error);
    }

    #[test]
    fn raw_control_character_in_string_is_an_error() {
        let (_, eof) = scan("\"a\nb\"");
        assert_eq!(eof, Op::Error);
    }

    #[test]
    fn error_names_the_character_and_position() {
        let mut scanner = Scanner::new();
        for c in r#"{"a":j"#.chars() {
            scanner.step(c);
        }
        let err = scanner.err.expect("scan should have failed");
        assert!(err.contains("'j'"), "{}", err);
        assert!(err.contains("position 6"), "{}", err);
    }

    #[test]
    fn several_documents_in_a_row_are_scanned() {
        let (ops, eof) = scan(r#"{"a":1} {"b":2}"#);
        assert!(!ops.contains(&Op::Error));
        assert_eq!(ops.iter().filter(|op| **op == Op::BeginObject).count(), 2);
        // The space between the two documents lands after the first one ended.
        assert!(ops.contains(&Op::End));
        assert_eq!(eof, Op::End);
    }

    #[test]
    fn unclosed_object_fails_at_eof() {
        let (ops, eof) = scan(r#"{"a":1"#);
        assert!(!ops.contains(&Op::Error));
        assert_eq!(eof, Op::Error);
    }

    #[test]
    fn empty_object_and_array() {
        for input in ["{}", "[]", "{ }", "[ ]", r#"{"a":{},"b":[]}"#] {
            let (ops, eof) = scan(input);
            assert!(!ops.contains(&Op::Error), "unexpected error scanning {}", input);
            assert_eq!(eof, Op::End, "{} did not end cleanly", input);
        }
    }

    #[test]
    fn missing_key_string_is_an_error() {
        let (_, eof) = scan("{a:1}");
        assert_eq!(eof, Op::Error);
    }
}
