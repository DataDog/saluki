use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::Mutex;

use anyhow::Context;
use bytes::Bytes;
use http::uri::Scheme;
use regex::Regex;

use anyhow::{anyhow, bail};
use thiserror::Error;

use http::{uri, HeaderMap, HeaderValue, Method, Request, Response};
use http_body_util::{BodyExt, Full};
use x509_cert::der::Decode;

use hyper::body::{self, Incoming};
use hyper_tls::{native_tls, HttpsConnector};

use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::TokioExecutor;

use tokio::net::TcpStream;
use tokio::time;

use async_trait::async_trait;

use integration_check::{check::Check, sink::Sink, Mapping};
use integration_check::{
    log, metric,
    service_check::{self, ServiceCheck},
};
use integration_check::{GenericError, Result};

use super::config::{Init, Instance};

use super::config::{self, defaults};

const MAX_CONTENT_LEN: usize = 20;
const SUPPORTED_SCHEME: [&str; 2] = ["http", "https"];
const DATA_METHODS: [http::Method; 5] = [
    Method::POST,
    Method::PUT,
    Method::DELETE,
    Method::PATCH,
    Method::OPTIONS,
];
const MESSAGE_LENGTH: usize = 2500;

#[derive(Error, Debug)]
pub enum IOErr {
    #[error("connection error")]
    Connect(GenericError),
    #[error("connection timeout")]
    Timeout,
    #[error("error: {0}")]
    Generic(GenericError),
}

enum SvcCheckEvent {
    Status,
    SSLCert,
}

enum SvcCheckMessage {
    WithContent(String),
    WithoutContent(String),
}
struct LightServiceCheck {
    event: SvcCheckEvent,
    status: service_check::Status,
    message: SvcCheckMessage,
}

struct State {
    service_checks: Vec<LightServiceCheck>,
    tags: HashMap<String, String>,
}

pub struct HttpCheck<S>
where
    S: Sink + Send + Sync,
{
    sink: S,
    instance_config: config::Instance,
    init_config: config::Init,
    state: Mutex<State>,
}

#[async_trait]
impl<S> Check for HttpCheck<S>
where
    S: Sink + Send + Sync,
{
    type Snk = S;

    fn build(sink: S, init_config: Mapping, instance_config: Mapping) -> Result<Self>
    where
        S: Sink + Send + Sync,
    {
        let init_config: Init =
            serde_yaml::from_value(serde_yaml::Value::Mapping(init_config)).context("Failed to parse init_config")?;
        let instance_config: Instance = serde_yaml::from_value(serde_yaml::Value::Mapping(instance_config))
            .context("Failed to parse instance_config")?;
        Ok(HttpCheck::<S>::new(sink, init_config, instance_config))
    }

    async fn run(&self) -> Result<()> {
        self.check().await
    }
}

impl<S> HttpCheck<S>
where
    S: Sink + Send + Sync,
{
    // FIXME name
    pub fn new(sink: S, init_config: config::Init, instance_config: config::Instance) -> Self {
        let state = State {
            service_checks: vec![],
            tags: HashMap::<String, String>::new(),
        };
        Self {
            sink,
            instance_config,
            init_config,
            state: Mutex::new(state),
        }
    }

    pub async fn check(&self) -> Result<()> {
        if let Err(err) = self.check_impl().await {
            self.sink.log(log::Level::Error, err.to_string()).await
        }
        Ok(())
    }

    async fn check_impl(&self) -> Result<()> {
        let url = self.instance_config.url.clone();
        let valid_url = url.scheme_str().is_some_and(|s| SUPPORTED_SCHEME.contains(&s)) && url.host().is_some();
        if !valid_url {
            bail!("Invalid URL: {}", url);
        }

        let mut service_tags = HashMap::<String, String>::new();
        if let Some(tags) = &self.instance_config.tags {
            self.state.lock().await.tags = tags.clone();
            service_tags = tags.clone();
        }

        let normalized_name = normalize_tag(&self.instance_config.name);
        self.state
            .lock()
            .await
            .tags
            .insert("instance".to_string(), normalized_name.clone());
        service_tags.insert("instance".to_string(), normalized_name);

        if !self.state.lock().await.tags.contains_key("url") {
            self.state
                .lock()
                .await
                .tags
                .insert("url".to_string(), self.instance_config.url.to_string());
        }
        if !service_tags.contains_key("url") {
            service_tags.insert("url".to_string(), self.instance_config.url.to_string());
        }

        if url.scheme_str().is_some_and(|s| s == "https")
            && self.instance_config.tls_verify.is_some_and(|v| !v)
            && !self.instance_config.tls_ignore_warning.is_some_and(|v| v)
        {
            self.sink
                .log(
                    log::Level::Debug,
                    format!(
                        "An unverified HTTPS request is being made to {}",
                        self.instance_config.url
                    ),
                )
                .await
        }

        let tls = self.make_tls_connector()?; // TODO don't need it for http
        let request = self.make_request()?;

        self.sink.log(log::Level::Debug, format!("Connecting to {url}")).await;

        let start_time = Instant::now();
        let elapsed = || Instant::now().duration_since(start_time);

        let maybe_response = self.http(tls, request).await;
        if let Err(err) = maybe_response.as_ref() {
            let elapsed = elapsed().as_millis();
            self.sink
                .log(
                    log::Level::Info,
                    format!(
                        "{} is DOWN, error: {}. Connection failed after {} ms",
                        self.instance_config.url.to_string(),
                        err.to_string(),
                        elapsed
                    ),
                )
                .await;
            self.add_service_check(
                SvcCheckEvent::Status,
                service_check::Status::Critical,
                SvcCheckMessage::WithoutContent(format!("{}. Connection failed after {} ms", err.to_string(), elapsed)), // TODO capitalize first later
            )
            .await;
        }

        if let Ok((mut response, maybe_certificate)) = maybe_response {
            let total_time = elapsed();

            if self
                .instance_config
                .collect_response_time
                .unwrap_or(defaults::COLLECT_RESPONSE_TIME)
            {
                self.gauge("network.http.response_time", (total_time.as_millis() as f64) / 1000.)
                    .await
            }

            if let Err(err) = self.handle_response(&mut response).await {
                self.sink
                    .log(
                        log::Level::Error,
                        format!(
                            "Error reading response: {}. Connection failed after {} ms",
                            err.to_string(),
                            total_time.as_millis()
                        ),
                    )
                    .await
            }

            let success = self.state.lock().await.service_checks[0].status == service_check::Status::Ok;
            let can_status = if success { 1. } else { 0. };
            let cant_status = if success { 0. } else { 1. };
            self.gauge("network.http.can_connect", can_status).await;
            self.gauge("network.http.cant_connect", cant_status).await;

            if self
                .instance_config
                .check_certificate_expiration
                .unwrap_or(defaults::CHECK_CERTIFICATE_EXPIRATION)
            {
                self.check_certificate(maybe_certificate).await
            }
        }

        let svc = std::mem::replace(&mut self.state.lock().await.service_checks, vec![]);
        for lsc in svc {
            let sc = to_service_check(lsc, &service_tags);
            self.sink.submit_service_check(sc).await;
        }

        Ok(())
    }

    async fn http(
        &self, tls: tokio_native_tls::TlsConnector, request: Request<Full<Bytes>>,
    ) -> std::result::Result<(Response<body::Incoming>, Option<native_tls::Certificate>), IOErr> {
        let url = self.instance_config.url.clone();
        let port = port_or_default(&url);
        let endpoint = format!("{}:{}", url.host().unwrap(), port);
        let is_https = url.scheme().is_some_and(|s| s == &Scheme::HTTPS);

        let global_timeout = self.init_config.timeout.map_or(defaults::TIMEOUT, Duration::from_secs);
        let connect_timeout = self
            .instance_config
            .connect_timeout
            .map_or(global_timeout, Duration::from_secs);

        let start_time = Instant::now();
        let remaining_timeout = || connect_timeout - start_time.elapsed();

        let tcp_stream = time::timeout(connect_timeout, async {
            TcpStream::connect(endpoint).await.map_err(|e| IOErr::Connect(e.into()))
        })
        .await
        .map_err(|_| IOErr::Timeout)??;

        let maybe_tls = if is_https {
            let tls = time::timeout(remaining_timeout(), async {
                tls.connect(url.host().unwrap(), tcp_stream)
                    .await
                    .map_err(|e| IOErr::Connect(e.into()))
            })
            .await
            .map_err(|_| IOErr::Timeout)??;
            Some(tls)
        } else {
            None
        };

        let read_timeout = self
            .instance_config
            .read_timeout
            .map_or(remaining_timeout(), Duration::from_secs);

        let mut http = HttpConnector::new();
        http.enforce_http(url.scheme().is_some_and(|s| s == &Scheme::HTTP));

        let https = HttpsConnector::from((http, tls));
        let client = Client::builder(TokioExecutor::new()).build::<_, Full<Bytes>>(https);

        let maybe_response = time::timeout(read_timeout, client.request(request))
            .await
            .map_err(|_| IOErr::Timeout)?;

        let mut certificate: Option<native_tls::Certificate> = None;
        if is_https {
            match maybe_tls.unwrap().get_ref().peer_certificate() {
                Ok(cert) => certificate = cert,
                Err(err) => {
                    self.sink
                        .log(log::Level::Error, format!("Read peer certificate: {}", err.to_string()))
                        .await
                }
            }
        }

        let response = maybe_response.map_err(|e| IOErr::Generic(e.into()))?;
        Ok((response, certificate))
    }

    fn make_tls_connector(&self) -> Result<tokio_native_tls::TlsConnector> {
        let mut tls_builder = native_tls::TlsConnector::builder();
        tls_builder.danger_accept_invalid_certs(!self.instance_config.tls_verify.unwrap_or(defaults::TLS_VERIFY));

        if let Some(path) = self.instance_config.tls_cert.as_ref() {
            tls_builder.disable_built_in_roots(true);
            let cert = load_pem(path)?;
            tls_builder.add_root_certificate(cert);
        }

        let native_tls = tls_builder.build()?;

        Ok(tokio_native_tls::TlsConnector::from(native_tls))
    }

    fn make_request(&self) -> Result<Request<Full<Bytes>>> {
        let mut headers = HeaderMap::new();
        if let Some(h) = &self.instance_config.headers {
            headers = h.clone()
        }

        let method = self.instance_config.method.as_ref().unwrap_or(&defaults::METHOD);
        if DATA_METHODS.contains(method) && !headers.contains_key("Content-Type") {
            headers.insert(
                "Content-Type",
                HeaderValue::from_static("application/x-www-form-urlencoded"),
            );
        }

        let mut request = http::Request::builder()
            .method(
                self.instance_config
                    .method
                    .as_ref()
                    .unwrap_or(&defaults::METHOD)
                    .clone(),
            )
            .uri(&self.instance_config.url);
        *request.headers_mut().unwrap() = headers; // FIXME unwrap

        let body = match &self.instance_config.data {
            Some(data) => Full::from(data.clone()),
            _ => Full::new(Bytes::new()),
        };

        Ok(request.body(body)?)
    }

    async fn check_certificate(&self, maybe_certificate: Option<native_tls::Certificate>) {
        let certificate = match maybe_certificate {
            Some(cert) => cert,
            None => {
                self.ssl_service_check(
                    service_check::Status::Unknown,
                    "Empty or no certificate found.".to_string(),
                )
                .await;
                return;
            }
        };

        // FIXME can this conversion be avoided?
        let certificate = match certificate
            .to_der()
            .map_err(GenericError::from)
            .and_then(|der| x509_cert::Certificate::from_der(&der).map_err(GenericError::from))
        {
            Ok(cert) => cert,
            Err(err) => {
                self.ssl_service_check(
                    service_check::Status::Unknown,
                    format!("Unable to parse the certificate to get expiration: {}", err.to_string()),
                )
                .await;
                return;
            }
        };

        let not_after = certificate.tbs_certificate.validity.not_after.to_system_time();

        let warning = Duration::from_secs(
            self.instance_config
                .seconds_warning
                .unwrap_or(self.instance_config.days_warning.unwrap_or(defaults::DAYS_WARNING) * 24 * 60 * 60),
        );
        let critical = Duration::from_secs(
            self.instance_config
                .seconds_critical
                .unwrap_or(self.instance_config.days_warning.unwrap_or(defaults::DAYS_WARNING) * 24 * 60 * 60),
        );

        let to_days = |d: Duration| d.as_secs() / 60 / 60 / 24;

        match not_after.duration_since(SystemTime::now()) {
            Ok(left) => {
                self.gauge("http.ssl.days_left", to_days(left) as f64).await;
                self.gauge("http.ssl.seconds_left", left.as_secs() as f64).await;
                if left < critical {
                    self.ssl_service_check(
                        service_check::Status::Critical,
                        format!(
                            "This cert TTL is critical: only {} days before it expires",
                            to_days(left)
                        ),
                    )
                    .await
                } else if left < warning {
                    self.ssl_service_check(
                        service_check::Status::Critical,
                        format!("This cert is almost expired, only {} days left", to_days(left)),
                    )
                    .await
                } else {
                    self.ssl_service_check(service_check::Status::Ok, format!("Days left: {}", to_days(left)))
                        .await
                }
            }
            Err(_) => {
                self.gauge("http.ssl.days_left", 0.).await;
                self.gauge("http.ssl.seconds_left", 0.).await;
                self.ssl_service_check(service_check::Status::Critical, "This cert is expired".to_string())
                    .await
            }
        }
    }

    async fn handle_response(&self, response: &mut Response<Incoming>) -> Result<()> {
        let mut body = Vec::<u8>::with_capacity(MAX_CONTENT_LEN);
        while let Some(frame) = response.body_mut().frame().await {
            let frame = frame?;

            if let Some(d) = frame.data_ref() {
                body.extend_from_slice(d.as_ref());
            }
            // FIXME don't read more than MAX_CONTENT_LEN
            if body.len() >= MAX_CONTENT_LEN {
                break;
            }
        }
        let body = String::from_utf8_lossy(&body);

        let maybe_content = |mut msg| {
            if self
                .instance_config
                .include_content
                .unwrap_or(defaults::INCLUDE_CONTENT)
            {
                msg += "\nContent: ";
                msg += &body[..MESSAGE_LENGTH.min(body.len())];
                SvcCheckMessage::WithContent(msg)
            } else {
                SvcCheckMessage::WithoutContent(msg)
            }
        };
        let get_message = |msg: &SvcCheckMessage| {
            match msg {
                SvcCheckMessage::WithContent(msg) => msg,
                SvcCheckMessage::WithoutContent(msg) => msg,
            }
            .clone()
        };

        let pattern = match self.instance_config.http_response_status_code.as_ref() {
            Some(s) => &s,
            None => defaults::HTTP_RESPONSE_STATUS_CODE,
        };
        let regex = Regex::new(pattern)?;

        if !regex.is_match(response.status().as_str()) {
            let message = maybe_content(format!(
                "Incorrect HTTP return code for url {}. Expected {}, got {}.",
                self.instance_config.url,
                pattern,
                response.status().as_str()
            ));
            self.sink.log(log::Level::Info, get_message(&message)).await;
            self.add_service_check(SvcCheckEvent::Status, service_check::Status::Critical, message)
                .await;
            return Ok(());
        }

        if let Some(needle) = self.instance_config.content_match.as_ref() {
            let reverse = self
                .instance_config
                .reverse_content_match
                .unwrap_or(defaults::REVERSE_CONTENT_MATCH);
            let regex = Regex::new(&needle)?;
            if regex.is_match(&body) {
                if reverse {
                    self.send_status_down(
                        format!(
                            "{} is found in return content with the reverse_content_match option",
                            needle
                        ),
                        maybe_content(format!(
                            "Content \"{}\" found in response with the reverse_content_match",
                            needle
                        )),
                    )
                    .await
                } else {
                    self.send_status_up(format!("{} is found in return content ", needle))
                        .await
                }
            } else {
                if reverse {
                    self.send_status_up(format!(
                        "{} is not found in return content with the reverse_content_match option",
                        needle
                    ))
                    .await
                } else {
                    self.send_status_down(
                        format!("{} is not found in return content", needle),
                        maybe_content(format!("Content \"{}\" not found in response.", needle)),
                    )
                    .await
                }
            }
        } else {
            self.send_status_up(format!("{} is UP", self.instance_config.url)).await
            // FIXME addr
        }

        Ok(())
    }

    async fn add_service_check(&self, event: SvcCheckEvent, status: service_check::Status, message: SvcCheckMessage) {
        let lsc = LightServiceCheck { event, status, message };
        self.state.lock().await.service_checks.push(lsc)
    }

    async fn ssl_service_check(&self, status: service_check::Status, message: String) {
        self.add_service_check(SvcCheckEvent::SSLCert, status, SvcCheckMessage::WithoutContent(message))
            .await
    }

    async fn gauge(&self, name: &str, value: f64) {
        self.sink
            .submit_metric(
                metric::Metric {
                    metric_type: metric::Type::Gauge,
                    name: name.to_string(),
                    value,
                    tags: self.state.lock().await.tags.clone(),
                },
                false,
            )
            .await
    }

    async fn send_status_up(&self, message: String) {
        self.sink.log(log::Level::Debug, message).await;
        self.add_service_check(
            SvcCheckEvent::Status,
            service_check::Status::Ok,
            SvcCheckMessage::WithoutContent("UP".to_string()),
        )
        .await
    }

    async fn send_status_down(&self, log_msg: String, down_msg: SvcCheckMessage) {
        self.sink.log(log::Level::Info, log_msg).await;
        self.add_service_check(SvcCheckEvent::Status, service_check::Status::Critical, down_msg)
            .await
    }
}

fn port_or_default(uri: &uri::Uri) -> u16 {
    match uri.port() {
        Some(port) => port.as_u16(),
        None => match uri.scheme_str() {
            // FIXME why can't use scheme()?
            Some("http") => 80,
            Some("https") => 443,
            _ => panic!("unexpected scheme"),
        },
    }
}

fn load_pem(path: &PathBuf) -> Result<native_tls::Certificate> {
    let mut file = File::open(path).with_context(|| format!("opening {} certificate", path.display()))?;
    let mut buffer = Vec::<u8>::new();
    file.read_to_end(&mut buffer)
        .with_context(|| format!("reading {} certificate", path.display()))?;
    native_tls::Certificate::from_pem(&buffer).map_err(|e| anyhow!("parsing certificate: {}", e))
}

fn normalize_tag(tag: &str) -> String {
    let tag_replacement = Regex::new(r#"[,\+\*\-/()\[\]{}\s]"#).expect("invalid regex");
    let multiple_underscore_cleanup = Regex::new(r#"__+"#).expect("invalid regex");
    let dot_underscore_cleanup = Regex::new(r#"_*\._*"#).expect("invalid regex");

    let tag = tag_replacement.replace_all(tag, "_");
    let tag = multiple_underscore_cleanup.replace_all(&tag, "_");
    let tag = dot_underscore_cleanup.replace_all(&tag, ".");
    tag.trim_matches('_').to_string()
}

fn to_service_check(lsc: LightServiceCheck, tags: &HashMap<String, String>) -> ServiceCheck {
    let event = match lsc.event {
        SvcCheckEvent::Status => "http.can_connect",
        SvcCheckEvent::SSLCert => "http.ssl_cert",
    };
    let message = match lsc.message {
        SvcCheckMessage::WithoutContent(msg) => msg,
        SvcCheckMessage::WithContent(mut msg) => {
            if msg.len() > 20 {
                msg.replace_range(17.., "...");
            };
            msg
        }
    };
    ServiceCheck {
        name: event.to_string(),
        status: lsc.status,
        tags: tags.clone(),
        message,
    }
}
