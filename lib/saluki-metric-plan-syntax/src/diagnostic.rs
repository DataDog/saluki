//! Source positions and the diagnostics that point at them.

use std::fmt;

/// A range of the source text, as byte offsets: from `start` up to, but not including, `end`.
///
/// Both offsets lie on character boundaries of the source the span was produced from, so `&source[span.range()]` is
/// always valid for that source.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct Span {
    /// The offset of the first byte.
    pub start: usize,

    /// The offset just past the last byte.
    pub end: usize,
}

impl Span {
    /// Creates a span from `start` up to, but not including, `end`.
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// Returns the span as a byte range.
    pub const fn range(&self) -> std::ops::Range<usize> {
        self.start..self.end
    }

    /// Returns the span that covers both this span and `other`.
    pub(crate) fn to(self, other: Span) -> Span {
        Span::new(self.start.min(other.start), self.end.max(other.end))
    }
}

/// A problem found while parsing, with where it is in the source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Diagnostic {
    /// What is wrong, and what was expected instead.
    pub message: String,

    /// The line the problem starts on, counting from 1.
    pub line: usize,

    /// The column the problem starts at, counting from 1, in characters (Unicode scalar values) from the start of the
    /// line.
    pub column: usize,

    /// The offending source text.
    pub span: Span,
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}: {}", self.line, self.column, self.message)
    }
}

/// Every problem found while parsing, in source order.
///
/// A syntax error ends parsing, so there is at most one, which may follow a note on its likely cause, such as an
/// operator that is not registered; problems found after the source parses, such as an edge to an undefined node or a
/// port the operator does not have, are all reported together.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Diagnostics(Vec<Diagnostic>);

impl Diagnostics {
    /// Returns the number of diagnostics.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns `true` if there are no diagnostics.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns an iterator over the diagnostics, in source order.
    pub fn iter(&self) -> impl Iterator<Item = &Diagnostic> {
        self.0.iter()
    }

    /// Returns the diagnostics as a slice, in source order.
    pub fn as_slice(&self) -> &[Diagnostic] {
        &self.0
    }
}

impl IntoIterator for Diagnostics {
    type Item = Diagnostic;
    type IntoIter = std::vec::IntoIter<Diagnostic>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> IntoIterator for &'a Diagnostics {
    type Item = &'a Diagnostic;
    type IntoIter = std::slice::Iter<'a, Diagnostic>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl fmt::Display for Diagnostics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, diagnostic) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str("\n")?;
            }
            fmt::Display::fmt(diagnostic, f)?;
        }
        Ok(())
    }
}

impl std::error::Error for Diagnostics {}

/// A problem found while parsing, before its line and column are computed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Error {
    pub(crate) message: String,
    pub(crate) span: Span,
}

impl Error {
    pub(crate) fn new(span: Span, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            span,
        }
    }
}

/// Converts errors into diagnostics for `source`, sorted by position.
pub(crate) fn diagnostics(source: &str, mut errors: Vec<Error>) -> Diagnostics {
    errors.sort_by_key(|error| (error.span.start, error.span.end));
    let lines = LineIndex::new(source);
    Diagnostics(
        errors
            .into_iter()
            .map(|error| {
                let (line, column) = lines.line_column(source, error.span.start);
                Diagnostic {
                    message: error.message,
                    line,
                    column,
                    span: error.span,
                }
            })
            .collect(),
    )
}

/// The distance, in bytes, between the offsets at which [`LineIndex`] records how many characters precede them.
const CHECKPOINT: usize = 256;

/// Where each line of a source starts, and how many characters precede regularly spaced offsets.
///
/// Finding a line and column then takes time independent of the length of the line, so reporting many problems on
/// one long line takes time proportional to the number of problems.
pub(crate) struct LineIndex {
    /// The byte offset at which each line starts.
    starts: Vec<usize>,

    /// The number of characters before each multiple of [`CHECKPOINT`] bytes.
    checkpoints: Vec<usize>,
}

impl LineIndex {
    pub(crate) fn new(source: &str) -> Self {
        let bytes = source.as_bytes();
        let starts = std::iter::once(0)
            .chain(
                bytes
                    .iter()
                    .enumerate()
                    .filter(|(_, b)| **b == b'\n')
                    .map(|(i, _)| i + 1),
            )
            .collect();
        let mut checkpoints = Vec::with_capacity(bytes.len() / CHECKPOINT + 1);
        let mut characters = 0;
        checkpoints.push(0);
        for chunk in bytes.chunks(CHECKPOINT) {
            characters += count_characters(chunk);
            checkpoints.push(characters);
        }
        Self { starts, checkpoints }
    }

    /// Returns the line and column, both counting from 1, of the character at `offset`.
    ///
    /// An offset past the end of the source is clamped to the end, and an offset inside a character is moved back to
    /// the character's start.
    pub(crate) fn line_column(&self, source: &str, offset: usize) -> (usize, usize) {
        let mut offset = offset.min(source.len());
        while !source.is_char_boundary(offset) {
            offset -= 1;
        }
        let line = self.starts.partition_point(|&start| start <= offset).max(1);
        let start = self.starts[line - 1];
        let column = self.characters_before(source, offset) - self.characters_before(source, start) + 1;
        (line, column)
    }

    /// Returns the number of characters before the byte offset `offset`, which is a character boundary.
    fn characters_before(&self, source: &str, offset: usize) -> usize {
        let checkpoint = offset / CHECKPOINT;
        self.checkpoints[checkpoint] + count_characters(&source.as_bytes()[checkpoint * CHECKPOINT..offset])
    }
}

/// Returns the number of characters that start in `bytes`, a slice of UTF-8 text: the bytes that do not continue a
/// character.
fn count_characters(bytes: &[u8]) -> usize {
    bytes.iter().filter(|&&b| (b & 0xC0) != 0x80).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_and_columns_count_from_one_in_characters() {
        let source = "ab\nçd\n\nx";
        let lines = LineIndex::new(source);
        // (offset, line, column)
        let cases = [
            (0, 1, 1),
            (1, 1, 2),
            (2, 1, 3),
            (3, 2, 1),
            // `ç` is two bytes, so `d` is at byte 5 but column 2.
            (5, 2, 2),
            // Inside `ç`: moved back to its start.
            (4, 2, 1),
            (7, 3, 1),
            (8, 4, 1),
            // Past the end: clamped to the end.
            (100, 4, 2),
        ];
        for (offset, line, column) in cases {
            assert_eq!(lines.line_column(source, offset), (line, column), "offset {}", offset);
        }
    }

    #[test]
    fn columns_on_long_lines_match_counting_from_the_line_start() {
        // Lines that span many checkpoints, with characters of every UTF-8 length straddling them.
        let line = "a\u{e7}\u{2713}\u{1F600}".repeat(200);
        let shorter = line.chars().take(333).collect::<String>();
        let source = format!("{}\n{}\n\n{}", line, shorter, line);
        let lines = LineIndex::new(&source);
        for offset in 0..=source.len() {
            let mut boundary = offset;
            while !source.is_char_boundary(boundary) {
                boundary -= 1;
            }
            let line_start = source[..boundary].rfind('\n').map_or(0, |newline| newline + 1);
            let expected = (
                source[..boundary].matches('\n').count() + 1,
                source[line_start..boundary].chars().count() + 1,
            );
            assert_eq!(lines.line_column(&source, offset), expected, "offset {}", offset);
        }
    }

    #[test]
    fn diagnostics_are_sorted_and_displayed_with_their_positions() {
        let source = "one\ntwo";
        let diagnostics = diagnostics(
            source,
            vec![
                Error::new(Span::new(4, 7), "second"),
                Error::new(Span::new(1, 2), "first"),
            ],
        );
        assert_eq!(diagnostics.to_string(), "1:2: first\n2:1: second");
        assert_eq!(diagnostics.len(), 2);
    }
}
