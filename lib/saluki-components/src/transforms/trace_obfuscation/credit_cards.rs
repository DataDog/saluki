//! Credit card number obfuscation.

use saluki_common::collections::FastHashSet;
use stringtheory::MetaString;

use super::obfuscator::CreditCardObfuscationConfig;

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
            .keep_values()
            .iter()
            .map(|s| MetaString::from(s.as_str()))
            .collect();

        Self {
            luhn: config.luhn(),
            keep_values,
        }
    }

    /// Obfuscates credit card numbers in a value for the given key.
    /// Returns `Some(replacement)` if a credit card number is detected, `None` if unchanged.
    pub fn obfuscate_credit_card_number(&self, key: &str, val: &str) -> Option<MetaString> {
        if key.starts_with('_') {
            return None;
        }

        // Check static allowlist first.
        if ALLOWLISTED_TAGS.contains(&key) {
            return None;
        }

        // Check user-provided `keep_values`.
        if self.keep_values.contains(key) {
            return None;
        }

        if self.is_card_number(val) {
            return Some("?".into());
        }

        None
    }

    fn is_card_number(&self, b: &str) -> bool {
        if b.is_empty() || b.len() < 12 {
            return false;
        }

        let first_char = b.chars().next().unwrap();
        if first_char != ' ' && first_char != '-' && !first_char.is_ascii_digit() {
            return false;
        }

        let mut prefix = 0; // Holds up to first 6 digits as numeric value
        let mut count = 0; // Counts digits encountered
        let mut found_prefix = false;
        let mut digits = Vec::new(); // For Luhn validation

        for ch in b.chars() {
            match ch {
                ' ' | '-' => continue,
                '0'..='9' => {
                    count += 1;
                    let digit = ch as u8 - b'0';

                    if self.luhn {
                        digits.push(digit);
                    }

                    if !found_prefix {
                        prefix = prefix * 10 + digit as i32;
                        let (maybe, yes) = valid_card_prefix(prefix);
                        if yes {
                            found_prefix = true;
                        } else if !maybe {
                            return false;
                        }
                    }
                }
                _ => return false,
            }
        }

        if !found_prefix {
            return false;
        }

        if !(12..=19).contains(&count) {
            return false;
        }

        if self.luhn {
            return luhn_valid(&digits);
        }

        true
    }
}

fn luhn_valid(digits: &[u8]) -> bool {
    let mut sum = 0;
    let mut alt = false;
    let n = digits.len();

    for i in (0..n).rev() {
        let mut digit = digits[i] as i32;
        if alt {
            digit *= 2;
            if digit > 9 {
                digit = (digit % 10) + 1;
            }
        }
        alt = !alt;
        sum += digit;
    }

    sum % 10 == 0
}

fn valid_card_prefix(n: i32) -> (bool, bool) {
    if n > 699999 {
        return (false, false);
    }

    if n < 10 {
        return match n {
            1 | 4 => (false, true),
            2 | 3 | 5 | 6 => (true, false),
            _ => (false, false),
        };
    }

    if n < 100 {
        if (34..=39).contains(&n) || (51..=55).contains(&n) || n == 62 || n == 65 {
            return (false, true);
        }
        if n == 30
            || n == 63
            || n == 64
            || n == 50
            || n == 60
            || (22..=27).contains(&n)
            || (56..=58).contains(&n)
            || (60..=69).contains(&n)
        {
            return (true, false);
        }
        return (false, false);
    }

    if n < 1000 {
        if (300..=305).contains(&n) || (644..=649).contains(&n) || n == 309 || n == 636 {
            return (false, true);
        }
        if (352..=358).contains(&n)
            || n == 501
            || n == 601
            || (222..=272).contains(&n)
            || (500..=509).contains(&n)
            || (560..=589).contains(&n)
            || (600..=699).contains(&n)
        {
            return (true, false);
        }
        return (false, false);
    }

    if n < 10000 {
        if (3528..=3589).contains(&n) || n == 5019 || n == 6011 {
            return (false, true);
        }
        if (2221..=2720).contains(&n)
            || (5000..=5099).contains(&n)
            || (5600..=5899).contains(&n)
            || (6000..=6999).contains(&n)
        {
            return (true, false);
        }
        return (false, false);
    }

    if n < 100000 {
        if (22210..=27209).contains(&n)
            || (50000..=50999).contains(&n)
            || (56000..=58999).contains(&n)
            || (60000..=69999).contains(&n)
        {
            return (true, false);
        }
        return (false, false);
    }

    if n < 1000000
        && ((222100..=272099).contains(&n)
            || (500000..=509999).contains(&n)
            || (560000..=589999).contains(&n)
            || (600000..=699999).contains(&n))
    {
        return (false, true);
    }

    (false, false)
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
    fn test_luhn_valid() {
        // Valid credit card numbers (with Luhn checksum)
        assert!(luhn_valid(&[4, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1])); // Visa test card
        assert!(luhn_valid(&[5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 4, 4, 4, 4])); // Mastercard test card
        assert!(luhn_valid(&[3, 7, 8, 2, 8, 2, 2, 4, 6, 3, 1, 0, 0, 0, 5])); // Amex test card

        // Invalid Luhn checksum
        assert!(!luhn_valid(&[4, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2]));
        assert!(!luhn_valid(&[4, 5, 3, 2, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0, 1, 0]));
    }

    #[test]
    fn test_valid_card_prefix() {
        assert_eq!(valid_card_prefix(4), (false, true));
        assert_eq!(valid_card_prefix(51), (false, true));
        assert_eq!(valid_card_prefix(55), (false, true));
        assert_eq!(valid_card_prefix(34), (false, true));
        assert_eq!(valid_card_prefix(37), (false, true));
        assert_eq!(valid_card_prefix(99), (false, false));
        assert_eq!(valid_card_prefix(5), (true, false));
    }

    #[test]
    fn test_iin_valid_card_prefix() {
        let cases = vec![
            (1, false, true),
            (4, false, true),
            (2, true, false),
            (3, true, false),
            (5, true, false),
            (6, true, false),
            (7, false, false),
            (8, false, false),
            (9, false, false),
            (34, false, true),
            (37, false, true),
            (39, false, true),
            (51, false, true),
            (55, false, true),
            (62, false, true),
            (65, false, true),
            (30, true, false),
            (63, true, false),
            (22, true, false),
            (27, true, false),
            (69, true, false),
            (31, false, false),
            (29, false, false),
            (21, false, false),
            (300, false, true),
            (305, false, true),
            (644, false, true),
            (649, false, true),
            (309, false, true),
            (636, false, true),
            (352, true, false),
            (358, true, false),
            (501, true, false),
            (601, true, false),
            (222, true, false),
            (272, true, false),
            (500, true, false),
            (509, true, false),
            (560, true, false),
            (589, true, false),
            (600, true, false),
            (699, true, false),
            (3528, false, true),
            (3589, false, true),
            (5019, false, true),
            (6011, false, true),
            (2221, true, false),
            (2720, true, false),
            (5000, true, false),
            (5099, true, false),
            (5600, true, false),
            (5899, true, false),
            (6000, true, false),
            (6999, true, false),
            (22210, true, false),
            (27209, true, false),
            (50000, true, false),
            (50999, true, false),
            (56000, true, false),
            (58999, true, false),
            (60000, true, false),
            (69999, true, false),
            (21000, false, false),
            (55555, false, false),
            (222100, false, true),
            (272099, false, true),
            (500000, false, true),
            (509999, false, true),
            (560000, false, true),
            (589999, false, true),
            (600000, false, true),
            (699999, false, true),
            (551234, false, false),
            (594388, false, false),
            (219899, false, false),
        ];

        for (input, expected_maybe, expected_yes) in cases {
            let (maybe, yes) = valid_card_prefix(input);
            assert_eq!(
                (maybe, yes),
                (expected_maybe, expected_yes),
                "Failed for input: {}",
                input
            );
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
}
