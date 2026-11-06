//! Head-based sampling rate decoding for OTLP spans.

/// Number of rejected values out of which a `th` threshold is expressed (2^56).
const THRESHOLD_RANGE: u64 = 1 << 56;

/// Maximum number of hex digits in a `th` value.
const MAX_THRESHOLD_HEX_DIGITS: usize = 14;

/// Number of hex digits in an `rv` value.
const RANDOM_VALUE_HEX_DIGITS: usize = 14;

/// Smallest `p` value that means "not sampled" instead of a probability.
const P_VALUE_NOT_SAMPLED: u64 = 63;

/// Maximum accepted size of a whole tracestate, in bytes.
const MAX_TRACESTATE_LEN: usize = 1024;

/// Maximum accepted size of the `ot` vendor value, in bytes.
const MAX_OT_VALUE_LEN: usize = 256;

/// Tracestates with this many list members or more are rejected.
const MAX_TRACESTATE_MEMBERS: usize = 32;

/// Maximum accepted size of a tracestate key, in bytes.
const MAX_KEY_LEN: usize = 256;

/// Maximum accepted size of the tenant part of a multi-tenant key, in bytes.
const MAX_TENANT_LEN: usize = 241;

/// Maximum accepted size of the system part of a multi-tenant key, in bytes.
const MAX_SYSTEM_LEN: usize = 14;

/// Decodes the head-based sampling probability from a raw W3C tracestate.
///
/// Two encodings in the `ot` vendor entry are understood:
///
/// - `th:<hex>`: a rejection threshold of up to 14 hex digits with trailing zeros omitted. The probability is
///   `1 - threshold / 2^56`, so `th:8` is 0.5 and `th:0` is 1.0.
/// - `p:<n>`: a power-of-two probability of `2^-n`, so `p:0` is 1.0 and `p:4` is 0.0625. Values of 63 or more mean
///   "not sampled" and carry no probability.
///
/// `th` takes precedence over `p`. Returns `None` when the tracestate has no probability, and also when any part of it
/// is malformed: a tracestate that fails validation is rejected as a whole, even if its `ot` entry looks usable.
pub(super) fn sample_rate_from_tracestate(tracestate: &str) -> Option<f64> {
    if tracestate.is_empty() || tracestate.len() > MAX_TRACESTATE_LEN || !is_valid_tracestate(tracestate) {
        return None;
    }

    // When the `ot` vendor is listed more than once, the last entry wins.
    let mut ot_value = None;
    let mut members = 0;
    let mut rest = tracestate;
    while !rest.is_empty() {
        members += 1;
        if members >= MAX_TRACESTATE_MEMBERS {
            return None;
        }

        let (member, tail) = rest.split_once(',').unwrap_or((rest, ""));
        rest = tail;

        let member = member.trim_matches([' ', '\t']);
        if member.is_empty() {
            continue;
        }
        let (key, value) = member.split_once('=')?;
        if key.len() > MAX_KEY_LEN {
            return None;
        }
        if let Some((tenant, system)) = key.split_once('@') {
            if tenant.len() > MAX_TENANT_LEN || system.len() > MAX_SYSTEM_LEN {
                return None;
            }
        }
        if key == "ot" {
            ot_value = Some(parse_ot_value(value)?);
        }
    }

    let ot = ot_value?;
    if let Some(threshold) = ot.threshold {
        return Some((THRESHOLD_RANGE - threshold) as f64 / THRESHOLD_RANGE as f64);
    }
    match ot.p_value?.parse::<u64>() {
        Ok(0) => Some(1.0),
        Ok(p) if p < P_VALUE_NOT_SAMPLED => Some(1.0 / (1u64 << p) as f64),
        _ => None,
    }
}

/// The parts of the `ot` vendor entry that carry a sampling probability.
struct OtValue<'a> {
    /// Rejection threshold out of 2^56, from `th`.
    threshold: Option<u64>,
    /// Raw value of the first `p` field.
    p_value: Option<&'a str>,
}

/// Parses and validates the value of the `ot` vendor entry, a `;`-separated list of `key:value` fields.
fn parse_ot_value(value: &str) -> Option<OtValue<'_>> {
    if value.len() > MAX_OT_VALUE_LEN || value.is_empty() || value.ends_with(';') {
        return None;
    }

    let mut ot = OtValue {
        threshold: None,
        p_value: None,
    };
    for field in value.split(';') {
        let (key, field_value) = field.split_once(':')?;
        let mut key_bytes = key.bytes();
        if !key_bytes.next().is_some_and(|b| b.is_ascii_lowercase())
            || !key_bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            || !field_value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        {
            return None;
        }

        match key {
            "rv" => {
                if field_value.len() != RANDOM_VALUE_HEX_DIGITS || !field_value.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return None;
                }
            }
            "th" => {
                let digits = field_value.len();
                if digits == 0
                    || digits > MAX_THRESHOLD_HEX_DIGITS
                    || !field_value.bytes().all(|b| b.is_ascii_hexdigit())
                {
                    return None;
                }
                // The omitted trailing zeros are restored by shifting up to 14 digits.
                let parsed = u64::from_str_radix(field_value, 16).ok()?;
                ot.threshold = Some(parsed << (4 * (MAX_THRESHOLD_HEX_DIGITS - digits)));
            }
            "p" => {
                ot.p_value.get_or_insert(field_value);
            }
            _ => {}
        }
    }
    Some(ot)
}

/// Checks the tracestate list-member syntax: `key=value` members separated by commas, with optional surrounding
/// whitespace and empty members allowed.
fn is_valid_tracestate(tracestate: &str) -> bool {
    tracestate.split(',').all(|member| {
        let member = member.trim_matches([' ', '\t']);
        if member.is_empty() {
            return true;
        }
        match member.split_once('=') {
            Some((key, value)) if !key.is_empty() => is_valid_key(key) && is_valid_value(value),
            _ => false,
        }
    })
}

fn is_valid_key_char(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-' | b'*' | b'/')
}

/// Either a simple key (`lcalpha` followed by key characters) or a multi-tenant key (`tenant@system`).
fn is_valid_key(key: &str) -> bool {
    match key.split_once('@') {
        Some((tenant, system)) => {
            let tenant = tenant.as_bytes();
            let system = system.as_bytes();
            !tenant.is_empty()
                && !system.is_empty()
                && (tenant[0].is_ascii_lowercase() || tenant[0].is_ascii_digit())
                && system[0].is_ascii_lowercase()
                && tenant[1..].iter().all(|&b| is_valid_key_char(b))
                && system[1..].iter().all(|&b| is_valid_key_char(b))
        }
        None => {
            let key = key.as_bytes();
            key[0].is_ascii_lowercase() && key[1..].iter().all(|&b| is_valid_key_char(b))
        }
    }
}

/// A value is 1 to 256 printable ASCII characters, excluding `,` and `=`, and cannot end in a space.
fn is_valid_value(value: &str) -> bool {
    let bytes = value.as_bytes();
    let is_non_blank = |b: u8| (0x21..=0x7e).contains(&b) && b != b',' && b != b'=';
    match bytes.split_last() {
        Some((&last, init)) => {
            bytes.len() <= 256 && is_non_blank(last) && init.iter().all(|&b| b == b' ' || is_non_blank(b))
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn assert_rate(tracestate: &str, expected: Option<f64>) {
        match (sample_rate_from_tracestate(tracestate), expected) {
            (Some(got), Some(want)) => assert!(
                (got - want).abs() < 1e-9,
                "tracestate {tracestate:?}: expected {want}, got {got}"
            ),
            (got, want) => assert_eq!(got, want, "tracestate {tracestate:?}"),
        }
    }

    #[test]
    fn decodes_threshold_and_power_of_two_encodings() {
        let cases: &[(&str, &str, Option<f64>)] = &[
            ("50% sampling (th:8)", "ot=th:8", Some(0.5)),
            ("100% sampling (th:0)", "ot=th:0", Some(1.0)),
            ("p-encoding 50% (p:1)", "ot=p:1;r:1", Some(0.5)),
            ("p-encoding 6.25% (p:4)", "ot=p:4;r:4", Some(0.0625)),
            ("p-encoding 100% (p:0)", "ot=p:0", Some(1.0)),
            ("th absent rv present", "ot=rv:abcdefabcdefab", None),
            ("no tracestate", "", None),
            ("non-ot vendor field only", "zz=vendorcontent", None),
        ];
        for (name, tracestate, expected) in cases {
            let got = sample_rate_from_tracestate(tracestate);
            match expected {
                Some(want) => {
                    let got = got.unwrap_or_else(|| panic!("{name}: expected {want}, got no rate"));
                    assert!((got - want).abs() < 1e-9, "{name}: expected {want}, got {got}");
                }
                None => assert_eq!(got, None, "{name}"),
            }
        }
    }

    #[test]
    fn malformed_threshold_gives_no_rate() {
        assert_rate("ot=th:zz", None);
    }

    #[test]
    fn threshold_trailing_zeros_are_restored() {
        assert_rate("ot=th:1", Some(0.9375));
        assert_rate("ot=th:10", Some(0.9375));
        assert_rate("ot=th:c", Some(0.25));
        assert_rate("ot=th:00000000000000", Some(1.0));
        assert_rate("ot=th:ffffffffffffff", Some(1.0 / THRESHOLD_RANGE as f64));
        assert_rate("ot=th:C", Some(0.25));
    }

    #[test]
    fn threshold_length_limits() {
        assert_rate("ot=th:", None);
        assert_rate("ot=th:fffffffffffff0", Some(16.0 / THRESHOLD_RANGE as f64));
        assert_rate("ot=th:800000000000000", None);
    }

    #[test]
    fn power_of_two_bounds() {
        assert_rate("ot=p:62", Some(2f64.powi(-62)));
        assert_rate("ot=p:63", None);
        assert_rate("ot=p:64", None);
        assert_rate("ot=p:99999999999999999999999", None);
        assert_rate("ot=p:abc", None);
        assert_rate("ot=p:", None);
        assert_rate("ot=p:-1", None);
        assert_rate("ot=p:01", Some(0.5));
    }

    #[test]
    fn threshold_takes_precedence_over_power_of_two() {
        assert_rate("ot=p:1;th:c", Some(0.25));
        assert_rate("ot=th:c;p:1", Some(0.25));
    }

    #[test]
    fn first_power_of_two_field_is_used() {
        assert_rate("ot=p:2;p:1", Some(0.25));
        assert_rate("ot=p:abc;p:1", None);
    }

    #[test]
    fn last_threshold_field_wins() {
        assert_rate("ot=th:8;th:c", Some(0.25));
    }

    #[test]
    fn ot_entry_is_found_among_other_vendors() {
        assert_rate("zz=vendorcontent,ot=th:8,aa=bb", Some(0.5));
        assert_rate("zz=vendorcontent , ot=th:8 ", Some(0.5));
        assert_rate("zz=vendorcontent,,ot=th:8,", Some(0.5));
        assert_rate("tenant@system=x,ot=th:8", Some(0.5));
        assert_rate("ot=th:8,ot=th:c", Some(0.25));
        assert_rate("ot=th:8,ot=rv:abcdefabcdefab", None);
    }

    #[test]
    fn other_ot_fields_do_not_prevent_decoding() {
        assert_rate("ot=rv:abcdefabcdefab;th:8", Some(0.5));
        assert_rate("ot=th:8;foo:bar.baz_qux-1", Some(0.5));
    }

    #[test]
    fn malformed_ot_entry_gives_no_rate() {
        for tracestate in [
            "ot=",
            "ot=th",
            "ot=th:8;",
            "ot=;th:8",
            "ot=th:8;;p:1",
            "ot=TH:8",
            "ot=th:8;1x:y",
            "ot=th:8;foo:b@r",
            "ot=rv:abc;th:8",
            "ot=rv:abcdefabcdefabcd;th:8",
            "ot=rv:abcdefabcdefag;th:8",
            "ot=th:8;th:zz",
            "ot=th:-1",
            "ot=th:+8",
        ] {
            assert_rate(tracestate, None);
        }
    }

    #[test]
    fn invalid_tracestate_is_rejected_as_a_whole() {
        for tracestate in [
            "garbage",
            "=ot",
            "ot=th:8,garbage",
            "ot=th:8,BAD=x",
            "ot=th:8,a=",
            "ot=th:8,a=b=c",
            "ot=th:8,1a=b",
            "ot=th:8,@sys=x",
            "ot=th:8,tenant@=x",
            "ot=th:8,tenant@1sys=x",
            "ot=th:8,te@nant@sys=x",
            "ot=th:8,tenant@averyveryverylongsystem=x",
        ] {
            assert_rate(tracestate, None);
        }
    }

    #[test]
    fn size_limits() {
        let long_value = "x".repeat(256);
        assert_rate(&format!("a={long_value},ot=th:8"), Some(0.5));
        assert_rate(&format!("a={long_value}x,ot=th:8"), None);

        let long_ot = format!("ot=th:8;f:{}", "x".repeat(256));
        assert_rate(&long_ot, None);

        let long_tenant = "a".repeat(242);
        assert_rate(&format!("{long_tenant}@sys=x,ot=th:8"), None);

        let members = |count: usize| {
            let mut parts: Vec<String> = (0..count - 1).map(|i| format!("v{i}=x")).collect();
            parts.push("ot=th:8".to_string());
            parts.join(",")
        };
        assert_rate(&members(31), Some(0.5));
        assert_rate(&members(32), None);

        assert_rate(&format!("ot=th:8,{}", "a=b,".repeat(300)), None);
    }

    proptest! {
        #[test]
        fn property_test_threshold_matches_definition(threshold in 0u64..THRESHOLD_RANGE) {
            let rate = sample_rate_from_tracestate(&format!("ot=th:{threshold:014x}"));
            let expected = (THRESHOLD_RANGE - threshold) as f64 / THRESHOLD_RANGE as f64;
            prop_assert_eq!(rate, Some(expected));
        }

        #[test]
        fn property_test_trimmed_threshold_matches_padded(threshold in 1u64..THRESHOLD_RANGE) {
            let padded = format!("{threshold:014x}");
            let trimmed = padded.trim_end_matches('0');
            prop_assert_eq!(
                sample_rate_from_tracestate(&format!("ot=th:{trimmed}")),
                sample_rate_from_tracestate(&format!("ot=th:{padded}")),
            );
        }

        #[test]
        fn property_test_power_of_two_matches_definition(p in 0u32..63) {
            prop_assert_eq!(sample_rate_from_tracestate(&format!("ot=p:{p}")), Some(2f64.powi(-(p as i32))));
        }

        #[test]
        fn property_test_arbitrary_input_never_panics_and_rate_is_a_probability(
            tracestate in "[ -~\t]{0,64}|[a-z0-9=,;:@ ]{0,64}|ot=[a-z0-9;:]{0,40}"
        ) {
            if let Some(rate) = sample_rate_from_tracestate(&tracestate) {
                prop_assert!(rate > 0.0 && rate <= 1.0, "rate {} out of range", rate);
            }
        }
    }
}
