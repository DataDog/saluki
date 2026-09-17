//! HTTP URL obfuscation.
//!
//! The algorithm lives in `libdd_trace_obfuscation::http`, a shared crate that tracks the reference implementation's
//! URL handling: its escaping rules, its acceptance of relative references, and its decoding of percent-encoded path
//! characters before the digit scan.

use libdd_trace_obfuscation::http;
use stringtheory::MetaString;

use super::obfuscator::HttpObfuscationConfig;

/// Obfuscates a URL string by removing userinfo, query strings, and/or path digits.
///
/// Returns `Some(obfuscated)` if the URL changed, `None` if it did not. Userinfo is always removed; the query string
/// and path digits are removed per `config`. A URL that cannot be parsed loses its userinfo when both `config` options
/// are off, and is replaced with `?` when either option is on.
pub fn obfuscate_url(val: &str, config: &HttpObfuscationConfig) -> Option<MetaString> {
    let obfuscated = http::obfuscate_url_string(val, config.remove_query_string, config.remove_paths_with_digits);

    // The algorithm normalizes escaping even where nothing sensitive was found, so it can hand back a string equal to
    // its input. Report that as unchanged so the caller keeps the URL it already has instead of replacing it with an
    // identical copy.
    if obfuscated == val {
        return None;
    }

    Some(obfuscated.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obfuscate(val: &str, remove_query_string: bool, remove_paths_with_digits: bool) -> Option<String> {
        let config = HttpObfuscationConfig {
            remove_query_string,
            remove_paths_with_digits,
        };
        obfuscate_url(val, &config).map(|s| s.as_ref().to_owned())
    }

    // The algorithm normalizes escaping without finding anything sensitive, so it can hand back a string equal to
    // its input. Report that as no change so the caller keeps the URL it already has.
    #[test]
    fn unchanged_url_reports_no_change() {
        assert_eq!(obfuscate("http://foo.com/foo%20bar/", false, false), None);
        assert_eq!(obfuscate("http://foo.com/foo%20bar/", true, true), None);
    }
}
