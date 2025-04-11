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
    use super::*;

    #[test]
    fn test_lower_alphanumeric() {
        assert_eq!(lower_alphanumeric("Hello World!"), "hello_world_");
        assert_eq!(lower_alphanumeric("1234"), "1234");
        assert_eq!(lower_alphanumeric("abc_def"), "abc_def");
        assert_eq!(lower_alphanumeric("abc-def"), "abc_def");
        assert_eq!(lower_alphanumeric("abc def"), "abc_def");
    }
}
