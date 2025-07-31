//! A YAML scrubber for redacting sensitive information.

use std::sync::OnceLock;

use regex::bytes::Regex;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;

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
    /// the `regex` crate's replacement-string syntax (e.g., `$1` to refer to the
    /// first capture group).
    pub repl: Option<Vec<u8>>,

    /// `repl_func`, if set, is called with the matched byte slice. The return value
    /// is used as the replacement. Only one of `repl` and `repl_func` should be set.
    pub repl_func: Option<ReplFunc>,
}

static DEFAULT_SCRUBBER: OnceLock<Scrubber> = OnceLock::new();

/// Returns a reference to the default, lazily-initialized global scrubber.
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

        // Replacer for URI passwords (e.g., protocol://user:password@host)
        let uri_password_replacer = Replacer {
            regex: Some(Regex::new(r#"(?i)([a-z][a-z0-9+-.]+://|\b)([^:\s]+):([^\s|"]+)@"#).unwrap()),
            repl: Some(b"$1$2:********@".to_vec()),
            hints: None,
            repl_func: None,
        };

        let password_replacer = Replacer {
            regex: Some(Regex::new(r#"(?i)(\"?(?:pass(?:word)?|pswd|pwd)\"?)((?:=| = |: )\"?)([0-9A-Za-z#!$%&'()*+,\-./:;<=>?@\[\\\]^_{|}~]+)"#).unwrap()),
            repl: Some(b"$1$2********".to_vec()),
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
                uri_password_replacer,
                password_replacer,
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
    pub async fn scrub_bytes(&self, data: &[u8]) -> Vec<u8> {
        let mut reader = BufReader::new(data);
        self.scrub_reader(&mut reader).await
    }

    async fn scrub_reader(&self, reader: &mut BufReader<&[u8]>) -> Vec<u8> {
        let mut scrubbed_lines = Vec::new();
        let mut line = Vec::new();
        let mut first = true;
        while let Ok(bytes_read) = reader.read_until(b'\n', &mut line).await {
            if bytes_read == 0 {
                break; // EOF
            }

            if blank_regex().is_match(&line) {
                scrubbed_lines.push(b"\n".to_vec());
            } else if !comment_regex().is_match(&line) {
                let b = self.scrub(&line, &self.replacers).await;
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
    async fn scrub(&self, data: &[u8], replacers: &[Replacer]) -> Vec<u8> {
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

    async fn assert_clean(contents: &str, clean_contents: &str) {
        let scrubber = default_scrubber();
        let cleaned = scrubber.scrub_bytes(contents.as_bytes()).await;
        let cleaned_string = String::from_utf8(cleaned).unwrap();
        assert_eq!(cleaned_string.trim(), clean_contents.trim());
    }

    #[tokio::test]
    async fn test_config_strip_api_key() {
        assert_clean(
            "api_key: aaaaaaaaaaaaaaaaaaaaaaaaaaaabbbb",
            "api_key: \"***************************abbbb\"",
        )
        .await;
        assert_clean(
            "api_key: AAAAAAAAAAAAAAAAAAAAAAAAAAAABBBB",
            "api_key: \"***************************ABBBB\"",
        )
        .await;
        assert_clean(
            "api_key: aaaaaaaaaaaaaaaaaaaaaaaaaaaabbbb",
            "api_key: \"***************************abbbb\"",
        )
        .await;
        assert_clean(
            "api_key: 'aaaaaaaaaaaaaaaaaaaaaaaaaaaabbbb'",
            "api_key: '***************************abbbb'",
        )
        .await;
        assert_clean(
            "   api_key:   'aaaaaaaaaaaaaaaaaaaaaaaaaaaabbbb'   ",
            "   api_key:   '***************************abbbb'   ",
        )
        .await;
    }

    #[tokio::test]
    async fn test_config_app_key() {
        assert_clean(
            "app_key: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaabbbb",
            "app_key: \"***********************************abbbb\"",
        )
        .await;
        assert_clean(
            "app_key: AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABBBB",
            "app_key: \"***********************************ABBBB\"",
        )
        .await;
        assert_clean(
            "app_key: \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaabbbb\"",
            "app_key: \"***********************************abbbb\"",
        )
        .await;
        assert_clean(
            "app_key: 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaabbbb'",
            "app_key: '***********************************abbbb'",
        )
        .await;
        assert_clean(
            "   app_key:   'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaabbbb'   ",
            "   app_key:   '***********************************abbbb'   ",
        )
        .await;
    }

    #[tokio::test]
    async fn test_config_rc_app_key() {
        assert_clean(
            "key: \"DDRCM_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABCDE\"",
            "key: \"***********************************ABCDE\"",
        )
        .await;
    }

    #[tokio::test]
    async fn test_text_strip_api_key() {
        assert_clean(
            "Error status code 500 : http://dog.tld/api?key=3290abeefc68e1bbe852a25252bad88c",
            "Error status code 500 : http://dog.tld/api?key=***************************ad88c",
        )
        .await;
        assert_clean(
            "hintedAPIKeyReplacer : http://dog.tld/api_key=InvalidLength12345abbbb",
            "hintedAPIKeyReplacer : http://dog.tld/api_key=***************************abbbb",
        )
        .await;
        assert_clean(
            "hintedAPIKeyReplacer : http://dog.tld/apikey=InvalidLength12345abbbb",
            "hintedAPIKeyReplacer : http://dog.tld/apikey=***************************abbbb",
        )
        .await;
        assert_clean(
            "apiKeyReplacer: https://agent-http-intake.logs.datadoghq.com/v1/input/aaaaaaaaaaaaaaaaaaaaaaaaaaaabbbb",
            "apiKeyReplacer: https://agent-http-intake.logs.datadoghq.com/v1/input/***************************abbbb",
        )
        .await;
    }

    #[tokio::test]
    async fn test_config_strip_url_password() {
        assert_clean(
            "proxy: random_url_key: http://user:password@host:port",
            "proxy: random_url_key: http://user:********@host:port",
        )
        .await;
        assert_clean(
            "random_url_key http://user:password@host:port",
            "random_url_key http://user:********@host:port",
        )
        .await;
        assert_clean(
            "random_url_key: http://user:password@host:port",
            "random_url_key: http://user:********@host:port",
        )
        .await;
        assert_clean(
            "random_url_key: http://user:p@ssw0r)@host:port",
            "random_url_key: http://user:********@host:port",
        )
        .await;
        assert_clean(
            "random_url_key: http://user:🔑🔒🔐🔓@host:port",
            "random_url_key: http://user:********@host:port",
        )
        .await;
        assert_clean(
            "random_url_key: http://user:password@host",
            "random_url_key: http://user:********@host",
        )
        .await;
        assert_clean(
            "random_url_key: protocol://user:p@ssw0r)@host:port",
            "random_url_key: protocol://user:********@host:port",
        )
        .await;
        assert_clean(
            "random_url_key: \"http://user:password@host:port\"",
            "random_url_key: \"http://user:********@host:port\"",
        )
        .await;
        assert_clean(
            "random_url_key: 'http://user:password@host:port'",
            "random_url_key: 'http://user:********@host:port'",
        )
        .await;
        assert_clean(
            "random_domain_key: 'user:password@host:port'",
            "random_domain_key: 'user:********@host:port'",
        )
        .await;
        assert_clean(
            "   random_url_key:   'http://user:password@host:port'   ",
            "   random_url_key:   'http://user:********@host:port'   ",
        )
        .await;
        assert_clean(
            "   random_url_key:   'mongodb+s.r-v://user:password@host:port'   ",
            "   random_url_key:   'mongodb+s.r-v://user:********@host:port'   ",
        )
        .await;
        assert_clean(
            "   random_url_key:   'mongodb+srv://user:pass-with-hyphen@abc.example.com/database'   ",
            "   random_url_key:   'mongodb+srv://user:********@abc.example.com/database'   ",
        )
        .await;
    }

    #[tokio::test]
    async fn test_text_strip_app_key() {
        assert_clean(
            "hintedAPPKeyReplacer : http://dog.tld/app_key=InvalidLength12345abbbb",
            "hintedAPPKeyReplacer : http://dog.tld/app_key=***********************************abbbb",
        )
        .await;
        assert_clean(
            "hintedAPPKeyReplacer : http://dog.tld/appkey=InvalidLength12345abbbb",
            "hintedAPPKeyReplacer : http://dog.tld/appkey=***********************************abbbb",
        )
        .await;
        assert_clean(
            "hintedAPPKeyReplacer : http://dog.tld/application_key=InvalidLength12345abbbb",
            "hintedAPPKeyReplacer : http://dog.tld/application_key=***********************************abbbb",
        )
        .await;
        assert_clean(
            "appKeyReplacer: http://dog.tld/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaabbbb",
            "appKeyReplacer: http://dog.tld/***********************************abbbb",
        )
        .await;
    }
}
