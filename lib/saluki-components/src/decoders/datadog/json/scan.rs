//! A syntax pass over a JSON body that stops where Go's `json.Decoder.Decode` stops.
//!
//! The reference decodes JSON bodies with `json.NewDecoder(body).Decode`, which scans the first value whole before
//! decoding any of it. So a syntax error, or the input ending early, wins over a type error earlier in the value, and
//! whatever follows the first value is never looked at. This pass reproduces that scan (`encoding/json/scanner.go`):
//! the same grammar, the same nesting limit, and the same point at which it stops reading.

/// Go's `maxNestingDepth`: an array or object nested deeper than this is a syntax error.
const MAX_NESTING_DEPTH: usize = 10_000;

/// The size of the decoder's first read, Go's `minRead` in `Decoder.refill`.
const MIN_READ: usize = 512;

/// How the scan of a body ended.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    /// The first value is complete, and is the first `len` bytes of the body, leading whitespace included.
    Value { len: usize },
    /// The body ended before the first value was complete: Go's `io.EOF` or `io.ErrUnexpectedEOF`.
    Eof,
    /// The byte at `at` isn't valid JSON there, or opens one array or object too many.
    Syntax { at: usize },
}

/// The result of a scan.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Scanned {
    pub(super) outcome: Outcome,
    /// How many bytes the scan looked at: a value that isn't an array or object only ends at the byte after it.
    pub(super) examined: usize,
}

/// Scans the first JSON value of `body`.
pub(super) fn scan(body: &[u8]) -> Scanned {
    let outcome = scan_value(body);
    let examined = match outcome {
        Outcome::Value { len } => {
            if matches!(body[len - 1], b'}' | b']') {
                len
            } else {
                (len + 1).min(body.len())
            }
        }
        Outcome::Eof => body.len(),
        Outcome::Syntax { at } => at + 1,
    };
    Scanned { outcome, examined }
}

/// Returns how much of a body of `len` bytes Go's `json.Decoder` has read by the time it has looked at `examined`
/// bytes.
///
/// The decoder reads into a buffer that starts at 512 bytes and grows to `2 * cap + 512` each time it fills, so it has
/// read 512, 1536, 3584, ... bytes. This assumes each read fills the buffer, as it does for a body already in memory;
/// over a connection, a read can return less.
pub(super) fn decoder_read(examined: usize, len: usize) -> usize {
    let mut read = MIN_READ;
    while read < examined {
        read = 2 * read + MIN_READ;
    }
    read.min(len)
}

/// Whitespace as the reference's scanner reads it.
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\r' | b'\n')
}

fn skip_space(body: &[u8], mut i: usize) -> usize {
    while body.get(i).copied().is_some_and(is_space) {
        i += 1;
    }
    i
}

/// What the scanner expects next.
enum Expect {
    /// A value.
    Value,
    /// A value, or the end of the array that was just opened.
    ValueOrArrayEnd,
    /// A key, or the end of the object that was just opened.
    KeyOrObjectEnd,
    /// A key, after a comma in an object.
    Key,
    /// A comma or the end of the enclosing array or object, after a value.
    CommaOrEnd,
}

fn scan_value(body: &[u8]) -> Outcome {
    // `true` for an object, `false` for an array. Bounded by the nesting limit.
    let mut stack: Vec<bool> = Vec::new();
    let mut expect = Expect::Value;
    let mut i = 0;
    loop {
        i = skip_space(body, i);
        let Some(&c) = body.get(i) else {
            return Outcome::Eof;
        };
        let value_start = match expect {
            Expect::Value => true,
            Expect::ValueOrArrayEnd if c == b']' => {
                stack.pop();
                i += 1;
                false
            }
            Expect::ValueOrArrayEnd => true,
            Expect::KeyOrObjectEnd if c == b'}' => {
                stack.pop();
                i += 1;
                false
            }
            Expect::KeyOrObjectEnd | Expect::Key => {
                if c != b'"' {
                    return Outcome::Syntax { at: i };
                }
                i = match scan_string(body, i) {
                    Ok(end) => skip_space(body, end),
                    Err(outcome) => return outcome,
                };
                match body.get(i) {
                    None => return Outcome::Eof,
                    Some(b':') => {}
                    Some(_) => return Outcome::Syntax { at: i },
                }
                i += 1;
                expect = Expect::Value;
                continue;
            }
            Expect::CommaOrEnd => {
                let object = *stack
                    .last()
                    .expect("a comma or an end is only expected in an array or object");
                match c {
                    b',' => {
                        i += 1;
                        expect = if object { Expect::Key } else { Expect::Value };
                        continue;
                    }
                    b'}' if object => {}
                    b']' if !object => {}
                    _ => return Outcome::Syntax { at: i },
                }
                stack.pop();
                i += 1;
                false
            }
        };

        if value_start {
            let end = match c {
                b'{' | b'[' => {
                    if stack.len() == MAX_NESTING_DEPTH {
                        return Outcome::Syntax { at: i };
                    }
                    stack.push(c == b'{');
                    expect = if c == b'{' {
                        Expect::KeyOrObjectEnd
                    } else {
                        Expect::ValueOrArrayEnd
                    };
                    i += 1;
                    continue;
                }
                b'"' => scan_string(body, i),
                b'-' | b'0'..=b'9' => scan_number(body, i),
                b't' => scan_literal(body, i, b"true"),
                b'f' => scan_literal(body, i, b"false"),
                b'n' => scan_literal(body, i, b"null"),
                _ => Err(Outcome::Syntax { at: i }),
            };
            i = match end {
                Ok(end) => end,
                Err(outcome) => return outcome,
            };
        }

        if stack.is_empty() {
            return Outcome::Value { len: i };
        }
        expect = Expect::CommaOrEnd;
    }
}

/// Scans the string starting with the quote at `start`, and returns the index after its closing quote.
fn scan_string(body: &[u8], start: usize) -> Result<usize, Outcome> {
    let mut i = start + 1;
    loop {
        let Some(&c) = body.get(i) else {
            return Err(Outcome::Eof);
        };
        match c {
            b'"' => return Ok(i + 1),
            b'\\' => {
                i += 1;
                match body.get(i) {
                    None => return Err(Outcome::Eof),
                    Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => i += 1,
                    Some(b'u') => {
                        i += 1;
                        for _ in 0..4 {
                            match body.get(i) {
                                None => return Err(Outcome::Eof),
                                Some(c) if c.is_ascii_hexdigit() => i += 1,
                                Some(_) => return Err(Outcome::Syntax { at: i }),
                            }
                        }
                    }
                    Some(_) => return Err(Outcome::Syntax { at: i }),
                }
            }
            // Bytes from 0x80 up are taken as they are, valid UTF-8 or not.
            0x00..=0x1f => return Err(Outcome::Syntax { at: i }),
            _ => i += 1,
        }
    }
}

/// Scans the number starting at `start`, and returns the index after it.
///
/// A number can only be known complete from the byte after it, so one that the input ends in is complete if it could
/// end there, and incomplete (`Eof`) otherwise.
fn scan_number(body: &[u8], start: usize) -> Result<usize, Outcome> {
    let digit_at = |i: usize| -> Result<(), Outcome> {
        match body.get(i) {
            None => Err(Outcome::Eof),
            Some(c) if c.is_ascii_digit() => Ok(()),
            Some(_) => Err(Outcome::Syntax { at: i }),
        }
    };
    let skip_digits = |mut i: usize| {
        while body.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        i
    };

    let mut i = start;
    if body[i] == b'-' {
        i += 1;
    }
    digit_at(i)?;
    // A leading zero ends the integer part: what follows `0` is the next token.
    i = if body[i] == b'0' { i + 1 } else { skip_digits(i) };
    if body.get(i) == Some(&b'.') {
        i += 1;
        digit_at(i)?;
        i = skip_digits(i);
    }
    if matches!(body.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(body.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        digit_at(i)?;
        i = skip_digits(i);
    }
    Ok(i)
}

/// Scans the literal `word` starting at `start`, and returns the index after it.
fn scan_literal(body: &[u8], start: usize, word: &[u8]) -> Result<usize, Outcome> {
    for (offset, expected) in word.iter().enumerate() {
        match body.get(start + offset) {
            None => return Err(Outcome::Eof),
            Some(c) if c == expected => {}
            Some(_) => return Err(Outcome::Syntax { at: start + offset }),
        }
    }
    Ok(start + word.len())
}
