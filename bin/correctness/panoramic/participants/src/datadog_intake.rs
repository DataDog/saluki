//! The mock intake's listeners and Linux container recipe.

use std::{collections::BTreeMap, net::SocketAddr};

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use panoramic_core::{
    artifact::ArtifactRole,
    endpoint::{Endpoint, Evidence, Port},
    layout::{Os, Realization, RealizationKind},
    participant::{
        CaseValues, EffectiveConfig, ExitChannel, OpContext, Operation, OutputChannel, ParticipantType, ProcessSpec,
        RenderedConfig,
    },
    recipe::{ContainerRecipe, Recipe},
    step::HandleError,
};
use serde_json::Value;

use crate::TOOLS_LINUX_IMAGE;

/// The mock Datadog intake, with independent telemetry storage per participant.
///
/// The `dump` operation accepts exactly `{"kind": "metrics"}` or `{"kind": "dogstatsd_forwarded"}`.
/// Metrics retain the intake's JSON array of serialized `stele::Metric` values. Forwarded packets are
/// arrays of byte values, one array per packet, in receipt order.
pub struct DatadogIntake;

impl DatadogIntake {
    /// Participant type name used in case files.
    pub const NAME: &'static str = "datadog-intake";
}

impl ParticipantType for DatadogIntake {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn supports(&self) -> &'static [(Os, RealizationKind)] {
        &[(Os::Linux, RealizationKind::LinuxContainer)]
    }

    fn artifacts(&self, _: Realization) -> &'static [ArtifactRole] {
        &[TOOLS_LINUX_IMAGE]
    }

    fn recipe(&self, _: Realization, config: &RenderedConfig) -> Result<Recipe, String> {
        Ok(Recipe::Container(
            ContainerRecipe::new(TOOLS_LINUX_IMAGE)
                .with_entrypoint(["/usr/local/bin/datadog-intake"])
                .with_env(config.env().clone()),
        ))
    }

    fn validate_config(&self, _: Realization, config: &CaseValues) -> Result<(), String> {
        listen_ports(&config.env).map(|_| ())
    }

    fn endpoints(&self, _: Realization, config: &EffectiveConfig) -> Result<BTreeMap<String, Endpoint>, String> {
        let [http, dogstatsd] = listen_ports(&config.env)?;
        // The intake binds UDP before HTTP, so /ready covers both listeners.
        let ready = Evidence::http_ok("api", "/ready");
        Ok(BTreeMap::from([
            (
                "api".into(),
                Endpoint::http(Port::Fixed(http)).with_ready(ready.clone()),
            ),
            (
                "dogstatsd_forward".into(),
                Endpoint::udp(Port::Fixed(dogstatsd)).with_ready(ready),
            ),
        ]))
    }

    fn operations(&self, _: Realization) -> &'static [&'static dyn Operation] {
        &[&Dump]
    }

    fn processes(&self) -> &'static [&'static str] {
        &["intake"]
    }

    fn process_specs(&self, _: Realization) -> &'static [ProcessSpec] {
        &[ProcessSpec {
            name: "intake",
            exit: ExitChannel::Container,
            output: OutputChannel::Container,
        }]
    }
}

struct Dump;

#[async_trait]
impl Operation for Dump {
    fn name(&self) -> &'static str {
        "dump"
    }

    fn http_endpoints(&self) -> &'static [&'static str] {
        &["api"]
    }

    async fn run(&self, cx: &OpContext<'_>, args: Value) -> Result<Value, HandleError> {
        let kind = args
            .as_object()
            .filter(|args| args.len() == 1)
            .and_then(|args| args.get("kind"))
            .and_then(Value::as_str);
        let path = match kind {
            Some("metrics") => "/metrics/dump",
            Some("dogstatsd_forwarded") => "/dogstatsd-forwarding/dump",
            _ => {
                return Err(HandleError::Harness {
                    message: "dump requires exactly {\"kind\": \"metrics\"|\"dogstatsd_forwarded\"}".into(),
                })
            }
        };
        let response = cx.http_get("api", path).await?;
        if !(200..300).contains(&response.status) {
            return Err(HandleError::Product {
                message: format!("intake dump at '{path}' returned HTTP {}", response.status),
            });
        }
        let values: Vec<Value> = serde_json::from_slice(&response.body).map_err(|_| HandleError::Product {
            message: format!("intake dump at '{path}' returned an invalid JSON array"),
        })?;
        if kind == Some("metrics") {
            return Ok(Value::Array(values));
        }
        let packets: Result<Vec<Vec<u8>>, HandleError> = values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                let encoded = value.as_str().ok_or_else(|| HandleError::Product {
                    message: format!("intake dump at '{path}' returned a non-string packet at index {index}"),
                })?;
                STANDARD.decode(encoded).map_err(|_| HandleError::Product {
                    message: format!("intake dump at '{path}' returned invalid base64 at packet index {index}"),
                })
            })
            .collect();
        Ok(serde_json::json!(packets?))
    }
}

fn listen_ports(env: &BTreeMap<String, String>) -> Result<[u16; 2], String> {
    let mut ports = [0; 2];
    for (index, (key, default)) in [
        ("INTAKE_HTTP_LISTEN_ADDR", "0.0.0.0:2049"),
        ("INTAKE_DOGSTATSD_LISTEN_ADDR", "0.0.0.0:9125"),
    ]
    .into_iter()
    .enumerate()
    {
        let value = env.get(key).map_or(default, String::as_str);
        let address: SocketAddr = value
            .parse()
            .map_err(|error| format!("{key}='{value}' must be a listen address in host:port form: {error}"))?;
        if address.port() == 0 {
            return Err(format!("{key}='{value}' must use a nonzero port"));
        }
        if !address.ip().is_unspecified() {
            return Err(format!(
                "{key}='{value}' must bind 0.0.0.0 or [::] so other participants and published ports can reach it"
            ));
        }
        ports[index] = address.port();
    }
    Ok(ports)
}

#[cfg(test)]
mod tests {
    use std::{io::ErrorKind, time::Duration};

    use panoramic_core::{
        endpoint::{test_support::assert_network_endpoint, EndpointKind},
        participant::test_support::run_http_operation,
        recipe::test_support::{assert_container, rendered},
    };
    use serde_json::json;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::*;

    const LINUX: Realization = Realization {
        os: Os::Linux,
        kind: RealizationKind::LinuxContainer,
    };

    async fn dump_response(kind: &str, path: &str, status: u16, body: &[u8]) -> Result<Value, HandleError> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let serve = async {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let length = stream.read(&mut buffer).await.unwrap();
                assert_ne!(length, 0, "connection closed before request headers");
                request.extend_from_slice(&buffer[..length]);
            }
            let request = String::from_utf8(request).unwrap();
            assert!(request.starts_with(&format!("GET {path} HTTP/1.1\r\n")), "{request}");
            let headers = format!(
                "HTTP/1.1 {status} response\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(headers.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
            stream.shutdown().await.unwrap();
        };
        let (_, result) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(serve, run_http_operation(&Dump, json!({"kind": kind}), port))
        })
        .await
        .expect("dump and test HTTP server must finish");
        result
    }

    #[test]
    fn the_intake_offers_dump_on_every_realization_and_requests_only_api() {
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            for kind in [
                RealizationKind::LinuxContainer,
                RealizationKind::WindowsContainer,
                RealizationKind::HostProcessGroup,
                RealizationKind::KubernetesPod,
            ] {
                let operations = DatadogIntake.operations(Realization { os, kind });
                assert_eq!(operations.len(), 1, "{os:?} {kind:?}");
                assert_eq!(operations[0].name(), "dump");
                assert_eq!(operations[0].http_endpoints(), &["api"]);
            }
        }
    }

    #[tokio::test]
    async fn dump_returns_metrics_json_unchanged_for_successful_statuses() {
        let metrics = json!([
            {
                "context": {"name": "example.counter", "tags": ["z:last", "a:first", "a:first"]},
                "values": [[123, {"mtype": "Count", "value": 3.5}], [456, {"mtype": "Count", "value": 1.0}]],
                "extra": {"preserved": true}
            },
            {
                "context": {"name": "example.rate", "tags": []},
                "values": [[789, {"mtype": "Rate", "value": 0.25, "interval": 10}]]
            }
        ]);
        for status in [200, 201, 206, 299] {
            let actual = dump_response(
                "metrics",
                "/metrics/dump",
                status,
                &serde_json::to_vec(&metrics).unwrap(),
            )
            .await
            .unwrap();
            assert_eq!(actual, metrics, "HTTP {status}");
        }
    }

    #[tokio::test]
    async fn dump_decodes_forwarded_packets_preserving_bytes_order_and_duplicates() {
        let actual = dump_response(
            "dogstatsd_forwarded",
            "/dogstatsd-forwarding/dump",
            200,
            br#"["bWV0cmljOjF8Yw==", "AJ+Slv8=", "", "bWV0cmljOjF8Yw=="]"#,
        )
        .await
        .unwrap();
        let expected: Vec<Vec<u8>> = vec![
            b"metric:1|c".to_vec(),
            vec![0, 159, 146, 150, 255],
            vec![],
            b"metric:1|c".to_vec(),
        ];
        assert_eq!(actual, json!(expected));
    }

    #[tokio::test]
    async fn dump_returns_empty_snapshots_for_both_kinds() {
        for (kind, path) in [
            ("metrics", "/metrics/dump"),
            ("dogstatsd_forwarded", "/dogstatsd-forwarding/dump"),
        ] {
            assert_eq!(dump_response(kind, path, 200, b"[]").await.unwrap(), json!([]));
        }
    }

    #[tokio::test]
    async fn dump_rejects_every_argument_shape_except_one_known_kind_before_requesting() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        for args in [
            Value::Null,
            json!("metrics"),
            json!(["metrics"]),
            json!({}),
            json!({"kind": "unknown-kind-secret"}),
            json!({"kind": "service_checks"}),
            json!({"kind": null}),
            json!({"kind": true}),
            json!({"kind": 1}),
            json!({"kind": ["metrics"]}),
            json!({"kind": {"metrics": true}}),
            json!({"kind": "metrics", "extra": "argument-secret"}),
            json!({"kind": "dogstatsd_forwarded", "extra": "argument-secret"}),
            json!({"extra": "argument-secret"}),
        ] {
            let error = run_http_operation(&Dump, args.clone(), port).await.unwrap_err();
            assert_eq!(
                error,
                HandleError::Harness {
                    message: "dump requires exactly {\"kind\": \"metrics\"|\"dogstatsd_forwarded\"}".into(),
                },
                "{args}"
            );
        }
        assert_eq!(listener.accept().unwrap_err().kind(), ErrorKind::WouldBlock);
    }

    #[tokio::test]
    async fn dump_reports_unsuccessful_statuses_without_disclosing_the_body() {
        for (kind, path) in [
            ("metrics", "/metrics/dump"),
            ("dogstatsd_forwarded", "/dogstatsd-forwarding/dump"),
        ] {
            for status in [300, 301, 400, 404, 500, 503] {
                let error = dump_response(kind, path, status, b"body-secret").await.unwrap_err();
                assert_eq!(
                    error,
                    HandleError::Product {
                        message: format!("intake dump at '{path}' returned HTTP {status}"),
                    }
                );
            }
        }
    }

    #[tokio::test]
    async fn dump_reports_invalid_json_arrays_without_disclosing_the_body() {
        for (kind, path) in [
            ("metrics", "/metrics/dump"),
            ("dogstatsd_forwarded", "/dogstatsd-forwarding/dump"),
        ] {
            for body in [
                b"body-secret".as_slice(),
                b"[\"body-secret\",]",
                b"[\"body-secret\"] trailing",
                b"\xffbody-secret",
                b"",
                b"null",
                b"{\"body-secret\": true}",
                b"\"body-secret\"",
            ] {
                let error = dump_response(kind, path, 200, body).await.unwrap_err();
                assert_eq!(
                    error,
                    HandleError::Product {
                        message: format!("intake dump at '{path}' returned an invalid JSON array"),
                    }
                );
            }
        }
    }

    #[tokio::test]
    async fn dump_rejects_non_string_and_invalid_base64_packets_without_disclosing_them() {
        for (packet, expected) in [
            (json!(null), "a non-string packet at index 1"),
            (json!(123), "a non-string packet at index 1"),
            (json!([1, 2, 3]), "a non-string packet at index 1"),
            (json!({"body-secret": true}), "a non-string packet at index 1"),
            (json!("body-secret"), "invalid base64 at packet index 1"),
            (json!("YQ"), "invalid base64 at packet index 1"),
            (json!("YQ==\n"), "invalid base64 at packet index 1"),
        ] {
            let body = serde_json::to_vec(&json!(["bWV0cmljOjF8Yw==", packet])).unwrap();
            let error = dump_response("dogstatsd_forwarded", "/dogstatsd-forwarding/dump", 200, &body)
                .await
                .unwrap_err();
            assert_eq!(
                error,
                HandleError::Product {
                    message: format!("intake dump at '/dogstatsd-forwarding/dump' returned {expected}"),
                }
            );
        }
    }

    #[test]
    fn the_intake_listens_on_historical_ports_unless_its_environment_overrides_them() {
        for (env, ports) in [
            (BTreeMap::new(), [2049, 9125]),
            (
                BTreeMap::from([
                    ("INTAKE_HTTP_LISTEN_ADDR".into(), "0.0.0.0:8080".into()),
                    ("INTAKE_DOGSTATSD_LISTEN_ADDR".into(), "[::]:8125".into()),
                ]),
                [8080, 8125],
            ),
        ] {
            DatadogIntake
                .validate_config(
                    LINUX,
                    &CaseValues {
                        env: env.clone(),
                        ..Default::default()
                    },
                )
                .unwrap();
            let endpoints = DatadogIntake
                .endpoints(
                    LINUX,
                    &EffectiveConfig {
                        env,
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(endpoints.len(), 2);
            for (name, kind, port) in [
                ("api", EndpointKind::Http, ports[0]),
                ("dogstatsd_forward", EndpointKind::Udp, ports[1]),
            ] {
                assert_network_endpoint(&endpoints[name], kind, port, Some(Evidence::http_ok("api", "/ready")));
            }
        }
    }

    #[test]
    fn an_intake_listen_address_must_have_a_nonzero_port_and_an_unspecified_host() {
        for key in ["INTAKE_HTTP_LISTEN_ADDR", "INTAKE_DOGSTATSD_LISTEN_ADDR"] {
            for (value, expected) in [
                ("0.0.0.0", "host:port"),
                ("0.0.0.0:invalid", "host:port"),
                ("0.0.0.0:65536", "host:port"),
                ("0.0.0.0:0", "nonzero port"),
                ("127.0.0.1:2049", "must bind 0.0.0.0 or [::]"),
                ("[::1]:2049", "must bind 0.0.0.0 or [::]"),
            ] {
                let env = BTreeMap::from([(key.into(), value.into())]);
                let error = DatadogIntake
                    .validate_config(
                        LINUX,
                        &CaseValues {
                            env: env.clone(),
                            ..Default::default()
                        },
                    )
                    .unwrap_err();
                assert!(
                    error.contains(key) && error.contains(value) && error.contains(expected),
                    "{error}"
                );
                assert_eq!(
                    DatadogIntake
                        .endpoints(
                            LINUX,
                            &EffectiveConfig {
                                env,
                                ..Default::default()
                            }
                        )
                        .unwrap_err(),
                    error
                );
            }
        }
    }

    #[test]
    fn the_intake_runs_one_process_observed_through_its_container() {
        assert_eq!(DatadogIntake.processes(), &["intake"]);
        assert_eq!(
            DatadogIntake.process_specs(LINUX),
            &[ProcessSpec {
                name: "intake",
                exit: ExitChannel::Container,
                output: OutputChannel::Container,
            }]
        );
    }

    #[test]
    fn the_intake_recipe_runs_the_tools_binary_with_the_case_environment() {
        let env = BTreeMap::from([("INTAKE_HTTP_LISTEN_ADDR".into(), "0.0.0.0:8080".into())]);
        let config = rendered(env.clone(), BTreeMap::new());
        let recipe = DatadogIntake.recipe(LINUX, &config).unwrap();
        assert_container(&recipe, TOOLS_LINUX_IMAGE, &["/usr/local/bin/datadog-intake"], &env);
    }

    #[test]
    fn the_intake_supports_only_linux_containers_and_declares_only_the_tools_image() {
        assert_eq!(
            DatadogIntake.supports(),
            &[(Os::Linux, RealizationKind::LinuxContainer)]
        );
        assert_eq!(DatadogIntake.artifacts(LINUX), &[TOOLS_LINUX_IMAGE]);
        assert!(DatadogIntake.facts(LINUX).is_empty());
        assert!(DatadogIntake.consumes(LINUX, &EffectiveConfig::default()).is_empty());
        assert!(DatadogIntake.readiness(LINUX, &EffectiveConfig::default()).is_empty());
        assert!(DatadogIntake.config_files().is_empty());
        assert!(DatadogIntake.params().is_empty());
    }
}
