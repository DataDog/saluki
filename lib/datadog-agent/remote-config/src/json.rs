//! Decoding support for products whose payloads are JSON.

use std::fmt;

use serde::de::DeserializeOwned;

use crate::ApplyError;

/// Deserializes a JSON payload, for a [`decode`](crate::ProductDecoder::decode) implementation to call.
///
/// Payloads are opaque bytes and need not be JSON, so decoding is left to each product. This helper serves the
/// products whose payloads are. It takes no configuration ID: the client attributes a `decode` error to the
/// configuration being decoded.
///
/// # Errors
///
/// Returns [`JsonError`] if `payload` is not valid JSON or does not match the shape of `T`.
///
/// # Examples
///
/// ```
/// use datadog_agent_remote_config::{decode_json, ConfigId, JsonError, ProductDecoder};
/// use serde::Deserialize;
///
/// #[derive(Deserialize)]
/// struct Limits {
///     max_spans: u32,
/// }
///
/// #[derive(Default)]
/// struct LimitsDecoder {
///     limits: Option<Limits>,
/// }
///
/// impl ProductDecoder for LimitsDecoder {
///     const PRODUCT: &'static str = "EXAMPLE_LIMITS";
///
///     type Snapshot = Option<Limits>;
///     type Error = JsonError;
///
///     fn decode(&mut self, _id: &ConfigId, payload: &[u8]) -> Result<(), Self::Error> {
///         self.limits = Some(decode_json(payload)?);
///         Ok(())
///     }
///
///     fn build(self) -> Result<Self::Snapshot, Self::Error> {
///         Ok(self.limits)
///     }
/// }
///
/// let mut decoder = LimitsDecoder::default();
/// decoder.decode(&ConfigId::new("limits.v1"), br#"{"max_spans": 100}"#).unwrap();
/// assert!(decoder.decode(&ConfigId::new("limits.v2"), b"{").is_err());
/// assert_eq!(decoder.build().unwrap().unwrap().max_spans, 100);
/// ```
pub fn decode_json<T>(payload: &[u8]) -> Result<T, JsonError>
where
    T: DeserializeOwned,
{
    serde_json::from_slice(payload).map_err(JsonError)
}

/// A payload that [`decode_json`] could not deserialize.
///
/// The message gives the position of the failure and nothing of the payload's contents, so it is safe to report to the
/// Agent. The underlying [`serde_json::Error`], which may quote payload values, is available as the error's
/// [`source`](std::error::Error::source) for local diagnostics.
#[derive(Debug)]
pub struct JsonError(serde_json::Error);

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Payload is malformed JSON at line {}, column {}.",
            self.0.line(),
            self.0.column()
        )
    }
}

impl std::error::Error for JsonError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

impl ApplyError for JsonError {
    fn apply_error(&self) -> String {
        self.to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use serde::Deserialize;

    use crate::{decode_json, ApplyError};

    #[derive(Debug, Deserialize, PartialEq)]
    struct Limits {
        max_spans: u32,
    }

    #[test]
    fn decodes_a_json_payload() {
        let limits: Limits = decode_json(br#"{"max_spans": 100}"#).unwrap();

        assert_eq!(limits, Limits { max_spans: 100 });
    }

    #[test]
    fn reports_malformed_json_without_its_contents() {
        for (payload, expected) in [
            (br#"{"max_spans": 1"#.as_slice(), "line 1, column 15"),
            (br#"{"max_spans": "hunter2"}"#.as_slice(), "line 1, column 23"),
        ] {
            let error = decode_json::<Limits>(payload).unwrap_err();

            assert_eq!(error.to_string(), format!("Payload is malformed JSON at {expected}."));
            assert_eq!(error.apply_error(), error.to_string());
            assert!(!error.to_string().contains("hunter2"));
            let source = error.source().expect("the serde_json error is the source");
            assert!(source.is::<serde_json::Error>());
        }
    }
}
