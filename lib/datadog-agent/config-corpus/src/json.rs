//! A JSON scanner that keeps the facts a canonical-form check needs.
//!
//! `serde_json` normalizes escapes, member order and number text away, so the strict corpus reader
//! parses each line itself, from a `&str` that the caller has already checked is UTF-8. The scanner accepts no whitespace between tokens (canonical lines have
//! none), rejects unsorted or duplicate object members at every depth, and records for every string
//! whether its escaping is exactly what Go's `encoding/json` writes with `SetEscapeHTML(false)`.
//! Whether that matters depends on where the string sits, which only the line reader knows.

/// A parsed JSON value.
#[derive(Debug)]
pub(crate) enum Json {
    Null,
    Bool(bool),
    /// The number's text as written.
    Num(String),
    Str(JStr),
    Arr(Vec<Json>),
    /// Members in file order, which the scanner has checked to be strictly sorted.
    Obj(Vec<(JStr, Json)>),
}

/// A decoded string and whether its raw form is Go `encoding/json` canonical.
#[derive(Debug)]
pub(crate) struct JStr {
    pub(crate) text: String,
    pub(crate) go_canonical: bool,
}

impl Json {
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Json::Null => "null",
            Json::Bool(_) => "boolean",
            Json::Num(_) => "number",
            Json::Str(_) => "string",
            Json::Arr(_) => "array",
            Json::Obj(_) => "object",
        }
    }

    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(&s.text),
            _ => None,
        }
    }
}

/// Parses one line (without its `\n`) as a single canonical JSON value.
pub(crate) fn parse_line(line: &str) -> Result<Json, String> {
    let mut p = Parser {
        s: line,
        b: line.as_bytes(),
        i: 0,
    };
    let v = p.value()?;
    if p.i != line.len() {
        return Err(p.err("trailing bytes after the JSON value"));
    }
    Ok(v)
}

/// Go `encoding/json` string escaping with `SetEscapeHTML(false)`.
pub(crate) fn go_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Structural tokens are ASCII, so the parser steps over bytes and switches to `char_indices` only
/// inside strings; `i` is therefore always a char boundary.
struct Parser<'a> {
    s: &'a str,
    b: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn err(&self, what: &str) -> String {
        match self.b.get(self.i) {
            Some(c) if c.is_ascii_whitespace() => {
                format!("canonical form: insignificant whitespace at byte {}", self.i + 1)
            }
            _ => format!("canonical form: {what} at byte {}", self.i + 1),
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn expect(&mut self, c: u8) -> Result<(), String> {
        if self.peek() == Some(c) {
            self.i += 1;
            Ok(())
        } else {
            Err(self.err(&format!("expected '{}'", c as char)))
        }
    }

    fn literal(&mut self, word: &str, v: Json) -> Result<Json, String> {
        if self.b[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            Ok(v)
        } else {
            Err(self.err("invalid literal"))
        }
    }

    fn value(&mut self) -> Result<Json, String> {
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => self.literal("true", Json::Bool(true)),
            Some(b'f') => self.literal("false", Json::Bool(false)),
            Some(b'n') => self.literal("null", Json::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(self.err("expected a JSON value")),
        }
    }

    fn object(&mut self) -> Result<Json, String> {
        self.expect(b'{')?;
        let mut members: Vec<(JStr, Json)> = Vec::new();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(Json::Obj(members));
        }
        loop {
            let at = self.i;
            if self.peek() != Some(b'"') {
                return Err(self.err("expected a member name"));
            }
            let key = self.string()?;
            if let Some((prev, _)) = members.last() {
                if prev.text.as_bytes() >= key.text.as_bytes() {
                    let why = if prev.text == key.text { "duplicate" } else { "unsorted" };
                    return Err(format!(
                        "canonical form: {why} object member {:?} after {:?} at byte {}",
                        key.text,
                        prev.text,
                        at + 1
                    ));
                }
            }
            self.expect(b':')?;
            let v = self.value()?;
            members.push((key, v));
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Json::Obj(members));
                }
                _ => return Err(self.err("expected ',' or '}'")),
            }
        }
    }

    fn array(&mut self) -> Result<Json, String> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            items.push(self.value()?);
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(Json::Arr(items));
                }
                _ => return Err(self.err("expected ',' or ']'")),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let digits = self
            .s
            .get(self.i..self.i + 4)
            .ok_or_else(|| self.err("short \\u escape"))?;
        if !digits.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(self.err("bad \\u escape"));
        }
        let v = u32::from_str_radix(digits, 16).map_err(|_| self.err("bad \\u escape"))?;
        self.i += 4;
        Ok(v)
    }

    fn string(&mut self) -> Result<JStr, String> {
        let start = self.i;
        self.expect(b'"')?;
        let mut text = String::new();
        loop {
            let Some(c) = self.peek() else {
                return Err(self.err("unterminated string"));
            };
            match c {
                b'"' => {
                    self.i += 1;
                    break;
                }
                b'\\' => {
                    self.i += 1;
                    let e = self.peek().ok_or_else(|| self.err("unterminated escape"))?;
                    self.i += 1;
                    match e {
                        b'"' => text.push('"'),
                        b'\\' => text.push('\\'),
                        b'/' => text.push('/'),
                        b'b' => text.push('\u{8}'),
                        b'f' => text.push('\u{c}'),
                        b'n' => text.push('\n'),
                        b'r' => text.push('\r'),
                        b't' => text.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let cp = if (0xd800..0xdc00).contains(&hi) {
                                if !self.b[self.i..].starts_with(b"\\u") {
                                    return Err(self.err("unpaired surrogate escape"));
                                }
                                self.i += 2;
                                let lo = self.hex4()?;
                                if !(0xdc00..0xe000).contains(&lo) {
                                    return Err(self.err("unpaired surrogate escape"));
                                }
                                0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00)
                            } else {
                                hi
                            };
                            let ch = char::from_u32(cp).ok_or_else(|| self.err("invalid \\u escape"))?;
                            text.push(ch);
                        }
                        _ => return Err(self.err("invalid escape")),
                    }
                }
                c if c < 0x20 => return Err(self.err("unescaped control character in string")),
                _ => {
                    // Copy the run of plain characters up to the next quote, escape or control.
                    let rest = &self.s[self.i..];
                    let end = rest
                        .char_indices()
                        .find(|&(_, ch)| ch == '"' || ch == '\\' || (ch as u32) < 0x20)
                        .map_or(rest.len(), |(at, _)| at);
                    text.push_str(&rest[..end]);
                    self.i += end;
                }
            }
        }
        let raw = &self.b[start..self.i];
        let go_canonical = go_escape(&text).as_bytes() == raw;
        Ok(JStr { text, go_canonical })
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        match self.peek() {
            Some(b'0') => self.i += 1,
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.i += 1;
                }
            }
            _ => return Err(self.err("invalid number")),
        }
        if self.peek() == Some(b'.') {
            self.i += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.err("invalid number"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.i += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.err("invalid number"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.i += 1;
            }
        }
        Ok(Json::Num(self.s[start..self.i].to_string()))
    }
}

impl Json {
    /// Converts to a `serde_json::Value`, for payloads the model keeps as JSON.
    pub(crate) fn to_value(&self) -> Result<serde_json::Value, String> {
        use serde_json::Value;
        Ok(match self {
            Json::Null => Value::Null,
            Json::Bool(b) => Value::Bool(*b),
            Json::Num(n) => Value::Number(
                n.parse::<serde_json::Number>()
                    .map_err(|e| format!("number {n} is out of range: {e}"))?,
            ),
            Json::Str(s) => Value::String(s.text.clone()),
            Json::Arr(a) => Value::Array(a.iter().map(Json::to_value).collect::<Result<_, _>>()?),
            Json::Obj(m) => Value::Object(
                m.iter()
                    .map(|(k, v)| Ok((k.text.clone(), v.to_value()?)))
                    .collect::<Result<_, String>>()?,
            ),
        })
    }
}
