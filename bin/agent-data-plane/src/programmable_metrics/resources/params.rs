//! The plan parameters of output bindings, host settings, and recipient protocols: their names, their types, and the
//! codec between them and the typed specifications of `saluki-components`.
//!
//! The plan crate carries what an output delivers through as typed [`Params`] that it never reads, and
//! `saluki-components` cannot read them, since it does not depend on the plan crate. This module is the one place that
//! names them: lowering encodes the typed specifications of `saluki-components` with these names, the installer decodes
//! them back, and the schema [`host_schema`] returns to the catalog is these tables, so validation, `explain`, and
//! decoding cannot disagree.
//!
//! A parameter's name is the name of the typed field it carries, in snake case:
//!
//! - [`ENCODER`]: an output binding's encoder (`OutputBinding::encoder`), the fields of [`EncoderSettings`]
//!   ([`encode_encoder`], [`decode_encoder`]);
//! - [`HOST_SETTINGS`]: a delivery host's settings (`HostBinding::settings`), the fields of [`DeliverySettings`]
//!   ([`encode_host_settings`], [`decode_host_settings`]);
//! - [`RECIPIENT_PROTOCOL`]: a recipient's protocol (`RecipientDecl::protocol`), the variants of [`RecipientProtocol`]
//!   flattened under `kind` ([`encode_protocol`], [`decode_protocol`]).
//!
//! Durations are written in their canonical form (`"10s"`), ratios and backoff factors as decimals written exactly
//! (`"0.5"`), and sizes as integers, so that one setting always writes one plan hash.

use std::{fmt, time::Duration};

use saluki_components::forwarders::delivery::{
    DeliverySettings, EncoderSettings, ForwarderHttpProtocol, IntakeAuthorities, ProtocolVariants, ProxyEndpoint,
    ProxySettings, RecipientProtocol, RetrySettings, SeriesVariant, V3Acceptance,
};
use saluki_error::{generic_error, GenericError};
use saluki_io::net::client::http::TlsMinimumVersion;
use saluki_metric_plan::{
    canonical::MAX_SAFE_INTEGER,
    ir::{decode_params, format_duration, Decimal, DecodedParams, ParamSpec, ParamType, ParamValue, Params},
    validate::HostSchema,
};
use serde_json::{json, Value};
use stringtheory::MetaString;

use super::FixedHost;

/// A size, count, or level: any integer the plan's canonical form represents exactly.
const INTEGER: ParamType = ParamType::Integer {
    min: -MAX_SAFE_INTEGER,
    max: MAX_SAFE_INTEGER,
};

/// A size or count, which is never negative.
const COUNT: ParamType = ParamType::Integer {
    min: 0,
    max: MAX_SAFE_INTEGER,
};

/// The protocol variants a lease produces.
const VARIANTS: &[ParamSpec] = &[
    ParamSpec::required("v2_series", ParamType::Bool),
    ParamSpec::required("v2_sketches", ParamType::Bool),
    ParamSpec::required("v3_series", ParamType::Bool),
    ParamSpec::required("v3_sketches", ParamType::Bool),
];

/// The parameters of an output binding's encoder: the fields of `EncoderSettings`.
pub const ENCODER: &[ParamSpec] = &[
    ParamSpec::required("variants", ParamType::Record(VARIANTS)),
    ParamSpec::required("series", ParamType::Enum(&["v2", "v1"])),
    ParamSpec::required("compressor_kind", ParamType::String),
    ParamSpec::required("zstd_compressor_level", INTEGER),
    ParamSpec::required("v3_compression_level", INTEGER),
    ParamSpec::required("max_metrics_per_payload", COUNT),
    ParamSpec::required("max_payload_size", COUNT),
    ParamSpec::required("max_uncompressed_payload_size", COUNT),
    ParamSpec::required("max_series_payload_size", COUNT),
    ParamSpec::required("max_series_uncompressed_payload_size", COUNT),
    ParamSpec::required("max_series_points_per_payload", COUNT),
    ParamSpec::required("flush_timeout", ParamType::Duration),
    ParamSpec::required("log_payloads", ParamType::Bool),
];

/// A delivery host's retry settings: the fields of `RetrySettings`.
const RETRY: &[ParamSpec] = &[
    ParamSpec::required("backoff_factor", ParamType::Decimal),
    ParamSpec::required("backoff_base", ParamType::Decimal),
    ParamSpec::required("backoff_max", ParamType::Decimal),
    ParamSpec::required("recovery_error_decrease_factor", COUNT),
    ParamSpec::required("recovery_reset", ParamType::Bool),
    ParamSpec::required("queue_max_size_bytes", COUNT),
    ParamSpec::required("storage_max_size_bytes", COUNT),
    ParamSpec::required("flush_to_disk_mem_ratio", ParamType::Decimal),
    ParamSpec::required("storage_max_disk_ratio", ParamType::Decimal),
    ParamSpec::required("outdated_file_in_days", COUNT),
];

/// One proxy: the fields of `ProxyEndpoint`, its URL without user information and the reference of the credential
/// that holds the user information (`native:proxy.http` or `native:proxy.https`).
const PROXY_ENDPOINT: &[ParamSpec] = &[
    ParamSpec::required("url", ParamType::String),
    ParamSpec::required("credential", ParamType::String),
];

/// A delivery host's proxies: the fields of `ProxySettings`.
const PROXY: &[ParamSpec] = &[
    ParamSpec::optional("http", ParamType::Record(PROXY_ENDPOINT)),
    ParamSpec::optional("https", ParamType::Record(PROXY_ENDPOINT)),
    ParamSpec::optional("no_proxy", ParamType::List(&ParamType::String)),
    ParamSpec::required("no_proxy_nonexact_match", ParamType::Bool),
    ParamSpec::required("use_proxy_for_cloud_metadata", ParamType::Bool),
];

/// The parameters of a delivery host's settings: the fields of `DeliverySettings`.
pub const HOST_SETTINGS: &[ParamSpec] = &[
    ParamSpec::required("endpoint_concurrency", COUNT),
    ParamSpec::required("endpoint_concurrency_multiplier", COUNT),
    ParamSpec::required("endpoint_buffer_size", COUNT),
    ParamSpec::required("request_timeout", ParamType::Duration),
    ParamSpec::required("retry", ParamType::Record(RETRY)),
    ParamSpec::required("proxy", ParamType::Record(PROXY)),
    ParamSpec::required("http_protocol", ParamType::Enum(&["auto", "http1"])),
    ParamSpec::required("connection_reset_interval", ParamType::Duration),
    ParamSpec::required("skip_ssl_validation", ParamType::Bool),
    ParamSpec::required("min_tls_version", ParamType::Enum(&["tls12", "tls13"])),
    ParamSpec::required("tls_handshake_timeout", ParamType::Duration),
    ParamSpec::required("api_key_validation_interval", ParamType::Duration),
];

/// Which metrics variants a recipient takes in their V3 form: the fields of `V3Acceptance`.
const V3: &[ParamSpec] = &[
    ParamSpec::required("series", ParamType::Bool),
    ParamSpec::required("sketches", ParamType::Bool),
];

/// The parameters of a recipient's protocol: `kind` names the variant of `RecipientProtocol`, and the other parameters
/// are its fields, which only the `datadog_intake` variant has (its authorities flattened as `logs_authority` and
/// `traces_authority`).
pub const RECIPIENT_PROTOCOL: &[ParamSpec] = &[
    ParamSpec::required("kind", ParamType::Enum(&["datadog_intake", "cluster_agent"])),
    ParamSpec::optional("logs_authority", ParamType::String),
    ParamSpec::optional("traces_authority", ParamType::String),
    ParamSpec::optional("allow_arbitrary_tags", ParamType::Bool),
    ParamSpec::optional("v3", ParamType::Record(V3)),
];

/// Returns the schema of a delivery host's parameters: these tables, and whether the host accepts recipients a
/// resources artifact declares, which only `dd_out` does.
pub fn host_schema(host: &FixedHost) -> HostSchema {
    HostSchema {
        encoder: ENCODER,
        settings: HOST_SETTINGS,
        protocol: RECIPIENT_PROTOCOL,
        declared_recipients: host.declared_recipients,
    }
}

/// Encodes an output binding's encoder settings as the parameters of [`ENCODER`].
///
/// # Errors
///
/// Returns an error naming the setting if a value has no parameter form: a size beyond the integers a plan holds, or a
/// flush timeout no plan duration represents.
pub fn encode_encoder(settings: &EncoderSettings) -> Result<Params, GenericError> {
    let variants = &settings.variants;
    params(json!({
        "variants": {
            "v2_series": variants.v2_series,
            "v2_sketches": variants.v2_sketches,
            "v3_series": variants.v3_series,
            "v3_sketches": variants.v3_sketches,
        },
        "series": match settings.series {
            SeriesVariant::V2 => "v2",
            SeriesVariant::V1 => "v1",
        },
        "compressor_kind": &*settings.compressor_kind,
        "zstd_compressor_level": settings.zstd_compressor_level,
        "v3_compression_level": settings.v3_compression_level,
        "max_metrics_per_payload": count("max_metrics_per_payload", settings.max_metrics_per_payload)?,
        "max_payload_size": count("max_payload_size", settings.max_payload_size)?,
        "max_uncompressed_payload_size":
            count("max_uncompressed_payload_size", settings.max_uncompressed_payload_size)?,
        "max_series_payload_size": count("max_series_payload_size", settings.max_series_payload_size)?,
        "max_series_uncompressed_payload_size":
            count("max_series_uncompressed_payload_size", settings.max_series_uncompressed_payload_size)?,
        "max_series_points_per_payload":
            count("max_series_points_per_payload", settings.max_series_points_per_payload)?,
        "flush_timeout": duration("flush_timeout", settings.flush_timeout)?,
        "log_payloads": settings.log_payloads,
    }))
}

/// Decodes an output binding's encoder parameters, written by [`encode_encoder`].
///
/// # Errors
///
/// Returns an error naming every parameter that does not match [`ENCODER`].
pub fn decode_encoder(params: &Params) -> Result<EncoderSettings, GenericError> {
    let decoded = decode("encoder", ENCODER, params)?;
    let variants = record(&decoded, "variants")?;
    Ok(EncoderSettings {
        variants: ProtocolVariants {
            v2_series: boolean(variants, "variants.v2_series")?,
            v2_sketches: boolean(variants, "variants.v2_sketches")?,
            v3_series: boolean(variants, "variants.v3_series")?,
            v3_sketches: boolean(variants, "variants.v3_sketches")?,
        },
        series: match string(&decoded, "series")? {
            "v1" => SeriesVariant::V1,
            _ => SeriesVariant::V2,
        },
        compressor_kind: string(&decoded, "compressor_kind")?.into(),
        zstd_compressor_level: integer(&decoded, "zstd_compressor_level")?,
        v3_compression_level: integer(&decoded, "v3_compression_level")?,
        max_metrics_per_payload: integer(&decoded, "max_metrics_per_payload")?,
        max_payload_size: integer(&decoded, "max_payload_size")?,
        max_uncompressed_payload_size: integer(&decoded, "max_uncompressed_payload_size")?,
        max_series_payload_size: integer(&decoded, "max_series_payload_size")?,
        max_series_uncompressed_payload_size: integer(&decoded, "max_series_uncompressed_payload_size")?,
        max_series_points_per_payload: integer(&decoded, "max_series_points_per_payload")?,
        flush_timeout: duration_of(&decoded, "flush_timeout")?,
        log_payloads: boolean(&decoded, "log_payloads")?,
    })
}

/// Encodes a delivery host's settings as the parameters of [`HOST_SETTINGS`].
///
/// Each proxy is its URL without user information and its credential reference, as the settings carry it: a host's
/// parameters never hold a credential's value.
///
/// # Errors
///
/// Returns an error naming the setting if a value has no parameter form: a size beyond the integers a plan holds, a
/// duration no plan duration represents, or a ratio that is not a finite number.
pub fn encode_host_settings(settings: &DeliverySettings) -> Result<Params, GenericError> {
    let retry = &settings.retry;
    let proxy = &settings.proxy;
    let endpoint = |endpoint: &ProxyEndpoint| json!({"url": endpoint.url, "credential": &*endpoint.credential});
    let mut proxies = json!({
        "no_proxy_nonexact_match": proxy.no_proxy_nonexact_match,
        "use_proxy_for_cloud_metadata": proxy.use_proxy_for_cloud_metadata,
    });
    if let Some(http) = &proxy.http {
        proxies["http"] = endpoint(http);
    }
    if let Some(https) = &proxy.https {
        proxies["https"] = endpoint(https);
    }
    if !proxy.no_proxy.is_empty() {
        proxies["no_proxy"] = json!(proxy.no_proxy);
    }
    params(json!({
        "endpoint_concurrency": count("endpoint_concurrency", settings.endpoint_concurrency)?,
        "endpoint_concurrency_multiplier":
            count("endpoint_concurrency_multiplier", settings.endpoint_concurrency_multiplier)?,
        "endpoint_buffer_size": count("endpoint_buffer_size", settings.endpoint_buffer_size)?,
        "request_timeout": duration("request_timeout", settings.request_timeout)?,
        "retry": {
            "backoff_factor": decimal("retry.backoff_factor", retry.backoff_factor)?,
            "backoff_base": decimal("retry.backoff_base", retry.backoff_base)?,
            "backoff_max": decimal("retry.backoff_max", retry.backoff_max)?,
            "recovery_error_decrease_factor": retry.recovery_error_decrease_factor,
            "recovery_reset": retry.recovery_reset,
            "queue_max_size_bytes": count("retry.queue_max_size_bytes", retry.queue_max_size_bytes)?,
            "storage_max_size_bytes": count("retry.storage_max_size_bytes", retry.storage_max_size_bytes)?,
            "flush_to_disk_mem_ratio": decimal("retry.flush_to_disk_mem_ratio", retry.flush_to_disk_mem_ratio)?,
            "storage_max_disk_ratio": decimal("retry.storage_max_disk_ratio", retry.storage_max_disk_ratio)?,
            "outdated_file_in_days": retry.outdated_file_in_days,
        },
        "proxy": proxies,
        "http_protocol": match settings.http_protocol {
            ForwarderHttpProtocol::Auto => "auto",
            ForwarderHttpProtocol::Http1 => "http1",
        },
        "connection_reset_interval": duration("connection_reset_interval", settings.connection_reset_interval)?,
        "skip_ssl_validation": settings.skip_ssl_validation,
        "min_tls_version": match settings.min_tls_version {
            TlsMinimumVersion::Tls12 => "tls12",
            TlsMinimumVersion::Tls13 => "tls13",
        },
        "tls_handshake_timeout": duration("tls_handshake_timeout", settings.tls_handshake_timeout)?,
        "api_key_validation_interval": duration("api_key_validation_interval", settings.api_key_validation_interval)?,
    }))
}

/// Decodes a delivery host's settings parameters, written by [`encode_host_settings`].
///
/// # Errors
///
/// Returns an error naming every parameter that does not match [`HOST_SETTINGS`].
pub fn decode_host_settings(params: &Params) -> Result<DeliverySettings, GenericError> {
    let decoded = decode("host settings", HOST_SETTINGS, params)?;
    let retry = record(&decoded, "retry")?;
    let proxy = record(&decoded, "proxy")?;
    let endpoint = |name: &str| -> Result<Option<ProxyEndpoint>, GenericError> {
        let Some(ParamValue::Record(endpoint)) = proxy.get(name) else {
            return Ok(None);
        };
        Ok(Some(ProxyEndpoint {
            url: string(endpoint, "proxy.url")?.to_string(),
            credential: string(endpoint, "proxy.credential")?.into(),
        }))
    };
    let no_proxy = match proxy.list("no_proxy") {
        Some(hosts) => hosts
            .iter()
            .map(|host| match host {
                ParamValue::String(host) => Ok(host.clone()),
                _ => Err(generic_error!(
                    "Parameter `proxy.no_proxy` holds something other than host names."
                )),
            })
            .collect::<Result<_, _>>()?,
        None => Vec::new(),
    };
    Ok(DeliverySettings {
        endpoint_concurrency: integer(&decoded, "endpoint_concurrency")?,
        endpoint_concurrency_multiplier: integer(&decoded, "endpoint_concurrency_multiplier")?,
        endpoint_buffer_size: integer(&decoded, "endpoint_buffer_size")?,
        request_timeout: duration_of(&decoded, "request_timeout")?,
        retry: RetrySettings {
            backoff_factor: decimal_of(retry, "retry.backoff_factor")?,
            backoff_base: decimal_of(retry, "retry.backoff_base")?,
            backoff_max: decimal_of(retry, "retry.backoff_max")?,
            recovery_error_decrease_factor: integer(retry, "retry.recovery_error_decrease_factor")?,
            recovery_reset: boolean(retry, "retry.recovery_reset")?,
            queue_max_size_bytes: integer(retry, "retry.queue_max_size_bytes")?,
            storage_max_size_bytes: integer(retry, "retry.storage_max_size_bytes")?,
            flush_to_disk_mem_ratio: decimal_of(retry, "retry.flush_to_disk_mem_ratio")?,
            storage_max_disk_ratio: decimal_of(retry, "retry.storage_max_disk_ratio")?,
            outdated_file_in_days: integer(retry, "retry.outdated_file_in_days")?,
        },
        proxy: ProxySettings {
            http: endpoint("http")?,
            https: endpoint("https")?,
            no_proxy,
            no_proxy_nonexact_match: boolean(proxy, "proxy.no_proxy_nonexact_match")?,
            use_proxy_for_cloud_metadata: boolean(proxy, "proxy.use_proxy_for_cloud_metadata")?,
        },
        http_protocol: match string(&decoded, "http_protocol")? {
            "http1" => ForwarderHttpProtocol::Http1,
            _ => ForwarderHttpProtocol::Auto,
        },
        connection_reset_interval: duration_of(&decoded, "connection_reset_interval")?,
        skip_ssl_validation: boolean(&decoded, "skip_ssl_validation")?,
        min_tls_version: match string(&decoded, "min_tls_version")? {
            "tls13" => TlsMinimumVersion::Tls13,
            _ => TlsMinimumVersion::Tls12,
        },
        tls_handshake_timeout: duration_of(&decoded, "tls_handshake_timeout")?,
        api_key_validation_interval: duration_of(&decoded, "api_key_validation_interval")?,
    })
}

/// Encodes a recipient's protocol as the parameters of [`RECIPIENT_PROTOCOL`].
pub fn encode_protocol(protocol: &RecipientProtocol) -> Params {
    let value = match protocol {
        RecipientProtocol::DatadogIntake {
            authorities,
            allow_arbitrary_tags,
            v3,
        } => {
            let mut value = json!({
                "kind": "datadog_intake",
                "allow_arbitrary_tags": allow_arbitrary_tags,
                "v3": {"series": v3.series, "sketches": v3.sketches},
            });
            if let Some(logs) = &authorities.logs {
                value["logs_authority"] = json!(&**logs);
            }
            if let Some(traces) = &authorities.traces {
                value["traces_authority"] = json!(&**traces);
            }
            value
        }
        RecipientProtocol::ClusterAgent => json!({"kind": "cluster_agent"}),
    };
    // Every member is a boolean, a record of booleans, or a string, so the parameters are always well formed.
    serde_json::from_value(value).unwrap_or_default()
}

/// Decodes a recipient's protocol parameters, written by [`encode_protocol`].
///
/// # Errors
///
/// Returns an error naming every parameter that does not match [`RECIPIENT_PROTOCOL`], a Datadog intake protocol
/// without its tag and V3 settings, or a Cluster Agent protocol with any parameter but `kind`.
pub fn decode_protocol(params: &Params) -> Result<RecipientProtocol, GenericError> {
    let decoded = decode("recipient protocol", RECIPIENT_PROTOCOL, params)?;
    match string(&decoded, "kind")? {
        "cluster_agent" => match decoded.iter().map(|(name, _)| name).find(|name| *name != "kind") {
            Some(name) => Err(generic_error!(
                "Parameter `{}` is not a parameter of the `cluster_agent` recipient protocol, which takes only `kind`.",
                name
            )),
            None => Ok(RecipientProtocol::ClusterAgent),
        },
        _ => {
            let v3 = record(&decoded, "v3")?;
            let authority = |name: &str| decoded.str(name).map(MetaString::from);
            Ok(RecipientProtocol::DatadogIntake {
                authorities: IntakeAuthorities {
                    logs: authority("logs_authority"),
                    traces: authority("traces_authority"),
                },
                allow_arbitrary_tags: boolean(&decoded, "allow_arbitrary_tags")?,
                v3: V3Acceptance {
                    series: boolean(v3, "v3.series")?,
                    sketches: boolean(v3, "v3.sketches")?,
                },
            })
        }
    }
}

/// Reads parameters written as a JSON object.
fn params(value: Value) -> Result<Params, GenericError> {
    serde_json::from_value(value).map_err(|error| generic_error!("Failed to write parameters: {}", error))
}

/// Writes a size or count, which a plan holds as an integer of at most [`MAX_SAFE_INTEGER`].
fn count<T: TryInto<i64> + Copy + fmt::Display>(name: &str, value: T) -> Result<Value, GenericError> {
    value
        .try_into()
        .ok()
        .filter(|value| *value <= MAX_SAFE_INTEGER)
        .map(Value::from)
        .ok_or_else(|| {
            generic_error!(
                "`{}` is {}, which the metrics plan cannot hold: a plan's integers are at most {}.",
                name,
                value,
                MAX_SAFE_INTEGER
            )
        })
}

/// Writes a duration in its canonical parameter form.
fn duration(name: &str, value: Duration) -> Result<Value, GenericError> {
    format_duration(value).map(Value::String).ok_or_else(|| {
        generic_error!(
            "`{}` is {:?}, which no plan duration represents: a plan duration is a whole number of at most {} of the \
             largest unit that divides it exactly.",
            name,
            value,
            MAX_SAFE_INTEGER
        )
    })
}

/// Writes a ratio or factor as a decimal, exactly: Rust writes the shortest decimal that reads back as the same
/// number.
fn decimal(name: &str, value: f64) -> Result<Value, GenericError> {
    let written = value.to_string();
    Decimal::new(&written)
        .map(|_| Value::String(written))
        .ok_or_else(|| generic_error!("`{}` is {}, which is not a finite number.", name, value))
}

/// Decodes `params` against `table`, naming every problem.
fn decode(what: &str, table: &[ParamSpec], params: &Params) -> Result<DecodedParams, GenericError> {
    decode_params(table, params).map_err(|errors| {
        let errors = errors.iter().map(ToString::to_string).collect::<Vec<_>>();
        generic_error!("The {} parameters are invalid: {}", what, errors.join("; "))
    })
}

fn missing(name: &str) -> GenericError {
    generic_error!("Parameter `{}` is missing or has no known value.", name)
}

/// Returns the last segment of a dotted parameter path: the member's name in its record.
fn member(path: &str) -> &str {
    path.rsplit('.').next().unwrap_or(path)
}

fn record<'a>(decoded: &'a DecodedParams, name: &str) -> Result<&'a DecodedParams, GenericError> {
    match decoded.known(member(name)) {
        Some(ParamValue::Record(record)) => Ok(record),
        _ => Err(missing(name)),
    }
}

fn boolean(decoded: &DecodedParams, name: &str) -> Result<bool, GenericError> {
    decoded.bool(member(name)).ok_or_else(|| missing(name))
}

fn string<'a>(decoded: &'a DecodedParams, name: &str) -> Result<&'a str, GenericError> {
    decoded.str(member(name)).ok_or_else(|| missing(name))
}

fn integer<T: TryFrom<i64>>(decoded: &DecodedParams, name: &str) -> Result<T, GenericError> {
    let value = decoded.integer(member(name)).ok_or_else(|| missing(name))?;
    T::try_from(value).map_err(|_| generic_error!("Parameter `{}` is {}, which is out of range.", name, value))
}

fn duration_of(decoded: &DecodedParams, name: &str) -> Result<Duration, GenericError> {
    decoded.duration(member(name)).ok_or_else(|| missing(name))
}

fn decimal_of(decoded: &DecodedParams, name: &str) -> Result<f64, GenericError> {
    match decoded.known(member(name)) {
        Some(ParamValue::Decimal(decimal)) => Ok(decimal.to_f64()),
        _ => Err(missing(name)),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use saluki_metric_plan::ir::is_identifier;

    use super::*;

    fn names(specs: &[ParamSpec]) -> Vec<&'static str> {
        specs.iter().map(|spec| spec.name).collect()
    }

    #[test]
    fn every_parameter_name_is_an_identifier_and_unique_in_its_table() {
        for table in [
            ENCODER,
            VARIANTS,
            HOST_SETTINGS,
            RETRY,
            PROXY,
            PROXY_ENDPOINT,
            RECIPIENT_PROTOCOL,
            V3,
        ] {
            let unique = names(table).into_iter().collect::<BTreeSet<_>>();
            assert_eq!(unique.len(), table.len());
            for name in unique {
                assert!(is_identifier(name), "{}", name);
            }
        }
    }

    #[test]
    fn only_the_datadog_delivery_host_accepts_declared_recipients() {
        use crate::programmable_metrics::resources::{CLUSTER_AGENT_HOST, DD_HOST, MRF_HOST};

        assert!(host_schema(&DD_HOST).declared_recipients);
        assert!(!host_schema(&MRF_HOST).declared_recipients);
        assert!(!host_schema(&CLUSTER_AGENT_HOST).declared_recipients);
        assert_eq!(host_schema(&DD_HOST).settings, HOST_SETTINGS);
    }

    #[test]
    fn the_tables_decode_parameters_written_with_their_names() {
        let encoder = serde_json::from_value::<Params>(json!({
            "variants": {"v2_series": true, "v2_sketches": true, "v3_series": false, "v3_sketches": false},
            "series": "v2",
            "compressor_kind": "zstd",
            "zstd_compressor_level": 3,
            "v3_compression_level": 0,
            "max_metrics_per_payload": 10000,
            "max_payload_size": 2621440,
            "max_uncompressed_payload_size": 4194304,
            "max_series_payload_size": 512000,
            "max_series_uncompressed_payload_size": 5242880,
            "max_series_points_per_payload": 10000,
            "flush_timeout": "2s",
            "log_payloads": false
        }))
        .unwrap();
        assert!(decode_params(ENCODER, &encoder).is_ok());

        let cluster_agent = serde_json::from_value::<Params>(json!({"kind": "cluster_agent"})).unwrap();
        assert!(decode_params(RECIPIENT_PROTOCOL, &cluster_agent).is_ok());
        let unknown = serde_json::from_value::<Params>(json!({"kind": "cluster_agent", "token": "x"})).unwrap();
        assert!(decode_params(RECIPIENT_PROTOCOL, &unknown).is_err());
    }

    #[test]
    fn each_proxy_is_a_url_without_user_information_and_the_credential_reference_that_holds_it() {
        let proxy = |proxy: serde_json::Value| serde_json::from_value::<Params>(proxy).unwrap();
        let proxies = proxy(json!({
            "http": {"url": "http://proxy.example.com:3128", "credential": "native:proxy.http"},
            "https": {"url": "http://proxy.example.com:3129", "credential": "native:proxy.https"},
            "no_proxy": ["169.254.169.254"],
            "no_proxy_nonexact_match": false,
            "use_proxy_for_cloud_metadata": false
        }));
        assert!(decode_params(PROXY, &proxies).is_ok());
        let unproxied = proxy(json!({"no_proxy_nonexact_match": false, "use_proxy_for_cloud_metadata": false}));
        assert!(decode_params(PROXY, &unproxied).is_ok());

        // A proxy without its credential reference, or with its user information in place of one, is refused.
        let unreferenced = proxy(json!({
            "https": {"url": "http://proxy.example.com:3129"},
            "no_proxy_nonexact_match": false,
            "use_proxy_for_cloud_metadata": false
        }));
        assert!(decode_params(PROXY, &unreferenced).is_err());
        let password = proxy(json!({
            "https": {"url": "http://proxy.example.com:3129", "credential": "native:proxy.https", "password": "x"},
            "no_proxy_nonexact_match": false,
            "use_proxy_for_cloud_metadata": false
        }));
        assert!(decode_params(PROXY, &password).is_err());
    }

    fn params_of(value: Value) -> Params {
        serde_json::from_value(value).unwrap()
    }

    fn encoder_settings() -> EncoderSettings {
        EncoderSettings {
            variants: ProtocolVariants {
                v2_series: true,
                v2_sketches: true,
                v3_series: true,
                v3_sketches: false,
            },
            series: SeriesVariant::V2,
            compressor_kind: "zstd".into(),
            zstd_compressor_level: 3,
            v3_compression_level: 0,
            max_metrics_per_payload: 10_000,
            max_payload_size: 2_621_440,
            max_uncompressed_payload_size: 4_194_304,
            max_series_payload_size: 512_000,
            max_series_uncompressed_payload_size: 5_242_880,
            max_series_points_per_payload: 10_000,
            flush_timeout: Duration::from_millis(2_500),
            log_payloads: false,
        }
    }

    fn host_settings() -> DeliverySettings {
        DeliverySettings {
            endpoint_concurrency: 10,
            endpoint_concurrency_multiplier: 1,
            endpoint_buffer_size: 100,
            request_timeout: Duration::from_secs(20),
            retry: RetrySettings {
                backoff_factor: 2.0,
                backoff_base: 2.0,
                backoff_max: 64.0,
                recovery_error_decrease_factor: 2,
                recovery_reset: false,
                queue_max_size_bytes: 15_728_640,
                storage_max_size_bytes: 0,
                flush_to_disk_mem_ratio: 0.5,
                storage_max_disk_ratio: 0.8,
                outdated_file_in_days: 10,
            },
            proxy: ProxySettings {
                http: None,
                https: Some(ProxyEndpoint {
                    url: "http://proxy.example.com:3129".to_string(),
                    credential: ProxySettings::NATIVE_HTTPS_CREDENTIAL.into(),
                }),
                no_proxy: vec!["internal.example.com".to_string()],
                no_proxy_nonexact_match: true,
                use_proxy_for_cloud_metadata: false,
            },
            http_protocol: ForwarderHttpProtocol::Http1,
            connection_reset_interval: Duration::ZERO,
            skip_ssl_validation: false,
            min_tls_version: TlsMinimumVersion::Tls13,
            tls_handshake_timeout: Duration::from_secs(10),
            api_key_validation_interval: Duration::from_secs(3_600),
        }
    }

    #[test]
    fn encoder_settings_are_written_with_their_field_names_and_read_back_unchanged() {
        let settings = encoder_settings();
        let encoded = encode_encoder(&settings).unwrap();
        assert_eq!(
            encoded,
            params_of(json!({
                "variants": {"v2_series": true, "v2_sketches": true, "v3_series": true, "v3_sketches": false},
                "series": "v2",
                "compressor_kind": "zstd",
                "zstd_compressor_level": 3,
                "v3_compression_level": 0,
                "max_metrics_per_payload": 10000,
                "max_payload_size": 2621440,
                "max_uncompressed_payload_size": 4194304,
                "max_series_payload_size": 512000,
                "max_series_uncompressed_payload_size": 5242880,
                "max_series_points_per_payload": 10000,
                "flush_timeout": "2500ms",
                "log_payloads": false
            }))
        );
        assert!(decode_params(ENCODER, &encoded).is_ok());
        assert_eq!(decode_encoder(&encoded).unwrap(), settings);

        let v1 = EncoderSettings {
            series: SeriesVariant::V1,
            log_payloads: true,
            ..encoder_settings()
        };
        assert_eq!(decode_encoder(&encode_encoder(&v1).unwrap()).unwrap(), v1);
    }

    #[test]
    fn host_settings_are_written_with_proxies_by_reference_and_read_back_unchanged() {
        let settings = host_settings();
        let encoded = encode_host_settings(&settings).unwrap();
        assert_eq!(
            encoded,
            params_of(json!({
                "endpoint_concurrency": 10,
                "endpoint_concurrency_multiplier": 1,
                "endpoint_buffer_size": 100,
                "request_timeout": "20s",
                "retry": {
                    "backoff_factor": "2",
                    "backoff_base": "2",
                    "backoff_max": "64",
                    "recovery_error_decrease_factor": 2,
                    "recovery_reset": false,
                    "queue_max_size_bytes": 15728640,
                    "storage_max_size_bytes": 0,
                    "flush_to_disk_mem_ratio": "0.5",
                    "storage_max_disk_ratio": "0.8",
                    "outdated_file_in_days": 10
                },
                "proxy": {
                    "https": {"url": "http://proxy.example.com:3129", "credential": "native:proxy.https"},
                    "no_proxy": ["internal.example.com"],
                    "no_proxy_nonexact_match": true,
                    "use_proxy_for_cloud_metadata": false
                },
                "http_protocol": "http1",
                "connection_reset_interval": "0s",
                "skip_ssl_validation": false,
                "min_tls_version": "tls13",
                "tls_handshake_timeout": "10s",
                "api_key_validation_interval": "1h"
            }))
        );
        assert!(decode_params(HOST_SETTINGS, &encoded).is_ok());
        assert_eq!(decode_host_settings(&encoded).unwrap(), settings);

        // Without proxies, the defaults.
        let mut unproxied = host_settings();
        unproxied.proxy = ProxySettings::default();
        unproxied.http_protocol = ForwarderHttpProtocol::Auto;
        unproxied.min_tls_version = TlsMinimumVersion::Tls12;
        unproxied.retry.backoff_factor = 0.1 + 0.2;
        let encoded = encode_host_settings(&unproxied).unwrap();
        assert_eq!(
            encoded.get("proxy"),
            Some(&json!({"no_proxy_nonexact_match": false, "use_proxy_for_cloud_metadata": false}))
        );
        let decoded = decode_host_settings(&encoded).unwrap();
        assert_eq!(
            decoded, unproxied,
            "a factor that is not a short decimal reads back exactly"
        );
    }

    #[test]
    fn recipient_protocols_are_written_under_their_kind_and_read_back_unchanged() {
        let intake = RecipientProtocol::DatadogIntake {
            authorities: IntakeAuthorities {
                logs: Some("agent-http-intake.logs.datadoghq.com".into()),
                traces: None,
            },
            allow_arbitrary_tags: true,
            v3: V3Acceptance {
                series: true,
                sketches: false,
            },
        };
        let encoded = encode_protocol(&intake);
        assert_eq!(
            encoded,
            params_of(json!({
                "kind": "datadog_intake",
                "logs_authority": "agent-http-intake.logs.datadoghq.com",
                "allow_arbitrary_tags": true,
                "v3": {"series": true, "sketches": false}
            }))
        );
        assert_eq!(decode_protocol(&encoded).unwrap(), intake);
        let cluster_agent = encode_protocol(&RecipientProtocol::ClusterAgent);
        assert_eq!(cluster_agent, params_of(json!({"kind": "cluster_agent"})));
        assert_eq!(
            decode_protocol(&cluster_agent).unwrap(),
            RecipientProtocol::ClusterAgent
        );

        // A Datadog intake protocol needs its tag and V3 settings; the Cluster Agent's takes nothing else.
        let incomplete = params_of(json!({"kind": "datadog_intake", "allow_arbitrary_tags": false}));
        assert_eq!(
            decode_protocol(&incomplete).unwrap_err().to_string(),
            "Parameter `v3` is missing or has no known value."
        );
        let extra = params_of(json!({"kind": "cluster_agent", "allow_arbitrary_tags": false}));
        assert!(decode_protocol(&extra)
            .unwrap_err()
            .to_string()
            .starts_with("Parameter `allow_arbitrary_tags` is not a parameter of the `cluster_agent`"));
    }

    #[test]
    fn values_without_a_parameter_form_are_refused_naming_the_setting() {
        let mut settings = encoder_settings();
        settings.max_payload_size = usize::MAX;
        assert!(encode_encoder(&settings)
            .unwrap_err()
            .to_string()
            .starts_with("`max_payload_size` is 18446744073709551615, which the metrics plan cannot hold"));
        let mut settings = encoder_settings();
        settings.flush_timeout = Duration::MAX;
        assert!(encode_encoder(&settings)
            .unwrap_err()
            .to_string()
            .starts_with("`flush_timeout` is"));
        let mut settings = host_settings();
        settings.retry.storage_max_disk_ratio = f64::NAN;
        assert_eq!(
            encode_host_settings(&settings).unwrap_err().to_string(),
            "`retry.storage_max_disk_ratio` is NaN, which is not a finite number."
        );

        // Parameters that do not match the table name each problem.
        let invalid = params_of(json!({"kind": "smtp"}));
        assert!(decode_protocol(&invalid)
            .unwrap_err()
            .to_string()
            .starts_with("The recipient protocol parameters are invalid: Invalid parameter `kind`"));
    }
}
