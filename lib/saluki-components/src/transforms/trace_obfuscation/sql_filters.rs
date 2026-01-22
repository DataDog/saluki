//! SQL obfuscation filters.

use std::collections::HashSet;

use super::obfuscator::SqlObfuscationConfig;
use super::sql_tokenizer::TokenKind;

/// Token filter trait.
pub trait TokenFilter {
    /// Filter a token, possibly transforming it or returning None to discard it.
    fn filter(
        &mut self, token: TokenKind, last_token: TokenKind, buffer: &[u8], last_buffer: &[u8],
    ) -> Result<(TokenKind, Option<Vec<u8>>), String>;

    /// Reset the filter state.
    fn reset(&mut self);
}

/// Metadata extracted from SQL queries.
#[derive(Debug, Default)]
pub struct SQLMetadata {
    /// Comma-separated table names found in the query.
    pub tables_csv: String,
}

/// Filter that collects metadata (table names).
pub struct MetadataFinderFilter {
    collect_table_names: bool,
    replace_digits: bool,
    tables_seen: HashSet<String>,
    tables_csv: String,
}

impl MetadataFinderFilter {
    pub fn new(config: &SqlObfuscationConfig) -> Self {
        Self {
            collect_table_names: config.table_names(),
            replace_digits: config.replace_digits(),
            tables_seen: HashSet::new(),
            tables_csv: String::new(),
        }
    }

    pub fn results(&self) -> SQLMetadata {
        SQLMetadata {
            tables_csv: self.tables_csv.clone(),
        }
    }

    fn store_table_name(&mut self, name: String) {
        if self.tables_seen.contains(&name) {
            return;
        }
        self.tables_seen.insert(name.clone());
        if !self.tables_csv.is_empty() {
            self.tables_csv.push(',');
        }
        self.tables_csv.push_str(&name);
    }
}

impl TokenFilter for MetadataFinderFilter {
    fn filter(
        &mut self, token: TokenKind, last_token: TokenKind, buffer: &[u8], _last_buffer: &[u8],
    ) -> Result<(TokenKind, Option<Vec<u8>>), String> {
        if self.collect_table_names {
            match last_token {
                TokenKind::From | TokenKind::Join => {
                    if let Some(first_char) = buffer.first() {
                        if !(*first_char as char).is_alphabetic() {
                            // Nested query, skip
                        } else {
                            let mut table_name = String::from_utf8_lossy(buffer).to_string();
                            if self.replace_digits {
                                table_name = replace_digits(&table_name);
                            }
                            self.store_table_name(table_name);
                            return Ok((TokenKind::TableName, Some(buffer.to_vec())));
                        }
                    }
                }
                TokenKind::Update | TokenKind::Into => {
                    let mut table_name = String::from_utf8_lossy(buffer).to_string();
                    if self.replace_digits {
                        table_name = replace_digits(&table_name);
                    }
                    self.store_table_name(table_name);
                    return Ok((TokenKind::TableName, Some(buffer.to_vec())));
                }
                _ => {}
            }
        }

        Ok((token, Some(buffer.to_vec())))
    }

    fn reset(&mut self) {
        self.tables_seen.clear();
        self.tables_csv.clear();
    }
}

/// Filter that discards certain tokens (comments, AS aliases).
pub struct DiscardFilter {
    keep_sql_alias: bool,
}

impl DiscardFilter {
    pub fn new(keep_sql_alias: bool) -> Self {
        Self { keep_sql_alias }
    }
}

impl TokenFilter for DiscardFilter {
    fn filter(
        &mut self, token: TokenKind, last_token: TokenKind, buffer: &[u8], _last_buffer: &[u8],
    ) -> Result<(TokenKind, Option<Vec<u8>>), String> {
        match last_token {
            TokenKind::FilteredBracketedIdentifier => {
                if token != TokenKind::ID || buffer != b"]" {
                    return Ok((TokenKind::FilteredBracketedIdentifier, None));
                }
            }
            TokenKind::As => {
                if buffer == b"[" {
                    return Ok((TokenKind::FilteredBracketedIdentifier, None));
                }
                if !self.keep_sql_alias {
                    return Ok((TokenKind::Filtered, None));
                }
                return Ok((token, Some(buffer.to_vec())));
            }
            _ => {}
        }

        match token {
            TokenKind::Comment => Ok((TokenKind::Filtered, None)),
            TokenKind::ID if buffer == b";" => Ok((mark_filtered_groupable(token, buffer), None)),
            TokenKind::As => {
                if !self.keep_sql_alias {
                    Ok((TokenKind::As, None))
                } else {
                    Ok((token, Some(buffer.to_vec())))
                }
            }
            _ => Ok((token, Some(buffer.to_vec()))),
        }
    }

    fn reset(&mut self) {}
}

/// Filter that replaces literals with "?".
pub struct ReplaceFilter {
    replace_digits: bool,
    config: SqlObfuscationConfig,
}

impl ReplaceFilter {
    pub fn new(config: &SqlObfuscationConfig) -> Self {
        Self {
            replace_digits: config.replace_digits(),
            config: config.clone(),
        }
    }
}

impl TokenFilter for ReplaceFilter {
    fn filter(
        &mut self, token: TokenKind, last_token: TokenKind, buffer: &[u8], last_buffer: &[u8],
    ) -> Result<(TokenKind, Option<Vec<u8>>), String> {
        if token == TokenKind::DollarQuotedFunc {
            let content = String::from_utf8_lossy(buffer);
            let recursive_config = self.config.with_dollar_quoted_func_disabled();
            let obfuscated = super::sql::obfuscate_sql_string(&content, &recursive_config)?;
            let wrapped = format!("$func${}$func$", obfuscated.query);
            return Ok((token, Some(wrapped.into_bytes())));
        }

        match last_token {
            TokenKind::Savepoint => {
                return Ok((mark_filtered_groupable(token, buffer), Some(b"?".to_vec())));
            }
            TokenKind::ID if last_buffer == b"=" => {
                if token == TokenKind::DoubleQuotedString || token == TokenKind::QuotedID {
                    return Ok((mark_filtered_groupable(token, buffer), Some(b"?".to_vec())));
                }
            }
            _ => {}
        }

        match token {
            TokenKind::DollarQuotedString
            | TokenKind::String
            | TokenKind::Number
            | TokenKind::Null
            | TokenKind::Variable
            | TokenKind::PreparedStatement
            | TokenKind::BooleanLiteral
            | TokenKind::EscapeSequence => Ok((mark_filtered_groupable(token, buffer), Some(b"?".to_vec()))),
            TokenKind::TableName | TokenKind::ID | TokenKind::QuotedID => {
                if self.replace_digits {
                    let replaced = replace_digits_bytes(buffer);
                    Ok((token, Some(replaced)))
                } else {
                    Ok((token, Some(buffer.to_vec())))
                }
            }
            _ => Ok((token, Some(buffer.to_vec()))),
        }
    }

    fn reset(&mut self) {}
}

/// Filter that groups together consecutive "?" tokens.
pub struct GroupingFilter {
    group_filter: i32,
    group_multi: i32,
}

impl GroupingFilter {
    pub fn new() -> Self {
        Self {
            group_filter: 0,
            group_multi: 0,
        }
    }
}

impl TokenFilter for GroupingFilter {
    fn filter(
        &mut self, token: TokenKind, last_token: TokenKind, buffer: &[u8], _last_buffer: &[u8],
    ) -> Result<(TokenKind, Option<Vec<u8>>), String> {
        if ((last_token == TokenKind::ID || last_token == TokenKind::QuotedID)
            && buffer == b"("
            && is_filtered_groupable(token))
            || (buffer == b"(" && self.group_multi > 0)
        {
            self.group_multi += 1;
        }

        let is_start_of_subquery = matches!(
            token,
            TokenKind::Select | TokenKind::Delete | TokenKind::Update | TokenKind::ID | TokenKind::QuotedID
        );

        if self.group_multi > 0 && last_token == TokenKind::FilteredGroupableParenthesis && is_start_of_subquery {
            self.reset();
            let mut result = b"( ".to_vec();
            result.extend_from_slice(buffer);
            return Ok((token, Some(result)));
        }

        if is_filtered_groupable(token) {
            self.group_filter += 1;
            if self.group_filter > 1 {
                return Ok((mark_filtered_groupable(token, buffer), None));
            }
            return Ok((token, Some(buffer.to_vec())));
        }

        if self.group_filter > 0 && (buffer == b"," || buffer == b"?") {
            return Ok((mark_filtered_groupable(token, buffer), None));
        }

        if self.group_multi > 1 {
            return Ok((mark_filtered_groupable(token, buffer), None));
        }

        if buffer != b"," && buffer != b"(" && buffer != b")" && !is_filtered_groupable(token) {
            self.reset();
        }

        Ok((token, Some(buffer.to_vec())))
    }

    fn reset(&mut self) {
        self.group_filter = 0;
        self.group_multi = 0;
    }
}

fn is_filtered_groupable(token: TokenKind) -> bool {
    matches!(
        token,
        TokenKind::FilteredGroupable | TokenKind::FilteredGroupableParenthesis
    )
}

fn mark_filtered_groupable(_token: TokenKind, buffer: &[u8]) -> TokenKind {
    if buffer == b"(" {
        TokenKind::FilteredGroupableParenthesis
    } else {
        TokenKind::FilteredGroupable
    }
}

fn replace_digits(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            // Found a digit - consume all consecutive digits and replace with single '?'
            result.push('?');
            while chars.peek().is_some_and(|ch| ch.is_ascii_digit()) {
                chars.next();
            }
        } else {
            // Not a digit - copy as-is
            result.push(c);
        }
    }

    result
}

fn replace_digits_bytes(b: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(b.len());
    let mut i = 0;

    while i < b.len() {
        if (b[i] as char).is_ascii_digit() {
            // Found a digit - consume all consecutive digits and replace with single '?'
            result.push(b'?');
            while i < b.len() && (b[i] as char).is_ascii_digit() {
                i += 1;
            }
        } else {
            // Not a digit - copy as-is
            result.push(b[i]);
            i += 1;
        }
    }

    result
}
