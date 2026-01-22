//! HTTP URL obfuscation.

use url::Url;

use super::obfuscator::HttpObfuscationConfig;

/// Obfuscates a URL string by removing userinfo, query strings, and/or path digits.
pub fn obfuscate_url(val: &str, config: &HttpObfuscationConfig) -> String {
    let mut url = match Url::parse(val) {
        Ok(u) => u,
        Err(_) => {
            if config.remove_query_string() || config.remove_path_digits() {
                return "?".to_string();
            }
            return obfuscate_userinfo_fallback(val);
        }
    };

    if url.username() != "" || url.password().is_some() {
        let _ = url.set_username("");
        let _ = url.set_password(None);
    }

    if config.remove_query_string() && url.query().is_some() {
        url.set_query(Some(""));
    }

    if config.remove_path_digits() {
        let obfuscated_path = obfuscate_path_digits(url.path());
        url.set_path(&obfuscated_path);
    }

    url.to_string().replace("REDACTED", "?")
}

fn obfuscate_userinfo_fallback(val: &str) -> String {
    if let Some(at_pos) = val.find('@') {
        if let Some(slash_pos) = val.find('/') {
            if at_pos < slash_pos {
                if let Some(scheme_end) = val.find("://") {
                    return format!("{}{}", &val[..scheme_end + 3], &val[at_pos + 1..]);
                }
            }
        }
    }
    val.to_string()
}

fn obfuscate_path_digits(path: &str) -> String {
    let segments: Vec<&str> = path.split('/').collect();
    let obfuscated: Vec<String> = segments
        .iter()
        .map(|seg| {
            if has_non_encoded_digit(seg) {
                "REDACTED".to_string()
            } else {
                seg.to_string()
            }
        })
        .collect();
    obfuscated.join("/")
}

fn has_non_encoded_digit(seg: &str) -> bool {
    let bytes = seg.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            i += 3;
        } else if bytes[i].is_ascii_digit() {
            return true;
        } else {
            i += 1;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> HttpObfuscationConfig {
        HttpObfuscationConfig {
            remove_query_string: false,
            remove_path_digits: false,
        }
    }

    #[test]
    fn test_obfuscate_url_removes_userinfo() {
        let config = default_config();
        let result = obfuscate_url("https://user:pass@example.com/path", &config);
        assert_eq!(result, "https://example.com/path");
    }

    #[test]
    fn test_obfuscate_url_removes_query_string() {
        let config = HttpObfuscationConfig {
            remove_query_string: true,
            remove_path_digits: false,
        };
        let result = obfuscate_url("https://example.com/path?secret=value&key=data", &config);
        assert_eq!(result, "https://example.com/path?");
    }

    #[test]
    fn test_obfuscate_url_removes_path_digits() {
        let config = HttpObfuscationConfig {
            remove_query_string: false,
            remove_path_digits: true,
        };
        let result = obfuscate_url("https://example.com/users/123/profile", &config);
        assert_eq!(result, "https://example.com/users/?/profile");
    }

    #[test]
    fn test_obfuscate_url_combined() {
        let config = HttpObfuscationConfig {
            remove_query_string: true,
            remove_path_digits: true,
        };
        let result = obfuscate_url("https://user:pass@example.com/api/v2/users/456?token=secret", &config);
        assert_eq!(result, "https://example.com/api/?/users/??");
    }

    #[test]
    fn test_obfuscate_url_invalid_returns_question_mark() {
        let config = HttpObfuscationConfig {
            remove_query_string: true,
            remove_path_digits: false,
        };
        let result = obfuscate_url("not a valid url", &config);
        assert_eq!(result, "?");
    }

    #[test]
    fn test_url_encoding_behavior() {
        let mut url = Url::parse("https://example.com/users/123").unwrap();
        url.set_path("/users/?/profile");
        assert!(url.to_string().contains("%3F"));
        assert!(!url.to_string().contains("/users/?/"));
    }

    #[test]
    fn test_obfuscate_path_digits() {
        let result = obfuscate_path_digits("/users/123/profile");
        assert_eq!(result, "/users/REDACTED/profile");

        let result = obfuscate_path_digits("/api/v2/users");
        assert_eq!(result, "/api/REDACTED/users");

        let result = obfuscate_path_digits("/no/digits/here");
        assert_eq!(result, "/no/digits/here");
    }

    #[test]
    fn test_disabled_no_config() {
        let config = default_config();
        let result = obfuscate_url("http://foo.com/1/2/3?q=james", &config);
        assert_eq!(result, "http://foo.com/1/2/3?q=james");
    }

    #[test]
    fn test_disabled_userinfo_always_removed() {
        let config = default_config();
        let result = obfuscate_url("http://user:password@foo.com/1/2/3?q=james", &config);
        assert_eq!(result, "http://foo.com/1/2/3?q=james");
    }

    // Query string removal tests
    #[test]
    fn test_query_root_path() {
        let config = HttpObfuscationConfig {
            remove_query_string: true,
            remove_path_digits: false,
        };
        let result = obfuscate_url("http://foo.com/", &config);
        assert_eq!(result, "http://foo.com/");
    }

    #[test]
    fn test_query_path_with_digits_unchanged() {
        let config = HttpObfuscationConfig {
            remove_query_string: true,
            remove_path_digits: false,
        };
        let result = obfuscate_url("http://foo.com/123", &config);
        assert_eq!(result, "http://foo.com/123");
    }

    #[test]
    fn test_query_removed_basic() {
        let config = HttpObfuscationConfig {
            remove_query_string: true,
            remove_path_digits: false,
        };
        let result = obfuscate_url("http://foo.com/id/123/page/1?search=bar&page=2", &config);
        assert_eq!(result, "http://foo.com/id/123/page/1?");
    }

    #[test]
    fn test_query_removed_with_fragment() {
        let config = HttpObfuscationConfig {
            remove_query_string: true,
            remove_path_digits: false,
        };
        let result = obfuscate_url("http://foo.com/id/123/page/1?search=bar&page=2#fragment", &config);
        assert_eq!(result, "http://foo.com/id/123/page/1?#fragment");
    }

    #[test]
    fn test_query_removed_no_key_value() {
        let config = HttpObfuscationConfig {
            remove_query_string: true,
            remove_path_digits: false,
        };
        let result = obfuscate_url("http://foo.com/id/123/page/1?blabla", &config);
        assert_eq!(result, "http://foo.com/id/123/page/1?");
    }

    #[test]
    fn test_query_percent_encoded_question_preserved() {
        let config = HttpObfuscationConfig {
            remove_query_string: true,
            remove_path_digits: false,
        };
        let result = obfuscate_url("http://foo.com/id/123/pa%3Fge/1?blabla", &config);
        assert_eq!(result, "http://foo.com/id/123/pa%3Fge/1?");
    }

    #[test]
    fn test_query_userinfo_and_query_removed() {
        let config = HttpObfuscationConfig {
            remove_query_string: true,
            remove_path_digits: false,
        };
        let result = obfuscate_url("http://user:password@foo.com/1/2/3?q=james", &config);
        assert_eq!(result, "http://foo.com/1/2/3?");
    }

    // Path digits removal tests
    #[test]
    fn test_digits_root_unchanged() {
        let config = HttpObfuscationConfig {
            remove_query_string: false,
            remove_path_digits: true,
        };
        let result = obfuscate_url("http://foo.com/", &config);
        assert_eq!(result, "http://foo.com/");
    }

    #[test]
    fn test_digits_no_digits_unchanged() {
        let config = HttpObfuscationConfig {
            remove_query_string: false,
            remove_path_digits: true,
        };
        let result = obfuscate_url("http://foo.com/name?query=search", &config);
        assert_eq!(result, "http://foo.com/name?query=search");
    }

    #[test]
    fn test_digits_path_obfuscated_query_preserved() {
        let config = HttpObfuscationConfig {
            remove_query_string: false,
            remove_path_digits: true,
        };
        let result = obfuscate_url("http://foo.com/id/123/page/1?search=bar&page=2", &config);
        assert_eq!(result, "http://foo.com/id/?/page/??search=bar&page=2");
    }

    #[test]
    fn test_digits_mixed_alphanumeric_with_fragment() {
        let config = HttpObfuscationConfig {
            remove_query_string: false,
            remove_path_digits: true,
        };
        let result = obfuscate_url(
            "http://foo.com/id/a1/page/1qwe233?search=bar&page=2#fragment-123",
            &config,
        );
        assert_eq!(result, "http://foo.com/id/?/page/??search=bar&page=2#fragment-123");
    }

    #[test]
    fn test_digits_all_digit_segment() {
        let config = HttpObfuscationConfig {
            remove_query_string: false,
            remove_path_digits: true,
        };
        let result = obfuscate_url("http://foo.com/123", &config);
        assert_eq!(result, "http://foo.com/?");
    }

    #[test]
    fn test_digits_multiple_digit_segments() {
        let config = HttpObfuscationConfig {
            remove_query_string: false,
            remove_path_digits: true,
        };
        let result = obfuscate_url("http://foo.com/123/abcd9", &config);
        assert_eq!(result, "http://foo.com/?/?");
    }

    #[test]
    fn test_digits_between_non_digits() {
        let config = HttpObfuscationConfig {
            remove_query_string: false,
            remove_path_digits: true,
        };
        let result = obfuscate_url("http://foo.com/123/name/abcd9", &config);
        assert_eq!(result, "http://foo.com/?/name/?");
    }

    #[test]
    fn test_digits_percent_encoded_preserved() {
        let config = HttpObfuscationConfig {
            remove_query_string: false,
            remove_path_digits: true,
        };
        let result = obfuscate_url("http://foo.com/1%3F3/nam%3Fe/abcd9", &config);
        assert_eq!(result, "http://foo.com/?/nam%3Fe/?");
    }

    #[test]
    fn test_digits_userinfo_removed() {
        let config = HttpObfuscationConfig {
            remove_query_string: false,
            remove_path_digits: true,
        };
        let result = obfuscate_url("http://user:password@foo.com/1/2/3?q=james", &config);
        assert_eq!(result, "http://foo.com/?/?/??q=james");
    }

    // Both query and digits removal tests
    #[test]
    fn test_both_root_unchanged() {
        let config = HttpObfuscationConfig {
            remove_query_string: true,
            remove_path_digits: true,
        };
        let result = obfuscate_url("http://foo.com/", &config);
        assert_eq!(result, "http://foo.com/");
    }

    #[test]
    fn test_both_no_digits_no_query() {
        let config = HttpObfuscationConfig {
            remove_query_string: true,
            remove_path_digits: true,
        };
        let result = obfuscate_url("http://foo.com/name/id", &config);
        assert_eq!(result, "http://foo.com/name/id");
    }

    #[test]
    fn test_both_no_digits_query_removed() {
        let config = HttpObfuscationConfig {
            remove_query_string: true,
            remove_path_digits: true,
        };
        let result = obfuscate_url("http://foo.com/name/id?query=search", &config);
        assert_eq!(result, "http://foo.com/name/id?");
    }

    #[test]
    fn test_both_digits_and_query_removed() {
        let config = HttpObfuscationConfig {
            remove_query_string: true,
            remove_path_digits: true,
        };
        let result = obfuscate_url("http://foo.com/id/123/page/1?search=bar&page=2", &config);
        assert_eq!(result, "http://foo.com/id/?/page/??");
    }

    #[test]
    fn test_both_with_fragment() {
        let config = HttpObfuscationConfig {
            remove_query_string: true,
            remove_path_digits: true,
        };
        let result = obfuscate_url("http://foo.com/id/123/page/1?search=bar&page=2#fragment", &config);
        assert_eq!(result, "http://foo.com/id/?/page/??#fragment");
    }

    #[test]
    fn test_both_percent_encoded_digits() {
        let config = HttpObfuscationConfig {
            remove_query_string: true,
            remove_path_digits: true,
        };
        let result = obfuscate_url("http://foo.com/1%3F3/nam%3Fe/abcd9", &config);
        assert_eq!(result, "http://foo.com/?/nam%3Fe/?");
    }

    #[test]
    fn test_both_percent_encoded_with_query() {
        let config = HttpObfuscationConfig {
            remove_query_string: true,
            remove_path_digits: true,
        };
        let result = obfuscate_url("http://foo.com/id/123/pa%3Fge/1?blabla", &config);
        assert_eq!(result, "http://foo.com/id/?/pa%3Fge/??");
    }

    #[test]
    fn test_both_userinfo_digits_query() {
        let config = HttpObfuscationConfig {
            remove_query_string: true,
            remove_path_digits: true,
        };
        let result = obfuscate_url("http://user:password@foo.com/1/2/3?q=james", &config);
        assert_eq!(result, "http://foo.com/?/?/??");
    }
}
