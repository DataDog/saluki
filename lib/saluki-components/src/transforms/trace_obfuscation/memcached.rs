//! Memcached command obfuscation.

use stringtheory::MetaString;

use super::obfuscator::MemcachedObfuscationConfig;

/// Obfuscates a Memcached command by removing key values.
/// Returns `Some("")` to signal tag removal, `Some(value)` to replace, `None` if unchanged.
pub fn obfuscate_memcached_command(cmd: &str, config: &MemcachedObfuscationConfig) -> Option<MetaString> {
    if !config.keep_command() {
        return Some("".into());
    }

    let truncated = cmd.split("\r\n").next().unwrap_or("");
    let trimmed = truncated.trim();

    if trimmed == cmd {
        None
    } else {
        Some(trimmed.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> MemcachedObfuscationConfig {
        MemcachedObfuscationConfig {
            enabled: true,
            keep_command: true,
        }
    }

    #[test]
    fn test_set_with_value_keep_true() {
        let config = default_config();
        let result = obfuscate_memcached_command("set mykey 0 60 5\r\nvalue", &config);
        assert_eq!(result.unwrap().as_ref(), "set mykey 0 60 5");
    }

    #[test]
    fn test_get_keep_true() {
        let config = default_config();
        let result = obfuscate_memcached_command("get mykey", &config);
        // Already clean, no change needed
        assert!(result.is_none());
    }

    #[test]
    fn test_add_with_value_keep_true() {
        let config = default_config();
        let result = obfuscate_memcached_command("add newkey 0 60 5\r\nvalue", &config);
        assert_eq!(result.unwrap().as_ref(), "add newkey 0 60 5");
    }

    #[test]
    fn test_decr_keep_true() {
        let config = default_config();
        let result = obfuscate_memcached_command("decr mykey 5", &config);
        // Already clean, no change needed
        assert!(result.is_none());
    }

    #[test]
    fn test_set_with_value_keep_false() {
        let config = MemcachedObfuscationConfig {
            enabled: true,
            keep_command: false,
        };
        let result = obfuscate_memcached_command("set mykey 0 60 5\r\nvalue", &config);
        assert_eq!(result.unwrap().as_ref(), "");
    }

    #[test]
    fn test_get_keep_false() {
        let config = MemcachedObfuscationConfig {
            enabled: true,
            keep_command: false,
        };
        let result = obfuscate_memcached_command("get mykey", &config);
        assert_eq!(result.unwrap().as_ref(), "");
    }

    #[test]
    fn test_invalid_get_no_key() {
        let config = MemcachedObfuscationConfig {
            enabled: true,
            keep_command: false,
        };
        let result = obfuscate_memcached_command("get", &config);
        assert_eq!(result.unwrap().as_ref(), "");
    }

    #[test]
    fn test_invalid_get_with_value() {
        let config = MemcachedObfuscationConfig {
            enabled: true,
            keep_command: false,
        };
        let result = obfuscate_memcached_command("get\r\nvalue", &config);
        assert_eq!(result.unwrap().as_ref(), "");
    }
}
