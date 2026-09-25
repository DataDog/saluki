//! Credit card number obfuscation.

use libdd_trace_obfuscation::credit_cards::is_card_number;
use saluki_common::collections::FastHashSet;
use stringtheory::MetaString;

use super::obfuscator::CreditCardObfuscationConfig;

/// Value written over an attribute that looks like a credit card number.
pub const CREDIT_CARD_REPLACEMENT: &str = "?";

/// Allowlist of tag keys that are known to never contain credit card numbers.
const ALLOWLISTED_TAGS: &[&str] = &[
    "_sample_rate",
    "_sampling_priority_v1",
    "account_id",
    "aws_account",
    "error",
    "error.msg",
    "error.type",
    "error.stack",
    "env",
    "graphql.field",
    "graphql.query",
    "graphql.type",
    "graphql.operation.name",
    "grpc.code",
    "grpc.method",
    "grpc.request",
    "http.status_code",
    "http.method",
    "runtime-id",
    "out.host",
    "out.port",
    "sampling.priority",
    "span.type",
    "span.name",
    "service.name",
    "service",
    "sql.query",
    "version",
    // Data Job Monitoring tags. These values are frequently similar to credit card numbers.
    "databricks_job_id",
    "databricks_job_run_id",
    "databricks_task_run_id",
    "config.spark_app_startTime",
    "config.spark_databricks_job_parentRunId",
];

/// Credit card obfuscator with configuration.
pub struct CreditCardObfuscator {
    luhn: bool,
    keep_values: FastHashSet<MetaString>,
}

impl CreditCardObfuscator {
    /// Creates a new credit card obfuscator from configuration.
    pub fn new(config: &CreditCardObfuscationConfig) -> Self {
        // Only store user-provided keep_values.
        // Static allowlist is checked separately via `ALLOWLISTED_TAGS.contains()`.
        let keep_values: FastHashSet<MetaString> = config
            .keep_values
            .iter()
            .map(|s| MetaString::from(s.as_str()))
            .collect();

        Self {
            luhn: config.luhn,
            keep_values,
        }
    }

    /// Returns `true` when the value carried under `key` is subject to card scrubbing.
    ///
    /// Keys prefixed with `_`, keys on the static allowlist, and keys listed in the configured
    /// `keep_values` are exempt.
    pub fn should_obfuscate_key(&self, key: &str) -> bool {
        if key.starts_with('_') {
            return false;
        }

        if ALLOWLISTED_TAGS.contains(&key) {
            return false;
        }

        !self.keep_values.contains(key)
    }

    /// Returns `true` when `val` could be a credit card number.
    ///
    /// Detection honors the configured `luhn` setting: when it is enabled, a candidate must also
    /// pass the Luhn checksum.
    pub fn is_card_number(&self, val: &str) -> bool {
        is_card_number(val, self.luhn)
    }

    /// Obfuscates credit card numbers in a value for the given key.
    /// Returns `Some(replacement)` if a credit card number is detected, `None` if unchanged.
    pub fn obfuscate_credit_card_number(&self, key: &str, val: &str) -> Option<MetaString> {
        if !self.should_obfuscate_key(key) {
            return None;
        }

        if self.is_card_number(val) {
            return Some(CREDIT_CARD_REPLACEMENT.into());
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> CreditCardObfuscationConfig {
        CreditCardObfuscationConfig {
            enabled: true,
            luhn: false,
            keep_values: Vec::new(),
        }
    }

    #[test]
    fn test_iin_is_sensitive_valid() {
        let config = CreditCardObfuscationConfig {
            enabled: true,
            luhn: true, // Enable Luhn validation
            keep_values: Vec::new(),
        };
        let obfuscator = CreditCardObfuscator::new(&config);

        let valid_cards = vec![
            "378282246310005",
            "  378282246310005",
            "  3782-8224-6310-005 ",
            "371449635398431",
            "378734493671000",
            "5610591081018250",
            "30569309025904",
            "38520000023237",
            "6011 1111 1111 1117",
            "6011000990139424",
            " 3530111333--300000  ",
            "3566002020360505",
            "5555555555554444",
            "5105-1051-0510-5100",
            " 4111111111111111",
            "4012888888881881 ",
            "422222 2222222",
            "5019717010103742",
            "6331101999990016",
            " 4242-4242-4242-4242 ",
            "4242-4242-4242-4242 ",
            "4242-4242-4242-4242  ",
            "4000056655665556",
            "5555555555554444",
            "2223003122003222",
            "5200828282828210",
            "5105105105105100",
            "378282246310005",
            "371449635398431",
            "6011111111111117",
            "6011000990139424",
            "3056930009020004",
            "3566002020360505",
            "620000000000000",
            "2222 4053 4324 8877",
            "2222 9909 0525 7051",
            "2223 0076 4872 6984",
            "2223 5771 2001 7656",
            "5105 1051 0510 5100",
            "5111 0100 3017 5156",
            "5185 5408 1000 0019",
            "5200 8282 8282 8210",
            "5204 2300 8000 0017",
            "5204 7400 0990 0014",
            "5420 9238 7872 4339",
            "5455 3307 6000 0018",
            "5506 9004 9000 0436",
            "5506 9004 9000 0444",
            "5506 9005 1000 0234",
            "5506 9208 0924 3667",
            "5506 9224 0063 4930",
            "5506 9274 2731 7625",
            "5553 0422 4198 4105",
            "5555 5537 5304 8194",
            "5555 5555 5555 4444",
            "4012 8888 8888 1881",
            "4111 1111 1111 1111",
            "6011 0009 9013 9424",
            "6011 1111 1111 1117",
            "3714 496353 98431",
            "3782 822463 10005",
            "3056 9309 0259 04",
            "3852 0000 0232 37",
            "3530 1113 3330 0000",
            "3566 0020 2036 0505",
            "3700 0000 0000 002",
            "3700 0000 0100 018",
            "6703 4444 4444 4449",
            "4871 0499 9999 9910",
            "4035 5010 0000 0008",
            "4360 0000 0100 0005",
            "6243 0300 0000 0001",
            "5019 5555 4444 5555",
            "3607 0500 0010 20",
            "6011 6011 6011 6611",
            "6445 6445 6445 6445",
            "5066 9911 1111 1118",
            "6062 8288 8866 6688",
            "3569 9900 1009 5841",
            "6771 7980 2100 0008",
            "2222 4000 7000 0005",
            "5555 3412 4444 1115",
            "5577 0000 5577 0004",
            "5555 4444 3333 1111",
            "2222 4107 4036 0010",
            "5555 5555 5555 4444",
            "2222 4107 0000 0002",
            "2222 4000 1000 0008",
            "2223 0000 4841 0010",
            "2222 4000 6000 0007",
            "2223 5204 4356 0010",
            "2222 4000 3000 0004",
            "5100 0600 0000 0002",
            "2222 4000 5000 0009",
            "1354 1001 4004 955",
            "4111 1111 4555 1142",
            "4988 4388 4388 4305",
            "4166 6766 6766 6746",
            "4646 4646 4646 4644",
            "4000 6200 0000 0007",
            "4000 0600 0000 0006",
            "4293 1891 0000 0008",
            "4988 0800 0000 0000",
            "4111 1111 1111 1111",
            "4444 3333 2222 1111",
            "4001 5900 0000 0001",
            "4000 1800 0000 0002",
            "4000 0200 0000 0000",
            "4000 1600 0000 0004",
            "4002 6900 0000 0008",
            "4400 0000 0000 0008",
            "4484 6000 0000 0004",
            "4607 0000 0000 0009",
            "4977 9494 9494 9497",
            "4000 6400 0000 0005",
            "4003 5500 0000 0003",
            "4000 7600 0000 0001",
            "4017 3400 0000 0003",
            "4005 5190 0000 0006",
            "4131 8400 0000 0003",
            "4035 5010 0000 0008",
            "4151 5000 0000 0008",
            "4571 0000 0000 0001",
            "4199 3500 0000 0002",
            "4001 0200 0000 0009",
        ];

        for (i, card) in valid_cards.iter().enumerate() {
            assert!(obfuscator.is_card_number(card), "Failed for card #{}: {}", i, card);
        }
    }

    #[test]
    fn test_iin_is_sensitive_invalid() {
        let config = CreditCardObfuscationConfig {
            enabled: true,
            luhn: false, // Disable Luhn validation for this test
            keep_values: Vec::new(),
        };
        let obfuscator = CreditCardObfuscator::new(&config);

        let invalid_cards = [
            "37828224631000521389798",
            "37828224631",
            "   3782822-4631 ",
            "3714djkkkksii31",
            "x371413321323331",
            "",
            "7712378231899",
            "   -  ",
        ];

        for (i, card) in invalid_cards.iter().enumerate() {
            assert!(
                !obfuscator.is_card_number(card),
                "Should be invalid for card #{}: {}",
                i,
                card
            );
        }
    }

    #[test]
    fn test_cc_keep_values() {
        let config = CreditCardObfuscationConfig {
            enabled: true,
            luhn: false,
            keep_values: vec!["skip_me".to_string()],
        };
        let obfuscator = CreditCardObfuscator::new(&config);

        let possible_card = "378282246310005";

        // skip_me is in keep_values, so no obfuscation (returns None)
        assert_eq!(obfuscator.obfuscate_credit_card_number("skip_me", possible_card), None);

        // obfuscate_me is not in keep_values, so obfuscation happens (returns Some("?"))
        assert_eq!(
            obfuscator.obfuscate_credit_card_number("obfuscate_me", possible_card),
            Some("?".into())
        );
    }

    #[test]
    fn test_is_card_number_basic() {
        let obfuscator = CreditCardObfuscator::new(&default_config());

        assert!(obfuscator.is_card_number("4532123456789010"));
        assert!(obfuscator.is_card_number("4532 1234 5678 9010"));
        assert!(obfuscator.is_card_number("4532-1234-5678-9010"));
        assert!(!obfuscator.is_card_number("45321234"));
        assert!(!obfuscator.is_card_number("9999123456789012"));
    }

    #[test]
    fn test_obfuscate_credit_card_number() {
        let obfuscator = CreditCardObfuscator::new(&default_config());

        // Credit card detected -> Some("?")
        assert_eq!(
            obfuscator.obfuscate_credit_card_number("payment.card", "4532123456789010"),
            Some("?".into())
        );
        // Allowlisted tag -> None (unchanged)
        assert_eq!(
            obfuscator.obfuscate_credit_card_number("http.status_code", "4532123456789010"),
            None
        );
        // Starts with underscore -> None (unchanged)
        assert_eq!(
            obfuscator.obfuscate_credit_card_number("_internal", "4532123456789010"),
            None
        );
        // Not a credit card -> None (unchanged)
        assert_eq!(obfuscator.obfuscate_credit_card_number("user.id", "12345"), None);
    }

    #[test]
    fn test_obfuscate_with_luhn() {
        let config = CreditCardObfuscationConfig {
            enabled: true,
            luhn: true,
            keep_values: Vec::new(),
        };
        let obfuscator = CreditCardObfuscator::new(&config);

        // Valid Luhn checksum -> obfuscated
        assert_eq!(
            obfuscator.obfuscate_credit_card_number("payment.card", "4111111111111111"),
            Some("?".into())
        );
        // Invalid Luhn checksum -> not a card, unchanged
        assert_eq!(
            obfuscator.obfuscate_credit_card_number("payment.card", "4111111111111112"),
            None
        );
    }

    #[test]
    fn test_more_than_sixteen_digits_is_not_a_card() {
        let obfuscator = CreditCardObfuscator::new(&default_config());

        // 16 digits behind a valid IIN prefix is the longest card number we detect.
        assert!(obfuscator.is_card_number("4111111111111111"));

        // 17 digits and up are something else, such as a job or run identifier.
        assert!(!obfuscator.is_card_number("41111111111111111"));
        assert!(!obfuscator.is_card_number("411111111111111111"));
        assert!(!obfuscator.is_card_number("4111111111111111111"));
        assert!(!obfuscator.is_card_number("4111-1111-1111-1111-111"));
    }

    #[test]
    fn test_data_job_monitoring_keys_are_exempt() {
        let obfuscator = CreditCardObfuscator::new(&default_config());

        let card = "4111111111111111";
        for key in [
            "databricks_job_id",
            "databricks_job_run_id",
            "databricks_task_run_id",
            "config.spark_app_startTime",
            "config.spark_databricks_job_parentRunId",
        ] {
            assert!(!obfuscator.should_obfuscate_key(key), "key should be exempt: {}", key);
            assert_eq!(
                obfuscator.obfuscate_credit_card_number(key, card),
                None,
                "key should be exempt: {}",
                key
            );
        }
    }
}
