//! SQL tokenizer for obfuscation.

use super::obfuscator::SqlObfuscationConfig;

/// Token types recognized by the SQL tokenizer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    LexError,
    EndChar,

    ID,
    QuotedID,
    Limit,
    Null,
    String,
    DoubleQuotedString,
    DollarQuotedString,
    DollarQuotedFunc,
    Number,
    BooleanLiteral,
    ValueArg,
    ListArg,
    Comment,
    Variable,
    Savepoint,
    PreparedStatement,
    EscapeSequence,

    NullSafeEqual,
    LE,
    GE,
    NE,
    Not,
    ColonCast,

    As,
    Alter,
    Drop,
    Create,
    Grant,
    Revoke,
    Commit,
    Begin,
    Truncate,
    Select,
    From,
    Update,
    Delete,
    Insert,
    Into,
    Join,
    TableName,

    JSONSelect,
    JSONSelectText,
    JSONSelectPath,
    JSONSelectPathText,
    JSONContains,
    JSONContainsLeft,
    JSONAnyKeysExist,
    JSONAllKeysExist,
    JSONDelete,

    FilteredGroupable,
    FilteredGroupableParenthesis,
    Filtered,
    FilteredBracketedIdentifier,
}

const END_CHAR: char = char::MAX;

/// SQL tokenizer that breaks SQL strings into tokens.
pub struct SQLTokenizer<'a> {
    buf: &'a [u8],
    pos: usize,
    off: usize,
    last_char: char,
    err: Option<String>,
    literal_escapes: bool,
    curlys: u32,
    config: &'a SqlObfuscationConfig,
}

impl<'a> SQLTokenizer<'a> {
    pub fn new(sql: &'a str, literal_escapes: bool, config: &'a SqlObfuscationConfig) -> Self {
        Self {
            buf: sql.as_bytes(),
            pos: 0,
            off: 0,
            last_char: '\0',
            err: None,
            literal_escapes,
            curlys: 0,
            config,
        }
    }

    pub fn err(&self) -> Option<&str> {
        self.err.as_deref()
    }

    pub fn scan(&mut self) -> (TokenKind, &'a [u8]) {
        if self.last_char == '\0' {
            self.advance();
        }
        self.skip_blank();

        let ch = self.last_char;

        if is_leading_letter(ch) && !(self.config.dbms() == "postgresql" && ch == '@') {
            return self.scan_identifier();
        }

        if is_digit(ch) {
            return self.scan_number(false);
        }

        self.advance();
        if self.last_char == END_CHAR && self.err.is_some() {
            return (TokenKind::LexError, &[]);
        }

        match ch {
            END_CHAR => {
                if self.err.is_some() {
                    return (TokenKind::LexError, &[]);
                }
                (TokenKind::EndChar, &[])
            }
            ':' => {
                if self.last_char == ':' {
                    self.advance();
                    let start = self.pos - 2;
                    (TokenKind::ColonCast, &self.buf[start..self.pos])
                } else if self.last_char.is_whitespace() {
                    (TokenKind::ID, &self.buf[self.pos - 1..self.pos])
                } else if self.last_char != '=' {
                    self.scan_bind_var()
                } else {
                    (TokenKind::ID, &self.buf[self.pos - 1..self.pos])
                }
            }
            '\'' => self.scan_string('\'', TokenKind::String),
            '"' => self.scan_string('"', TokenKind::QuotedID),
            '`' => self.scan_string('`', TokenKind::QuotedID),
            '.' => {
                if is_digit(self.last_char) {
                    self.scan_number(true)
                } else {
                    (TokenKind::ID, &self.buf[self.pos - 1..self.pos])
                }
            }
            '(' | ')' | '[' | ']' | ',' | ';' => (TokenKind::ID, &self.buf[self.pos - 1..self.pos]),
            '-' => {
                if self.last_char == '-' {
                    self.advance();
                    self.scan_comment_to_eol()
                } else if self.last_char == '>' {
                    self.advance();
                    if self.last_char == '>' {
                        self.advance();
                        let start = self.pos - 3;
                        (TokenKind::JSONSelectText, &self.buf[start..self.pos])
                    } else {
                        let start = self.pos - 2;
                        (TokenKind::JSONSelect, &self.buf[start..self.pos])
                    }
                } else if is_digit(self.last_char) {
                    self.scan_number(false)
                } else {
                    (TokenKind::ID, &self.buf[self.pos - 1..self.pos])
                }
            }
            '/' => {
                if self.last_char == '*' {
                    self.advance();
                    self.scan_comment_multiline()
                } else {
                    (TokenKind::ID, &self.buf[self.pos - 1..self.pos])
                }
            }
            '#' => {
                if self.last_char == '>' {
                    self.advance();
                    if self.last_char == '>' {
                        self.advance();
                        let start = self.pos - 3;
                        (TokenKind::JSONSelectPathText, &self.buf[start..self.pos])
                    } else {
                        let start = self.pos - 2;
                        (TokenKind::JSONSelectPath, &self.buf[start..self.pos])
                    }
                } else if self.last_char == '-' {
                    self.advance();
                    let start = self.pos - 2;
                    (TokenKind::JSONDelete, &self.buf[start..self.pos])
                } else {
                    self.scan_comment_to_eol()
                }
            }
            '$' => self.scan_dollar_quoted_or_var(),
            '?' => {
                if self.last_char == '|' {
                    self.advance();
                    let start = self.pos - 2;
                    (TokenKind::JSONAnyKeysExist, &self.buf[start..self.pos])
                } else if self.last_char == '&' {
                    self.advance();
                    let start = self.pos - 2;
                    (TokenKind::JSONAllKeysExist, &self.buf[start..self.pos])
                } else {
                    (TokenKind::PreparedStatement, &self.buf[self.pos - 1..self.pos])
                }
            }
            '@' => {
                if self.last_char == '>' {
                    self.advance();
                    let start = self.pos - 2;
                    (TokenKind::JSONContains, &self.buf[start..self.pos])
                } else if self.last_char == '@' {
                    self.advance();
                    self.scan_bind_var()
                } else {
                    self.scan_bind_var()
                }
            }
            '<' => {
                if self.last_char == '=' {
                    self.advance();
                    if self.last_char == '>' {
                        self.advance();
                        let start = self.pos - 3;
                        (TokenKind::NullSafeEqual, &self.buf[start..self.pos])
                    } else {
                        let start = self.pos - 2;
                        (TokenKind::LE, &self.buf[start..self.pos])
                    }
                } else if self.last_char == '>' {
                    self.advance();
                    let start = self.pos - 2;
                    (TokenKind::NE, &self.buf[start..self.pos])
                } else if self.last_char == '@' {
                    self.advance();
                    let start = self.pos - 2;
                    (TokenKind::JSONContainsLeft, &self.buf[start..self.pos])
                } else {
                    (TokenKind::ID, &self.buf[self.pos - 1..self.pos])
                }
            }
            '>' => {
                if self.last_char == '=' {
                    self.advance();
                    let start = self.pos - 2;
                    (TokenKind::GE, &self.buf[start..self.pos])
                } else {
                    (TokenKind::ID, &self.buf[self.pos - 1..self.pos])
                }
            }
            '!' => {
                if self.last_char == '=' {
                    self.advance();
                    let start = self.pos - 2;
                    (TokenKind::NE, &self.buf[start..self.pos])
                } else {
                    (TokenKind::Not, &self.buf[self.pos - 1..self.pos])
                }
            }
            '%' => {
                if self.last_char == '(' {
                    self.scan_variable_identifier()
                } else if is_letter(self.last_char) {
                    self.scan_format_parameter()
                } else {
                    // Modulo operator
                    (TokenKind::ID, &self.buf[self.pos - 1..self.pos])
                }
            }
            '=' | '+' | '*' | '&' | '|' | '^' | '~' => (TokenKind::ID, &self.buf[self.pos - 1..self.pos]),
            '{' => {
                if self.pos == 1 || self.curlys > 0 {
                    self.curlys += 1;
                    (TokenKind::ID, &self.buf[self.pos - 1..self.pos])
                } else {
                    self.scan_escape_sequence()
                }
            }
            '}' => {
                if self.curlys > 0 {
                    self.curlys -= 1;
                }
                (TokenKind::ID, &self.buf[self.pos - 1..self.pos])
            }
            _ => {
                self.set_err(&format!("unexpected character: {:?}", ch));
                (TokenKind::LexError, &[])
            }
        }
    }

    fn advance(&mut self) {
        if self.off >= self.buf.len() {
            self.pos = self.off;
            self.last_char = END_CHAR;
            return;
        }

        let ch = self.buf[self.off] as char;
        self.pos = self.off;
        self.off += 1;
        self.last_char = ch;
    }

    fn skip_blank(&mut self) {
        while matches!(self.last_char, ' ' | '\n' | '\r' | '\t') {
            self.advance();
        }
    }

    fn scan_identifier(&mut self) -> (TokenKind, &'a [u8]) {
        let start = self.pos;
        self.advance();

        while is_letter_or_digit(self.last_char) || matches!(self.last_char, '*' | '$' | '#') {
            self.advance();
        }

        let buf = &self.buf[start..self.pos];
        let upper = std::str::from_utf8(buf).unwrap_or("").to_uppercase();
        let kind = match upper.as_str() {
            "NULL" => TokenKind::Null,
            "TRUE" | "FALSE" => TokenKind::BooleanLiteral,
            "SAVEPOINT" => TokenKind::Savepoint,
            "LIMIT" => TokenKind::Limit,
            "AS" => TokenKind::As,
            "ALTER" => TokenKind::Alter,
            "CREATE" => TokenKind::Create,
            "GRANT" => TokenKind::Grant,
            "REVOKE" => TokenKind::Revoke,
            "COMMIT" => TokenKind::Commit,
            "BEGIN" => TokenKind::Begin,
            "TRUNCATE" => TokenKind::Truncate,
            "DROP" => TokenKind::Drop,
            "SELECT" => TokenKind::Select,
            "FROM" => TokenKind::From,
            "UPDATE" => TokenKind::Update,
            "DELETE" => TokenKind::Delete,
            "INSERT" => TokenKind::Insert,
            "INTO" => TokenKind::Into,
            "JOIN" => TokenKind::Join,
            _ => TokenKind::ID,
        };

        (kind, buf)
    }

    fn scan_number(&mut self, seen_dot: bool) -> (TokenKind, &'a [u8]) {
        let start = if seen_dot { self.pos - 1 } else { self.pos };
        let mut has_dot = seen_dot;

        while is_digit(self.last_char) || (!has_dot && self.last_char == '.') {
            if self.last_char == '.' {
                has_dot = true;
            }
            self.advance();
        }

        // Handle scientific notation
        if self.last_char == 'e' || self.last_char == 'E' {
            self.advance();
            if self.last_char == '+' || self.last_char == '-' {
                self.advance();
            }
            while is_digit(self.last_char) {
                self.advance();
            }
        }

        let buf = &self.buf[start..self.pos];
        (TokenKind::Number, buf)
    }

    fn scan_string(&mut self, quote: char, kind: TokenKind) -> (TokenKind, &'a [u8]) {
        let start = self.pos;

        loop {
            let ch = self.last_char;
            self.advance();

            if ch == END_CHAR {
                self.set_err("unexpected end of input in string");
                return (TokenKind::LexError, &[]);
            }

            if ch == quote {
                if self.last_char == quote {
                    // Doubled quote - continue
                    self.advance();
                    continue;
                } else {
                    // End of string
                    break;
                }
            }

            if ch == '\\' && !self.literal_escapes {
                self.advance();
            }
        }

        let buf = &self.buf[start..self.pos - 1];
        (kind, buf)
    }

    fn scan_comment_to_eol(&mut self) -> (TokenKind, &'a [u8]) {
        let start = self.pos - 2;

        while self.last_char != '\n' && self.last_char != END_CHAR {
            self.advance();
        }

        let buf = &self.buf[start..self.pos];
        (TokenKind::Comment, buf)
    }

    fn scan_comment_multiline(&mut self) -> (TokenKind, &'a [u8]) {
        let start = self.pos - 2;

        loop {
            if self.last_char == END_CHAR {
                self.set_err("unexpected end of input in comment");
                return (TokenKind::LexError, &[]);
            }

            if self.last_char == '*' {
                self.advance();
                if self.last_char == '/' {
                    self.advance();
                    break;
                }
            } else {
                self.advance();
            }
        }

        let buf = &self.buf[start..self.pos];
        (TokenKind::Comment, buf)
    }

    fn scan_dollar_quoted_or_var(&mut self) -> (TokenKind, &'a [u8]) {
        let start = self.pos - 1;
        let tag_start = self.pos;

        while is_letter_or_digit(self.last_char) || self.last_char == '_' {
            self.advance();
        }

        if self.last_char == '$' {
            self.advance();
            let tag = &self.buf[tag_start..self.pos - 1];
            return self.scan_dollar_quoted_string(tag, start);
        }

        let buf = &self.buf[start..self.pos];
        (TokenKind::Variable, buf)
    }

    fn scan_dollar_quoted_string(&mut self, tag: &[u8], start: usize) -> (TokenKind, &'a [u8]) {
        let mut match_idx = 0;
        let mut in_end_tag = false;
        let tag_len = tag.len();

        loop {
            if self.last_char == END_CHAR {
                self.set_err("unexpected end of input in dollar-quoted string");
                return (TokenKind::LexError, &[]);
            }

            if self.last_char == '$' {
                if in_end_tag && match_idx == tag_len {
                    self.advance();
                    break;
                } else {
                    in_end_tag = true;
                    match_idx = 0;
                }
            } else if in_end_tag {
                if match_idx < tag_len && self.last_char as u8 == tag[match_idx] {
                    match_idx += 1;
                } else {
                    in_end_tag = false;
                    match_idx = 0;
                }
            }

            self.advance();
        }

        let content_start = start + tag_len + 2;
        let content_end = self.pos - tag_len - 2;
        let buf = &self.buf[content_start..content_end];

        let kind = if self.config.dollar_quoted_func() && tag == b"func" {
            TokenKind::DollarQuotedFunc
        } else {
            TokenKind::DollarQuotedString
        };

        (kind, buf)
    }

    fn scan_variable_identifier(&mut self) -> (TokenKind, &'a [u8]) {
        let start = self.pos - 1;

        while self.last_char != ')' && self.last_char != END_CHAR {
            self.advance();
        }

        self.advance();

        if !is_letter(self.last_char) {
            self.set_err(&format!(
                "invalid character after variable identifier: '{}' ({})",
                self.last_char, self.last_char as u32
            ));
            return (TokenKind::LexError, &self.buf[start..self.pos]);
        }

        self.advance();

        (TokenKind::Variable, &self.buf[start..self.pos])
    }

    fn scan_format_parameter(&mut self) -> (TokenKind, &'a [u8]) {
        let start = self.pos - 1;
        self.advance();
        (TokenKind::Variable, &self.buf[start..self.pos])
    }

    fn scan_bind_var(&mut self) -> (TokenKind, &'a [u8]) {
        let start = self.pos - 1;
        let mut token = TokenKind::ValueArg;

        if self.last_char == ':' {
            token = TokenKind::ListArg;
            self.advance();
        }

        if !is_letter(self.last_char) && !is_digit(self.last_char) {
            self.set_err(&format!(
                "bind variables should start with letters or digits, got '{}' ({})",
                self.last_char, self.last_char as u32
            ));
            return (TokenKind::LexError, &self.buf[start..self.pos]);
        }

        while is_letter(self.last_char) || is_digit(self.last_char) || self.last_char == '.' {
            self.advance();
        }

        (token, &self.buf[start..self.pos])
    }

    fn scan_escape_sequence(&mut self) -> (TokenKind, &'a [u8]) {
        let start = self.pos - 1;

        while self.last_char != '}' && self.last_char != END_CHAR {
            self.advance();
        }

        if self.last_char == END_CHAR {
            self.set_err("unexpected EOF in escape sequence");
            return (TokenKind::LexError, &self.buf[start..self.pos]);
        }

        self.advance();

        (TokenKind::EscapeSequence, &self.buf[start..self.pos])
    }

    fn set_err(&mut self, msg: &str) {
        if self.err.is_none() {
            self.err = Some(format!("at position {}: {}", self.pos, msg));
        }
    }
}

impl From<char> for TokenKind {
    fn from(ch: char) -> Self {
        match ch {
            '(' | ')' | '[' | ']' | ',' | ';' => TokenKind::ID,
            _ => TokenKind::ID,
        }
    }
}

fn is_leading_letter(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_'
}

fn is_letter_or_digit(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

fn is_digit(ch: char) -> bool {
    ch.is_ascii_digit()
}

fn is_letter(ch: char) -> bool {
    ch.is_ascii_alphabetic()
}
