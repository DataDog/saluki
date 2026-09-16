//! HTTP URL obfuscation.
//!
//! The rewrite itself lives in `libdd_trace_obfuscation::http`, a shared crate that tracks the reference
//! implementation's URL handling: its escaping rules, its acceptance of relative references, and its decoding of
//! percent-encoded path characters before the digit scan. This module adapts that leaf API to
//! [`HttpObfuscationConfig`] and [`MetaString`].
//!
//! # Missing
//!
//! Two behaviors of the shared crate diverge from the reference implementation. Each one is tracked upstream and
//! pinned by a test below, so the output moves visibly in a diff when the pinned revision moves:
//!
//! - A percent-escaped unreserved character in the path is decoded, so a URL the reference implementation emits
//!   byte-identical is rewritten (<https://github.com/DataDog/libdatadog/issues/2522>).
//! - A `[` or `]` in the path or fragment fails the crate's stricter parse, so the URL collapses to `?` whenever
//!   either config option is on (<https://github.com/DataDog/libdatadog/issues/2523>).
//!
//! Removing the userinfo from a URL that cannot be parsed is a deliberate divergence, not a gap: the reference
//! implementation returns such a URL verbatim, credentials included.

use libdd_trace_obfuscation::http;
use stringtheory::MetaString;

use super::obfuscator::HttpObfuscationConfig;

/// Obfuscates a URL string by removing userinfo, query strings, and/or path digits.
///
/// Returns `Some(obfuscated)` if the URL changed, `None` if it did not. Userinfo is always removed; the query string
/// and path digits are removed per `config`. A URL that cannot be parsed loses its userinfo when both `config` options
/// are off, and is replaced with `?` when either option is on.
///
/// # Design
///
/// This calls the rewrite directly rather than `libdd_trace_obfuscation::http::obfuscate_url`, which screens with
/// `libdd_trace_obfuscation::http::should_obfuscate_url` first. That scan allocates nothing, but it is not a superset of
/// the rewrite's triggers: it finds nothing to do in a URL whose authority cannot be parsed, such as
/// `https://example.com:port/x`, which the rewrite replaces with `?` when either option is on and which the reference
/// implementation redacts (<https://github.com/DataDog/libdatadog/issues/2524>). Screening with it would forward those
/// URLs unchanged, so every URL reaches the rewrite and the exact equality check below.
pub fn obfuscate_url(val: &str, config: &HttpObfuscationConfig) -> Option<MetaString> {
    let obfuscated = http::obfuscate_url_string(val, config.remove_query_string, config.remove_paths_with_digits);

    // The rewrite normalizes escaping even where nothing sensitive was found, so it can hand back a string equal to its
    // input. Report that as unchanged so the caller keeps the URL it already has instead of replacing it with an
    // identical copy.
    if obfuscated == val {
        return None;
    }

    Some(obfuscated.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(remove_query_string: bool, remove_paths_with_digits: bool) -> HttpObfuscationConfig {
        HttpObfuscationConfig {
            remove_query_string,
            remove_paths_with_digits,
        }
    }

    fn obfuscate(val: &str, remove_query_string: bool, remove_paths_with_digits: bool) -> Option<String> {
        obfuscate_url(val, &config(remove_query_string, remove_paths_with_digits)).map(|s| s.as_ref().to_owned())
    }

    // A percent-encoded digit is decoded before the digit scan, so `%32` redacts the same as `2`.
    #[test]
    fn encoded_digit_triggers_redaction() {
        assert_eq!(
            obfuscate("http://foo.com/users/%32/profile", false, true).as_deref(),
            Some("http://foo.com/users/?/profile")
        );
        assert_eq!(
            obfuscate("http://foo.com/id/%32%35/page", true, true).as_deref(),
            Some("http://foo.com/id/?/page")
        );
    }

    // An unparseable URL keeps everything but its userinfo when neither option is set. This is deliberately safer than
    // the reference implementation, which returns such a URL verbatim, credentials and all.
    #[test]
    fn unparseable_url_loses_userinfo_by_design() {
        assert_eq!(obfuscate("http://foo.com/%", false, false), None);
        assert_eq!(
            obfuscate("http://user:password@foo.com/%", false, false).as_deref(),
            Some("http://foo.com/%")
        );
        assert_eq!(
            obfuscate("http://user:password@foo.com/\u{1}", false, false).as_deref(),
            Some("http://foo.com/\u{1}")
        );
    }

    // An `@` in the query is not userinfo, so the fallback must not cut the URL there.
    #[test]
    fn unparseable_url_keeps_query_at_sign() {
        assert_eq!(obfuscate("http://foo.com?email=a@b\u{0}", false, false), None);
    }

    // With either option set, an unparseable URL is replaced wholesale.
    #[test]
    fn unparseable_url_becomes_question_mark() {
        assert_eq!(obfuscate("http://foo.com/%", true, false).as_deref(), Some("?"));
        assert_eq!(obfuscate("http://foo.com/%", false, true).as_deref(), Some("?"));
    }

    // A relative URL is a valid reference: redact its digit segments rather than dropping the endpoint.
    #[test]
    fn relative_url_redacts_digits() {
        assert_eq!(
            obfuscate("/users/123/profile", false, true).as_deref(),
            Some("/users/?/profile")
        );
        assert_eq!(
            obfuscate("/users/123/profile?page=2", true, true).as_deref(),
            Some("/users/?/profile?")
        );
        assert_eq!(obfuscate("/users/name/profile", false, true), None);
    }

    // Spaces do not make a URL unparseable; they are escaped, matching the reference implementation.
    #[test]
    fn spaces_are_escaped_not_dropped() {
        assert_eq!(
            obfuscate("this is not a valid url", true, true).as_deref(),
            Some("this%20is%20not%20a%20valid%20url")
        );
    }

    #[test]
    fn userinfo_removed_regardless_of_config() {
        assert_eq!(
            obfuscate("https://user:pass@example.com/path", false, false).as_deref(),
            Some("https://example.com/path")
        );
        assert_eq!(
            obfuscate("http://user:password@foo.com/1/2/3?q=james", false, false).as_deref(),
            Some("http://foo.com/1/2/3?q=james")
        );
    }

    #[test]
    fn nothing_to_do_leaves_url_alone() {
        assert_eq!(obfuscate("http://foo.com/1/2/3?q=james", false, false), None);
        assert_eq!(obfuscate("http://foo.com/", true, true), None);
        assert_eq!(obfuscate("http://foo.com/name/id", true, true), None);
        assert_eq!(obfuscate("http://foo.com/123", true, false), None);
        assert_eq!(obfuscate("http://foo.com/name?query=search", false, true), None);
    }

    #[test]
    fn query_string_removed() {
        assert_eq!(
            obfuscate("https://example.com/path?secret=value&key=data", true, false).as_deref(),
            Some("https://example.com/path?")
        );
        assert_eq!(
            obfuscate("http://foo.com/id/123/page/1?search=bar&page=2", true, false).as_deref(),
            Some("http://foo.com/id/123/page/1?")
        );
        assert_eq!(
            obfuscate("http://foo.com/id/123/page/1?blabla", true, false).as_deref(),
            Some("http://foo.com/id/123/page/1?")
        );
        assert_eq!(
            obfuscate("http://user:password@foo.com/1/2/3?q=james", true, false).as_deref(),
            Some("http://foo.com/1/2/3?")
        );
    }

    #[test]
    fn query_string_removal_keeps_fragment() {
        assert_eq!(
            obfuscate("http://foo.com/id/123/page/1?search=bar&page=2#fragment", true, false).as_deref(),
            Some("http://foo.com/id/123/page/1?#fragment")
        );
    }

    #[test]
    fn path_digits_removed() {
        assert_eq!(
            obfuscate("https://example.com/users/123/profile", false, true).as_deref(),
            Some("https://example.com/users/?/profile")
        );
        assert_eq!(
            obfuscate("http://foo.com/123", false, true).as_deref(),
            Some("http://foo.com/?")
        );
        assert_eq!(
            obfuscate("http://foo.com/123/abcd9", false, true).as_deref(),
            Some("http://foo.com/?/?")
        );
        assert_eq!(
            obfuscate("http://foo.com/123/name/abcd9", false, true).as_deref(),
            Some("http://foo.com/?/name/?")
        );
        assert_eq!(
            obfuscate("http://user:password@foo.com/1/2/3?q=james", false, true).as_deref(),
            Some("http://foo.com/?/?/??q=james")
        );
    }

    #[test]
    fn path_digit_removal_keeps_query_and_fragment() {
        assert_eq!(
            obfuscate("http://foo.com/id/123/page/1?search=bar&page=2", false, true).as_deref(),
            Some("http://foo.com/id/?/page/??search=bar&page=2")
        );
        assert_eq!(
            obfuscate(
                "http://foo.com/id/a1/page/1qwe233?search=bar&page=2#fragment-123",
                false,
                true
            )
            .as_deref(),
            Some("http://foo.com/id/?/page/??search=bar&page=2#fragment-123")
        );
    }

    // `%3F` decodes to `?`, not a digit, so only the segments holding digits are redacted, and the escape survives.
    #[test]
    fn path_digit_removal_keeps_other_escapes() {
        assert_eq!(
            obfuscate("http://foo.com/1%3F3/nam%3Fe/abcd9", false, true).as_deref(),
            Some("http://foo.com/?/nam%3Fe/?")
        );
        assert_eq!(
            obfuscate("http://foo.com/id/123/pa%3Fge/1?blabla", true, true).as_deref(),
            Some("http://foo.com/id/?/pa%3Fge/??")
        );
        assert_eq!(
            obfuscate("http://foo.com/id/123/pa%3Fge/1?blabla", true, false).as_deref(),
            Some("http://foo.com/id/123/pa%3Fge/1?")
        );
    }

    #[test]
    fn query_string_and_path_digits_removed() {
        assert_eq!(
            obfuscate(
                "https://user:pass@example.com/api/v2/users/456?token=secret",
                true,
                true
            )
            .as_deref(),
            Some("https://example.com/api/?/users/??")
        );
        assert_eq!(
            obfuscate("http://foo.com/name/id?query=search", true, true).as_deref(),
            Some("http://foo.com/name/id?")
        );
        assert_eq!(
            obfuscate("http://foo.com/id/123/page/1?search=bar&page=2#fragment", true, true).as_deref(),
            Some("http://foo.com/id/?/page/??#fragment")
        );
        assert_eq!(
            obfuscate("http://user:password@foo.com/1/2/3?q=james", true, true).as_deref(),
            Some("http://foo.com/?/?/??")
        );
    }

    // The rewrite normalizes escaping without finding anything sensitive, so it returns a string equal to its input and
    // the caller keeps the URL it already has.
    #[test]
    fn unchanged_rewrite_reports_no_change() {
        assert_eq!(obfuscate("http://foo.com/foo%20bar/", false, false), None);
        assert_eq!(obfuscate("http://foo.com/foo%20bar/", true, true), None);
    }

    // Known gap, tracked in https://github.com/DataDog/libdatadog/issues/2522: a percent-escaped unreserved character
    // in the path is decoded even when nothing sensitive was found, so these URLs are rewritten where the reference
    // implementation emits them byte-identical. Pinned so the upstream fix shows up here as a diff.
    #[test]
    fn pinned_gap_path_escapes_are_decoded() {
        assert_eq!(
            obfuscate("https://example.com/a/%2E%2E/b", false, false).as_deref(),
            Some("https://example.com/a/../b")
        );
        assert_eq!(
            obfuscate("https://example.com/%7Euser/index.html", false, false).as_deref(),
            Some("https://example.com/~user/index.html")
        );
        assert_eq!(
            obfuscate("https://example.com/path/%41BC/x", false, false).as_deref(),
            Some("https://example.com/path/ABC/x")
        );
        // Digit removal is off here, so the decoded `2` is revealed rather than redacted.
        assert_eq!(
            obfuscate("https://example.com/users/%32/profile", true, false).as_deref(),
            Some("https://example.com/users/2/profile")
        );
        // Same root cause on the authority: the reference implementation escapes only the path.
        assert_eq!(
            obfuscate("http://foo bar.com/x", false, false).as_deref(),
            Some("http://foo%20bar.com/x")
        );
    }

    // Known gap, tracked in https://github.com/DataDog/libdatadog/issues/2523: a `[` or `]` in the path or fragment is
    // not escaped before the parse it fails, so the whole URL collapses to `?` and the endpoint is lost. The reference
    // implementation keeps the endpoint and redacts only the digit segment. Pinned so the upstream fix is visible.
    #[test]
    fn pinned_gap_brackets_collapse_the_url() {
        let url = "https://example.com/api/items[1]/detail";
        assert_eq!(obfuscate(url, false, false), None);
        assert_eq!(obfuscate(url, true, false).as_deref(), Some("?"));
        assert_eq!(obfuscate(url, false, true).as_deref(), Some("?"));
        assert_eq!(obfuscate(url, true, true).as_deref(), Some("?"));
        assert_eq!(obfuscate("http://foo.com/x#a[b", true, false).as_deref(), Some("?"));
    }

    // A URL whose authority cannot be parsed is redacted wholesale as soon as either option is on, matching the
    // reference implementation. `libdd_trace_obfuscation::http::should_obfuscate_url` finds no trigger in these URLs
    // (https://github.com/DataDog/libdatadog/issues/2524), which is why nothing here screens with it.
    #[test]
    fn unparseable_authority_is_redacted() {
        for url in ["https://example.com:port/x", "http://foo:bar.com/x", ":"] {
            for (remove_query_string, remove_paths_with_digits) in [(true, false), (false, true), (true, true)] {
                assert_eq!(
                    obfuscate(url, remove_query_string, remove_paths_with_digits).as_deref(),
                    Some("?"),
                    "expected wholesale redaction for {url:?}"
                );
            }
        }
    }

    // Every URL reaches the rewrite now, including ones a byte scan would not flag, so pin that the rewrite still finds
    // what needs redacting in each of them.
    #[test]
    fn rewrite_changes_urls_that_carry_something_to_redact() {
        let cases = [
            ("http://user:password@foo.com/1/2/3?q=james", false, false),
            ("http://foo.com/users/%32/profile", false, true),
            ("/users/123/profile", false, true),
            ("http://foo.com/%", true, false),
            ("this is not a valid url", true, true),
        ];

        for (url, remove_query_string, remove_paths_with_digits) in cases {
            let config = config(remove_query_string, remove_paths_with_digits);
            assert!(obfuscate_url(url, &config).is_some(), "expected a change for {url:?}");
        }
    }

    #[test]
    fn rewrite_leaves_urls_with_nothing_to_redact() {
        assert_eq!(obfuscate("http://foo.com/p?q=1", false, false), None);
        assert_eq!(obfuscate("http://foo.com/path", false, false), None);
        assert_eq!(
            obfuscate("http://foo.com/p1", false, true).as_deref(),
            Some("http://foo.com/?")
        );
    }
}
