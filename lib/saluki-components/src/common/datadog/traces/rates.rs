//! Sampling rates by service, as the priority sampler publishes them to tracers.
//!
//! Mirrors the trace-agent's `RateByService` (`pkg/trace/sampler/dynamic_config.go`): the priority sampler sets every
//! rate each time its sliding window moves, and the receiver answers trace payloads with them (`httpRateByService` in
//! `pkg/trace/api/responses.go`). [`ServiceRates`] is the handle both share: the sampler writes through a
//! [`RateByService`], and the intake reads the current [`RatesSnapshot`].
//!
//! Rates change at most once per sampler bucket (five seconds), while every accepted payload reads them. So the writer
//! encodes the response body once per change and swaps it in whole, with `arc-swap`: a read is a lock-free pointer load
//! and a reference count, rather than the map copy and JSON encoding the trace-agent does per request under a read
//! lock.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use bytes::Bytes;
use http::HeaderValue;
use saluki_common::collections::FastHashMap;

/// Body of a response with no rates, as Go's `json.Encoder` writes `traceResponse`, with a trailing newline.
const EMPTY_RATES_BODY: &str = "{\"rate_by_service\":{}}\n";

/// The rates current at one point, ready to be sent.
#[derive(Debug)]
pub(crate) struct RatesSnapshot {
    /// Version of the rates, empty until the sampler first sets them.
    pub version: HeaderValue,

    /// The `traceResponse` JSON body holding every rate.
    pub body: Bytes,
}

impl Default for RatesSnapshot {
    fn default() -> Self {
        Self {
            version: HeaderValue::from_static(""),
            body: Bytes::from_static(EMPTY_RATES_BODY.as_bytes()),
        }
    }
}

/// Shared handle to the sampling rates by service, written by the trace sampler and read by the trace intake.
#[derive(Clone, Debug, Default)]
pub struct ServiceRates {
    current: Arc<ArcSwap<RatesSnapshot>>,
}

impl ServiceRates {
    /// Returns the current rates.
    pub(crate) fn load(&self) -> Arc<RatesSnapshot> {
        self.current.load_full()
    }
}

struct Rate {
    rate: f64,
    color: bool,
}

/// Writer of the rates, the trace-agent's `RateByService.SetAll`.
pub(crate) struct RateByService {
    handle: ServiceRates,
    rates: FastHashMap<String, Rate>,
    /// Flips on every `set_all`; a key not painted with the current color is no longer set.
    color: bool,
    /// Counts versions, as the trace-agent's `localVersion`.
    versions: u64,
}

impl RateByService {
    pub(crate) fn new(handle: ServiceRates) -> Self {
        Self {
            handle,
            rates: FastHashMap::default(),
            color: false,
            versions: 0,
        }
    }

    /// Sets the rate of every `service:X,env:Y` key, removing keys not given, and publishes a new version if anything
    /// changed. Rates are clamped to `[0, 1]`.
    pub(crate) fn set_all(&mut self, rates: impl IntoIterator<Item = (String, f64)>) {
        self.color = !self.color;
        let mut changed = false;
        for (key, rate) in rates {
            // Unlike Go's `math.Min(math.Max(r, 0), 1)`, NaN reads as 0 rather than failing the response encoding.
            let rate = if rate > 0.0 { rate.min(1.0) } else { 0.0 };
            let entry = self.rates.entry(key).or_insert_with(|| {
                changed = true;
                Rate { rate, color: false }
            });
            if entry.rate != rate {
                changed = true;
                entry.rate = rate;
            }
            entry.color = self.color;
        }
        let color = self.color;
        self.rates.retain(|_, rate| {
            changed |= rate.color != color;
            rate.color == color
        });
        if changed {
            self.versions += 1;
            self.handle.current.store(Arc::new(RatesSnapshot {
                version: new_version(SystemTime::now(), self.versions),
                body: encode_rates(&self.rates),
            }));
        }
    }
}

/// Builds a version as the trace-agent's `newVersion`: the Unix time in seconds and a counter, both in hexadecimal.
fn new_version(now: SystemTime, counter: u64) -> HeaderValue {
    let secs = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    HeaderValue::from_str(&format!("{secs:x}-{counter:x}")).expect("hexadecimal digits and a dash are a valid header")
}

/// Encodes the rates as Go's `json.Encoder` encodes `traceResponse`: keys sorted, floats and strings formatted as
/// `encoding/json` formats them, and a trailing newline.
fn encode_rates(rates: &FastHashMap<String, Rate>) -> Bytes {
    let sorted: BTreeMap<&str, f64> = rates.iter().map(|(key, rate)| (key.as_str(), rate.rate)).collect();
    let mut body = String::from("{\"rate_by_service\":{");
    for (i, (key, rate)) in sorted.into_iter().enumerate() {
        if i > 0 {
            body.push(',');
        }
        write_go_json_string(&mut body, key);
        body.push(':');
        write_go_json_float(&mut body, rate);
    }
    body.push_str("}}\n");
    Bytes::from(body)
}

/// Writes a string as `encoding/json` does with HTML escaping on, the `json.Encoder` default.
fn write_go_json_string(out: &mut String, value: &str) {
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' || c == '<' || c == '>' || c == '&' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Writes a float as `encoding/json` does: the shortest form that round-trips, in exponent form below `1e-6` or from
/// `1e21`, with a two-digit negative exponent shortened (`1e-07` as `1e-7`).
fn write_go_json_float(out: &mut String, value: f64) {
    let abs = value.abs();
    if abs != 0.0 && !(1e-6..1e21).contains(&abs) {
        // Rust's `LowerExp` already writes the exponent without padding, as Go's cleanup leaves it, but Go writes a
        // positive exponent with its sign.
        let formatted = format!("{value:e}");
        match formatted.split_once('e') {
            Some((mantissa, exponent)) if !exponent.starts_with('-') => {
                let _ = write!(out, "{mantissa}e+{exponent}");
            }
            _ => out.push_str(&formatted),
        }
    } else {
        let _ = write!(out, "{value}");
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn rates(entries: &[(&str, f64)]) -> Vec<(String, f64)> {
        entries.iter().map(|(key, rate)| (key.to_string(), *rate)).collect()
    }

    fn body(handle: &ServiceRates) -> String {
        String::from_utf8(handle.load().body.to_vec()).unwrap()
    }

    #[test]
    fn starts_with_no_rates_and_no_version() {
        let handle = ServiceRates::default();
        assert_eq!(handle.load().version, "");
        assert_eq!(body(&handle), EMPTY_RATES_BODY);
    }

    // `TestRateByServiceGetSet` in `dynamic_config_test.go`.
    #[test]
    fn set_all_replaces_every_rate() {
        let handle = ServiceRates::default();
        let mut writer = RateByService::new(handle.clone());
        let cases: [(&[(&str, f64)], &str); 5] = [
            (
                &[("service:,env:", 0.1)],
                r#"{"rate_by_service":{"service:,env:":0.1}}"#,
            ),
            (
                &[
                    ("service:,env:", 0.3),
                    ("service:mcnulty,env:dev", 0.2),
                    ("service:postgres,env:dev", 0.1),
                ],
                r#"{"rate_by_service":{"service:,env:":0.3,"service:mcnulty,env:dev":0.2,"service:postgres,env:dev":0.1}}"#,
            ),
            (&[("service:,env:", 1.0)], r#"{"rate_by_service":{"service:,env:":1}}"#),
            (&[], r#"{"rate_by_service":{}}"#),
            (
                &[("service:,env:", 0.2)],
                r#"{"rate_by_service":{"service:,env:":0.2}}"#,
            ),
        ];
        for (input, expected) in cases {
            writer.set_all(rates(input));
            assert_eq!(body(&handle), format!("{expected}\n"));
        }
    }

    // `TestRateByServiceLimits` in `dynamic_config_test.go`.
    #[test]
    fn rates_are_clamped() {
        let handle = ServiceRates::default();
        let mut writer = RateByService::new(handle.clone());
        writer.set_all(rates(&[("service:high,env:", 2.0), ("service:low,env:", -1.0)]));
        assert_eq!(
            body(&handle),
            "{\"rate_by_service\":{\"service:high,env:\":1,\"service:low,env:\":0}}\n"
        );
    }

    // `TestVersionChanges` in `dynamic_config_test.go`.
    #[test]
    fn the_version_changes_only_with_the_rates() {
        let handle = ServiceRates::default();
        let mut writer = RateByService::new(handle.clone());
        let mut current = rates(&[
            ("service:one,env:prod", 0.5),
            ("service:two,env:test", 0.4),
            ("service:three,env:test", 0.4),
        ]);
        let version = |handle: &ServiceRates| handle.load().version.clone();

        writer.set_all(current.clone());
        let first = version(&handle);
        assert_ne!(first, "");

        writer.set_all(current.clone());
        assert_eq!(version(&handle), first, "same rates");

        current[0].1 = 0.4;
        writer.set_all(current.clone());
        let second = version(&handle);
        assert_ne!(second, first, "a changed rate");

        current.push(("service:four,env:test".to_string(), 0.4));
        writer.set_all(current.clone());
        let third = version(&handle);
        assert_ne!(third, second, "an added rate");

        current.pop();
        writer.set_all(current);
        assert_ne!(version(&handle), third, "a removed rate");
    }

    #[test]
    fn versions_are_the_hexadecimal_time_and_counter() {
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(new_version(now, 26), "6553f100-1a");
    }

    #[test]
    fn floats_are_formatted_as_go_encoding_json_formats_them() {
        // Expected strings are Go's `json.Marshal(float64)` output.
        let cases = [
            (0.0, "0"),
            (1.0, "1"),
            (0.5, "0.5"),
            (0.1, "0.1"),
            (1.0 / 3.0, "0.3333333333333333"),
            (0.000001, "0.000001"),
            (0.0000001, "1e-7"),
            (0.00000015, "1.5e-7"),
            (1e-10, "1e-10"),
            (5e-324, "5e-324"),
            (123456.0, "123456"),
            (1e20, "100000000000000000000"),
            (1e21, "1e+21"),
            (1.5e300, "1.5e+300"),
        ];
        for (value, expected) in cases {
            let mut out = String::new();
            write_go_json_float(&mut out, value);
            assert_eq!(out, expected, "{value:e}");
        }
    }

    #[test]
    fn strings_are_escaped_as_go_encoding_json_escapes_them() {
        // Expected strings are Go's `json.NewEncoder(w).Encode(string)` output, without the trailing newline.
        let cases = [
            ("service:web,env:prod", r#""service:web,env:prod""#),
            ("a\"b\\c", r#""a\"b\\c""#),
            ("\n\r\t\u{8}\u{c}", r#""\n\r\t\b\f""#),
            ("\u{1}\u{1f}", r#""\u0001\u001f""#),
            ("<a&b>", r#""\u003ca\u0026b\u003e""#),
            ("\u{2028}\u{2029}", r#""\u2028\u2029""#),
            ("café", "\"café\""),
        ];
        for (value, expected) in cases {
            let mut out = String::new();
            write_go_json_string(&mut out, value);
            assert_eq!(out, expected);
        }
    }
}
