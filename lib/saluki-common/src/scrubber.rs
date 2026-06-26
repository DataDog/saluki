//! A YAML scrubber for redacting sensitive information.

use std::io::{BufRead, BufReader};
use std::sync::OnceLock;

use regex::bytes::Regex;

static COMMENT_REGEX: OnceLock<Regex> = OnceLock::new();
static BLANK_REGEX: OnceLock<Regex> = OnceLock::new();

fn comment_regex() -> &'static Regex {
    COMMENT_REGEX.get_or_init(|| Regex::new(r"^\s*#.*$").unwrap())
}

fn blank_regex() -> &'static Regex {
    BLANK_REGEX.get_or_init(|| Regex::new(r"^\s*$").unwrap())
}

type ReplFunc = Box<dyn Fn(&[u8]) -> Vec<u8> + Send + Sync>;

/// Defines a rule for scrubbing sensitive information.
pub struct Replacer {
    /// `regex` must match the sensitive information within a value.
    pub regex: Option<Regex>,

    /// `hints`, if given, are strings which must also be present in the text for the
    /// `regex` to match. This can be used to limit the contexts where an otherwise
    /// very broad `regex` is actually applied.
    pub hints: Option<Vec<String>>,

    /// `repl` is the byte slice to replace the substring matching `regex`. It can use
    /// the `regex` crate's replacement-string syntax (for example, `$1` to refer to the
    /// first capture group).
    pub repl: Option<Vec<u8>>,

    /// `repl_func`, if set, is called with the matched byte slice. The return value
    /// is used as the replacement. Only one of `repl` and `repl_func` should be set.
    pub repl_func: Option<ReplFunc>,
}

static DEFAULT_SCRUBBER: OnceLock<Scrubber> = OnceLock::new();

/// Returns a reference to the default, lazily initialized global scrubber.
///
/// This function ensures that the default scrubber, with its associated regex compilation,
/// is only initialized once for the lifetime of the application.
pub fn default_scrubber() -> &'static Scrubber {
    DEFAULT_SCRUBBER.get_or_init(Scrubber::default)
}

impl Default for Scrubber {
    fn default() -> Self {
        let hinted_api_key_replacer = Replacer {
            regex: Some(Regex::new(r"(api_?key=)[a-zA-Z0-9]+([a-zA-Z0-9]{5})\b").unwrap()),
            repl: Some(b"$1***************************$2".to_vec()),
            hints: Some(vec!["api_key".to_string(), "apikey".to_string()]),
            repl_func: None,
        };

        let hinted_app_key_replacer = Replacer {
            regex: Some(Regex::new(r"(ap(?:p|plication)_?key=)[a-zA-Z0-9]+([a-zA-Z0-9]{5})\b").unwrap()),
            repl: Some(b"$1***********************************$2".to_vec()),
            hints: Some(vec![
                "appkey".to_string(),
                "app_key".to_string(),
                "application_key".to_string(),
            ]),
            repl_func: None,
        };

        // Non-hinted API key replacer: matches 32 hex chars, keeps last 5
        let api_key_replacer = Replacer {
            regex: Some(Regex::new(r"\b[a-fA-F0-9]{27}([a-fA-F0-9]{5})\b").unwrap()),
            repl: Some(b"***************************$1".to_vec()),
            hints: None,
            repl_func: None,
        };

        // YAML-specific replacers that are aware of quotes and other syntax
        let api_key_replacer_yaml = Replacer {
            regex: Some(Regex::new(r#"(\-|\:|,|\[|\{)(\s+)?\b[a-fA-F0-9]{27}([a-fA-F0-9]{5})\b"#).unwrap()),
            repl: Some(b"$1$2\"***************************$3\"".to_vec()),
            hints: None,
            repl_func: None,
        };

        let app_key_replacer_yaml = Replacer {
            regex: Some(Regex::new(r#"(\-|\:|,|\[|\{)(\s+)?\b[a-fA-F0-9]{35}([a-fA-F0-9]{5})\b"#).unwrap()),
            repl: Some(b"$1$2\"***********************************$3\"".to_vec()),
            hints: None,
            repl_func: None,
        };

        let app_key_replacer = Replacer {
            regex: Some(Regex::new(r"\b[a-fA-F0-9]{35}([a-fA-F0-9]{5})\b").unwrap()),
            repl: Some(b"***********************************$1".to_vec()),
            hints: None,
            repl_func: None,
        };

        // Replacer for DDRCM App Key
        let rc_app_key_replacer = Replacer {
            regex: Some(Regex::new(r"\bDDRCM_[A-Z0-9]+([A-Z0-9]{5})\b").unwrap()),
            repl: Some(b"***********************************$1".to_vec()),
            hints: None,
            repl_func: None,
        };

        // Bearer token in the canonical 64-hex form (for example, an IPC/Cluster Agent auth token in an
        // `Authorization: Bearer <token>` header): mask the first 59 hex characters and keep the last 5 for
        // correlation. Runs before `bearer_catchall_replacer` so the masked output is left untouched by it.
        let bearer_hex_replacer = Replacer {
            regex: Some(Regex::new(r"\bBearer [a-fA-F0-9]{59}([a-fA-F0-9]{5})\b").unwrap()),
            repl: Some(b"Bearer ***********************************************************$1".to_vec()),
            hints: Some(vec!["Bearer".to_string()]),
            repl_func: None,
        };

        // Any other `Bearer <token>` value (arbitrary, non-hex). The token character class excludes `*`,
        // whitespace, and `"` so the match stops at the JSON string boundary and cannot span into adjacent
        // fields, keeping scrubbed JSON valid (and the `*` exclusion avoids re-matching the output of
        // `bearer_hex_replacer`). This is the JSON-safe equivalent of the upstream `\bBearer\s+[^*]+\b`.
        let bearer_catchall_replacer = Replacer {
            regex: Some(Regex::new(r#"\bBearer\s+[^*\s"]+"#).unwrap()),
            repl: Some(b"Bearer ********".to_vec()),
            hints: Some(vec!["Bearer".to_string()]),
            repl_func: None,
        };

        // Replacer for URI passwords (for example, protocol://user:password@host)
        let uri_password_replacer = Replacer {
            regex: Some(Regex::new(r#"(?i)([a-z][a-z0-9+-.]+://|\b)([^:\s]+):([^\s|"]+)@"#).unwrap()),
            repl: Some(b"$1$2:********@".to_vec()),
            hints: None,
            repl_func: None,
        };

        // Capture the optional closing `"` as $4 so the replacement preserves it for JSON values without breaking
        // unquoted values (plain text / YAML). Without $4, `"password":"secret"` → `"password":"********` (invalid JSON).
        // `:[ ]?` matches both compact JSON (`"password":"secret"`) and spaced YAML (`password: secret`).
        let password_replacer = Replacer {
            regex: Some(Regex::new(r#"(?i)(\"?(?:pass(?:word)?|pswd|pwd)\"?)((?:=| = |:[ ]?)\"?)([0-9A-Za-z#!$%&'()*+,\-./:;<=>?@\[\\\]^_{|}~]+)(\"?)"#).unwrap()),
            repl: Some(b"$1$2********$4".to_vec()),
            hints: None,
            repl_func: None,
        };

        // Redacts the value of any key ending in `token` or `jwt` (for example, `auth_token`,
        // `cluster_agent.auth_token`, `refresh_token`). Mirrors `password_replacer`: the trailing `"` in the
        // key group plus the `$4` closing-quote capture keep compact JSON (`"auth_token":"x"`) valid, and the
        // optional leading `"` lets the match start mid-key (so dotted keys like `cluster_agent.auth_token`
        // match after the `.`). `hints` is intentionally `None`: the hint check is case-sensitive, so a
        // `"token"` hint would skip an uppercase `AUTH_TOKEN` key that `(?i)` would otherwise match.
        let token_replacer = Replacer {
            regex: Some(Regex::new(r#"(?i)(\"?(?:[\w-]*(?:token|jwt))\"?)((?:=| = |:[ ]?)\"?)([0-9A-Za-z#!$%&'()*+,\-./:;<=>?@\[\\\]^_{|}~]+)(\"?)"#).unwrap()),
            repl: Some(b"$1$2********$4".to_vec()),
            hints: None,
            repl_func: None,
        };

        Self {
            replacers: vec![
                hinted_api_key_replacer,
                hinted_app_key_replacer,
                api_key_replacer_yaml,
                app_key_replacer_yaml,
                api_key_replacer,
                app_key_replacer,
                rc_app_key_replacer,
                bearer_hex_replacer,
                bearer_catchall_replacer,
                uri_password_replacer,
                password_replacer,
                token_replacer,
            ],
        }
    }
}

/// A YAML scrubber that can be configured with different replacers.
pub struct Scrubber {
    replacers: Vec<Replacer>,
}

impl Scrubber {
    /// Creates a new `Scrubber` with no replacers.
    pub fn new() -> Self {
        Self { replacers: vec![] }
    }

    /// Adds a replacer to the scrubber.
    pub fn add_replacer(&mut self, replacer: Replacer) {
        self.replacers.push(replacer);
    }

    /// Scrubs sensitive data from a byte slice.
    ///
    /// This method will scrub the data, returning a new byte vector.
    pub fn scrub_bytes(&self, data: &[u8]) -> Vec<u8> {
        let mut reader = BufReader::new(data);
        self.scrub_reader(&mut reader)
    }

    fn scrub_reader(&self, reader: &mut BufReader<&[u8]>) -> Vec<u8> {
        let mut scrubbed_lines = Vec::new();
        let mut line = Vec::new();
        let mut first = true;
        while let Ok(bytes_read) = reader.read_until(b'\n', &mut line) {
            if bytes_read == 0 {
                break; // EOF
            }

            if blank_regex().is_match(&line) {
                scrubbed_lines.push(b"\n".to_vec());
            } else if !comment_regex().is_match(&line) {
                let b = self.scrub(&line, &self.replacers);
                if !first {
                    scrubbed_lines.push(b"\n".to_vec());
                }
                scrubbed_lines.push(b);
                first = false;
            }
            line.clear();
        }
        scrubbed_lines.join(&b'\n')
    }

    /// Applies the replacers to the data.
    fn scrub(&self, data: &[u8], replacers: &[Replacer]) -> Vec<u8> {
        let mut scrubbed_data = data.to_vec();
        for replacer in replacers {
            if replacer.regex.is_none() {
                continue;
            }

            let contains_hint = if let Some(hints) = &replacer.hints {
                hints.iter().any(|hint| {
                    let needle = hint.as_bytes();
                    data.windows(needle.len()).any(|window| window == needle)
                })
            } else {
                false
            };

            if replacer.hints.as_ref().is_none_or(|h| h.is_empty() || contains_hint) {
                if let Some(re) = &replacer.regex {
                    if let Some(repl_func) = &replacer.repl_func {
                        scrubbed_data = re
                            .replace_all(&scrubbed_data, |caps: &regex::bytes::Captures| repl_func(&caps[0]))
                            .into_owned();
                    } else if let Some(repl) = &replacer.repl {
                        scrubbed_data = re.replace_all(&scrubbed_data, repl.as_slice()).into_owned();
                    }
                }
            }
        }
        scrubbed_data
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_clean(contents: &str, clean_contents: &str) {
        let scrubber = default_scrubber();
        let cleaned = scrubber.scrub_bytes(contents.as_bytes());
        let cleaned_string = String::from_utf8(cleaned).unwrap();
        assert_eq!(cleaned_string.trim(), clean_contents.trim());
    }

    #[test]
    fn test_config_strip_api_key() {
        assert_clean(
            "api_key: aaaaaaaaaaaaaaaaaaaaaaaaaaaabbbb",
            "api_key: \"***************************abbbb\"",
        );
        assert_clean(
            "api_key: AAAAAAAAAAAAAAAAAAAAAAAAAAAABBBB",
            "api_key: \"***************************ABBBB\"",
        );
        assert_clean(
            "api_key: aaaaaaaaaaaaaaaaaaaaaaaaaaaabbbb",
            "api_key: \"***************************abbbb\"",
        );
        assert_clean(
            "api_key: 'aaaaaaaaaaaaaaaaaaaaaaaaaaaabbbb'",
            "api_key: '***************************abbbb'",
        );
        assert_clean(
            "   api_key:   'aaaaaaaaaaaaaaaaaaaaaaaaaaaabbbb'   ",
            "   api_key:   '***************************abbbb'   ",
        );
    }

    #[test]
    fn test_config_app_key() {
        assert_clean(
            "app_key: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaabbbb",
            "app_key: \"***********************************abbbb\"",
        );
        assert_clean(
            "app_key: AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABBBB",
            "app_key: \"***********************************ABBBB\"",
        );
        assert_clean(
            "app_key: \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaabbbb\"",
            "app_key: \"***********************************abbbb\"",
        );
        assert_clean(
            "app_key: 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaabbbb'",
            "app_key: '***********************************abbbb'",
        );
        assert_clean(
            "   app_key:   'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaabbbb'   ",
            "   app_key:   '***********************************abbbb'   ",
        );
    }

    #[test]
    fn test_config_rc_app_key() {
        assert_clean(
            "key: \"DDRCM_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABCDE\"",
            "key: \"***********************************ABCDE\"",
        );
    }

    #[test]
    fn test_text_strip_api_key() {
        assert_clean(
            "Error status code 500 : http://dog.tld/api?key=3290abeefc68e1bbe852a25252bad88c",
            "Error status code 500 : http://dog.tld/api?key=***************************ad88c",
        );
        assert_clean(
            "hintedAPIKeyReplacer : http://dog.tld/api_key=InvalidLength12345abbbb",
            "hintedAPIKeyReplacer : http://dog.tld/api_key=***************************abbbb",
        );
        assert_clean(
            "hintedAPIKeyReplacer : http://dog.tld/apikey=InvalidLength12345abbbb",
            "hintedAPIKeyReplacer : http://dog.tld/apikey=***************************abbbb",
        );
        assert_clean(
            "apiKeyReplacer: https://agent-http-intake.logs.datadoghq.com/v1/input/aaaaaaaaaaaaaaaaaaaaaaaaaaaabbbb",
            "apiKeyReplacer: https://agent-http-intake.logs.datadoghq.com/v1/input/***************************abbbb",
        );
    }

    #[test]
    fn test_config_strip_url_password() {
        assert_clean(
            "proxy: random_url_key: http://user:password@host:port",
            "proxy: random_url_key: http://user:********@host:port",
        );
        assert_clean(
            "random_url_key http://user:password@host:port",
            "random_url_key http://user:********@host:port",
        );
        assert_clean(
            "random_url_key: http://user:password@host:port",
            "random_url_key: http://user:********@host:port",
        );
        assert_clean(
            "random_url_key: http://user:p@ssw0r)@host:port",
            "random_url_key: http://user:********@host:port",
        );
        assert_clean(
            "random_url_key: http://user:🔑🔒🔐🔓@host:port",
            "random_url_key: http://user:********@host:port",
        );
        assert_clean(
            "random_url_key: http://user:password@host",
            "random_url_key: http://user:********@host",
        );
        assert_clean(
            "random_url_key: protocol://user:p@ssw0r)@host:port",
            "random_url_key: protocol://user:********@host:port",
        );
        assert_clean(
            "random_url_key: \"http://user:password@host:port\"",
            "random_url_key: \"http://user:********@host:port\"",
        );
        assert_clean(
            "random_url_key: 'http://user:password@host:port'",
            "random_url_key: 'http://user:********@host:port'",
        );
        assert_clean(
            "random_domain_key: 'user:password@host:port'",
            "random_domain_key: 'user:********@host:port'",
        );
        assert_clean(
            "   random_url_key:   'http://user:password@host:port'   ",
            "   random_url_key:   'http://user:********@host:port'   ",
        );
        assert_clean(
            "   random_url_key:   'mongodb+s.r-v://user:password@host:port'   ",
            "   random_url_key:   'mongodb+s.r-v://user:********@host:port'   ",
        );
        assert_clean(
            "   random_url_key:   'mongodb+srv://user:pass-with-hyphen@abc.example.com/database'   ",
            "   random_url_key:   'mongodb+srv://user:********@abc.example.com/database'   ",
        );
    }

    #[test]
    fn test_password_yaml_double_quoted_value() {
        assert_clean("password: \"supersecret\"", "password: \"********\"");
    }

    #[test]
    fn test_password_unquoted_value_still_scrubbed() {
        assert_clean("password=supersecret", "password=********");
        assert_clean("password: supersecret", "password: ********");
    }

    #[test]
    fn test_json_password_like_key_scrubs_to_valid_json() {
        let scrubber = default_scrubber();
        // spaced (pretty-printed JSON / YAML)
        let input = r#"{"mysql_password": "supersecret"}"#;
        let cleaned = String::from_utf8(scrubber.scrub_bytes(input.as_bytes())).unwrap();
        serde_json::from_str::<serde_json::Value>(&cleaned).expect("scrubbed JSON must parse");
        assert!(cleaned.contains("********"));

        // compact JSON (no space after colon)
        let input_compact = r#"{"password":"secret"}"#;
        let cleaned_compact = String::from_utf8(scrubber.scrub_bytes(input_compact.as_bytes())).unwrap();
        serde_json::from_str::<serde_json::Value>(&cleaned_compact).expect("compact scrubbed JSON must parse");
        assert!(
            cleaned_compact.contains("********"),
            "compact JSON password must be scrubbed: {cleaned_compact}"
        );
    }

    #[test]
    fn test_json_single_line_api_key_scrub() {
        let scrubber = default_scrubber();
        let input = r#"{"api_key":"aaaaaaaaaaaaaaaaaaaaaaaaaaaabbbb"}"#;
        let cleaned = scrubber.scrub_bytes(input.as_bytes());
        let cleaned_string = String::from_utf8(cleaned).unwrap();
        // Must remain valid JSON after scrubbing (regex YAML-style replacers must not corrupt JSON).
        serde_json::from_str::<serde_json::Value>(&cleaned_string).expect("scrubbed output must parse as JSON");
        assert!(
            cleaned_string.contains("***************************"),
            "expected masked api key suffix, got: {cleaned_string}"
        );
    }

    #[test]
    fn test_large_single_line_json_scrubbed_still_parses() {
        let mut map = serde_json::Map::new();
        map.insert("api_key".into(), serde_json::json!("aaaaaaaaaaaaaaaaaaaaaaaaaaaabbbb"));
        map.insert("pad".into(), serde_json::json!("x".repeat(25_000)));
        let line = serde_json::to_string(&serde_json::Value::Object(map)).unwrap();
        assert!(line.len() > 16_384, "sanity: payload should exceed 16 KiB");

        let scrubber = default_scrubber();
        let cleaned = scrubber.scrub_bytes(line.as_bytes());
        let cleaned_string = String::from_utf8(cleaned).unwrap();
        serde_json::from_str::<serde_json::Value>(&cleaned_string).expect("JSON parse after scrub");
    }

    #[test]
    fn test_text_strip_app_key() {
        assert_clean(
            "hintedAPPKeyReplacer : http://dog.tld/app_key=InvalidLength12345abbbb",
            "hintedAPPKeyReplacer : http://dog.tld/app_key=***********************************abbbb",
        );
        assert_clean(
            "hintedAPPKeyReplacer : http://dog.tld/appkey=InvalidLength12345abbbb",
            "hintedAPPKeyReplacer : http://dog.tld/appkey=***********************************abbbb",
        );
        assert_clean(
            "hintedAPPKeyReplacer : http://dog.tld/application_key=InvalidLength12345abbbb",
            "hintedAPPKeyReplacer : http://dog.tld/application_key=***********************************abbbb",
        );
        assert_clean(
            "appKeyReplacer: http://dog.tld/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaabbbb",
            "appKeyReplacer: http://dog.tld/***********************************abbbb",
        );
    }

    #[test]
    fn test_config_strip_auth_token() {
        assert_clean("auth_token: secret", "auth_token: ********");
        assert_clean("auth_token=secret", "auth_token=********");
        assert_clean("   auth_token: cluster-agent-token", "   auth_token: ********");
        // Flat dotted key: the prefix before the final `.` is preserved, only the value is masked.
        assert_clean(
            "cluster_agent.auth_token: cluster-agent-token",
            "cluster_agent.auth_token: ********",
        );
        // Any key ending in `token`/`jwt`, quoted or not.
        assert_clean("refresh_token: abc123", "refresh_token: ********");
        assert_clean("jwt: eyJhbGci.payload", "jwt: ********");
        assert_clean("auth_token: \"secret\"", "auth_token: \"********\"");
    }

    #[test]
    fn test_json_auth_token_scrubs_to_valid_json() {
        // The reported scenario: `cluster_agent.auth_token` in the JSON the `config` CLI re-parses.
        let scrubber = default_scrubber();
        let input = r#"{"cluster_agent.auth_token":"cluster-agent-token"}"#;
        let cleaned = String::from_utf8(scrubber.scrub_bytes(input.as_bytes())).unwrap();
        serde_json::from_str::<serde_json::Value>(&cleaned).expect("scrubbed JSON must parse");
        assert!(cleaned.contains("********"), "auth_token must be masked: {cleaned}");
        assert!(
            !cleaned.contains("cluster-agent-token"),
            "raw token must not remain: {cleaned}"
        );
    }

    #[test]
    fn test_token_replacer_no_false_positive() {
        // Keys that merely contain `token` but do not end in `token`/`jwt` are left untouched.
        assert_clean("tokenizer: enabled", "tokenizer: enabled");
        assert_clean("max_tokens: 100", "max_tokens: 100");
        assert_clean("token_expiry: 3600", "token_expiry: 3600");
    }

    #[test]
    fn test_strip_bearer_token() {
        // Canonical 64-hex bearer token: first 59 characters masked, last 5 kept for correlation.
        let token = format!("{}bcdef", "a".repeat(59));
        let masked = format!("{}bcdef", "*".repeat(59));
        assert_clean(
            &format!("Authorization: Bearer {token}"),
            &format!("Authorization: Bearer {masked}"),
        );

        // Arbitrary (non-hex) bearer token: fully masked.
        assert_clean(
            "Authorization: Bearer my-arbitrary-token-value",
            "Authorization: Bearer ********",
        );
    }

    #[test]
    fn test_bearer_token_in_json_stays_valid() {
        // A `Bearer <token>` embedded in compact JSON must mask only the token, not span into adjacent fields.
        let scrubber = default_scrubber();
        let input = r#"{"authorization":"Bearer my-arbitrary-token-value","keep":"value"}"#;
        let cleaned = String::from_utf8(scrubber.scrub_bytes(input.as_bytes())).unwrap();
        serde_json::from_str::<serde_json::Value>(&cleaned).expect("scrubbed JSON must parse");
        assert!(
            cleaned.contains("Bearer ********"),
            "bearer token must be masked: {cleaned}"
        );
        assert!(
            cleaned.contains(r#""keep":"value""#),
            "adjacent field must survive: {cleaned}"
        );
    }
}
