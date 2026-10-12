//! Resources artifacts: recipients and outputs declared apart from native configuration.
//!
//! The `resources` product of the [artifact channel](crate::channel) carries one config whose envelope holds a
//! [`ResourcesArtifact`]: the recipients (endpoints, credential references, protocols, delivery policies) and the
//! outputs it declares. Policy artifacts reference outputs by ID only, so a destination enters the host only through
//! native configuration or through this product, which has its own revisions and authority.
//!
//! - [`ResourcesArtifact`]: the document, version [`RESOURCES_VERSION`], read strictly.
//! - [`validate`]: reads a document and checks it, putting endpoints in [canonical form](canonical_endpoint).
//! - [`fingerprint`]: an output's fingerprint, version 2, which covers its host, what it accepts, and its
//!   [`Selector`]: for a declared output, its recipient list.
//!
//! A declared output's identifier is `resources_<name>` ([`OutputId::declared`]); a declared recipient's is
//! `<host>/declared/<credential name>/<canonical endpoint>` ([`declared_recipient_id`]).

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use snafu::Snafu;
use url::Url;

use crate::{
    canonical::{to_canonical_string, PlanHash},
    extension::BoundedDelivery,
    ir::{
        is_identifier, CredentialRef, HostId, OutputId, RecipientId, Selector, DECLARED_OUTPUT_PREFIX,
        MAX_DECLARED_OUTPUT_NAME_LEN,
    },
    limits::values::{MAX_DECLARED_OUTPUTS, MAX_DECLARED_RECIPIENTS, RESOURCES_ARTIFACT_SIZE},
    model::PortType,
    validate::Catalog,
};

/// The version of the resources artifact's format.
pub const RESOURCES_VERSION: u32 = 1;

/// The version of output fingerprints that [`fingerprint`] computes.
pub const FINGERPRINT_VERSION: u32 = 2;

/// The namespace of the credential references a resources artifact may use: `credentials/<name>`, each an entry the
/// operator declares in the host's configuration for resources artifacts.
pub const CREDENTIALS_NAMESPACE: &str = "credentials/";

/// The longest display label of a declared output, in bytes.
pub const MAX_LABEL_LEN: usize = 256;

/// The longest endpoint a declared recipient may have, in bytes, as written and in canonical form.
pub const MAX_ENDPOINT_LEN: usize = 2048;

/// The longest canonical endpoint a recipient identity spells out, in bytes; a longer one is written as a digest, as
/// a native recipient identity's is.
const MAX_SPELLED_OUT_ENDPOINT_LEN: usize = 400;

/// The longest recipient identity, in bytes, as [`RecipientId`] allows.
const MAX_RECIPIENT_ID_LEN: usize = 512;

/// A resources artifact: the recipients and outputs declared apart from native configuration.
///
/// Serialized as an object with the members `resources_version` ([`RESOURCES_VERSION`]), `recipients`, and `outputs`;
/// unknown members are rejected.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResourcesArtifact {
    /// The document's format version, [`RESOURCES_VERSION`].
    pub resources_version: u32,

    /// The recipients declared.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recipients: Vec<DeclaredRecipient>,

    /// The outputs declared.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<DeclaredOutput>,
}

impl ResourcesArtifact {
    /// Returns the recipient declared under `name`, if there is one.
    pub fn recipient(&self, name: &str) -> Option<&DeclaredRecipient> {
        self.recipients.iter().find(|recipient| recipient.name == name)
    }

    /// Returns every credential the artifact's recipients use, without repeats, in order.
    ///
    /// The host checks that each is configured before it lowers the declarations: a resources revision whose
    /// credential is not configured is pending.
    pub fn credentials(&self) -> BTreeSet<&CredentialRef> {
        self.recipients.iter().map(|recipient| &recipient.credential).collect()
    }
}

/// One recipient a resources artifact declares: an endpoint of a delivery host, with its credential.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredRecipient {
    /// The recipient's name within the artifact, an identifier, which declared outputs refer to it by.
    pub name: String,

    /// The delivery host the recipient belongs to.
    pub host: HostId,

    /// The endpoint, an `http` or `https` URL; in canonical form once the artifact is [validated](validate).
    pub endpoint: String,

    /// The credential the recipient sends with: a `credentials/<name>` reference to an entry the operator declared.
    pub credential: CredentialRef,

    /// The protocol variants the recipient accepts.
    #[serde(default, skip_serializing_if = "DeclaredProtocol::is_default")]
    pub protocol: DeclaredProtocol,

    /// The recipient's delivery policy within its host's native bounds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<BoundedDelivery>,
}

impl DeclaredRecipient {
    /// Returns the recipient's identity, from its host, its credential's name, and its endpoint, which must already be
    /// in canonical form, as [validation](validate) leaves it (see [`declared_recipient_id`]).
    ///
    /// # Errors
    ///
    /// Returns an error if the credential is not a `credentials/<name>` reference whose name is an identifier.
    pub fn id(&self) -> Result<RecipientId, ResourcesError> {
        let name = credential_name(&self.credential).ok_or_else(|| {
            invalid(format!(
                "recipient `{}`: credential `{}` is not one a resources artifact may use: write `{}<name>`, naming \
                 an entry the operator declared for resources artifacts, whose name is an identifier",
                self.name, self.credential, CREDENTIALS_NAMESPACE
            ))
        })?;
        Ok(declared_recipient_id(&self.host, name, &self.endpoint))
    }
}

/// The protocol variants a declared recipient accepts, beside the defaults every recipient accepts.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredProtocol {
    /// Whether the recipient takes series in the V3 protocol, and so no V2 series.
    #[serde(default)]
    pub v3_series: bool,

    /// Whether the recipient takes sketches in the V3 protocol, and so no V2 sketches.
    #[serde(default)]
    pub v3_sketches: bool,
}

impl DeclaredProtocol {
    /// Returns `true` if the recipient accepts only the default variants, which serialization leaves out.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// One output a resources artifact declares: an encoder stream delivering to declared recipients.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredOutput {
    /// The output's name, an identifier of at most 53 bytes: the output's ID is `resources_<name>`.
    pub name: String,

    /// The names of the declared recipients the output delivers to, all on one host.
    pub recipients: Vec<String>,

    /// The records the output accepts.
    #[serde(default)]
    pub accepts: DeclaredAccepts,

    /// The output's display label, for tools that list outputs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl DeclaredOutput {
    /// Returns the output's identifier, `resources_<name>`.
    ///
    /// # Errors
    ///
    /// Returns an error if the name is not an identifier short enough for `resources_<name>` to be one.
    pub fn id(&self) -> Result<OutputId, crate::ir::IdError> {
        OutputId::declared(&self.name)
    }
}

/// The records a declared output accepts.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeclaredAccepts {
    /// Every metric, of every kind and timestamp form.
    #[default]
    Any,
}

impl DeclaredAccepts {
    /// Returns the port type of the records the output accepts.
    pub const fn port_type(self) -> PortType {
        match self {
            Self::Any => PortType::ANY,
        }
    }
}

/// Why a resources artifact is not valid.
#[derive(Clone, Debug, Eq, PartialEq, Snafu)]
#[snafu(context(suffix(false)), visibility(pub(crate)))]
pub enum ResourcesError {
    /// The document is not a resources artifact.
    #[snafu(display("The document is not a valid resources artifact: {}", message))]
    Malformed {
        /// What is wrong with it.
        message: String,
    },

    /// The document breaks one of the rules resources artifacts follow.
    #[snafu(display("The resources artifact is invalid: {}", message))]
    Invalid {
        /// The rule it breaks, and where.
        message: String,
    },
}

fn invalid(message: String) -> ResourcesError {
    ResourcesError::Invalid { message }
}

/// Reads a resources artifact from its document, as received in its envelope, and checks it against the host's
/// catalog.
///
/// The checks are those of the specification's resources artifacts, in this order:
///
/// 1. the document's size, the length of its canonical form as for a policy artifact
///    ([`receive_artifact`][crate::compose::receive_artifact]), is at most `RESOURCES_ARTIFACT_SIZE` bytes;
/// 2. it reads as a [`ResourcesArtifact`], strictly (unknown members are refused), in version [`RESOURCES_VERSION`];
/// 3. it declares at most `MAX_DECLARED_RECIPIENTS` recipients and `MAX_DECLARED_OUTPUTS` outputs;
/// 4. each recipient has a name that is an identifier no other recipient has; names a delivery host that the catalog
///    declares as accepting declared recipients ([`HostSchema::declared_recipients`][crate::validate::HostSchema]);
///    has an endpoint that is an `http` or `https` URL without user information, query, or fragment, which is put in
///    [canonical form](canonical_endpoint); uses a credential `credentials/<name>` whose name is an identifier; bounds
///    its queue, if it does, to at least one byte; and has an identity ([`DeclaredRecipient::id`]) no other recipient
///    of the artifact has;
/// 5. each output has a name short enough that `resources_<name>` is an identifier, which no other output has; names
///    at least one recipient, each a recipient of the artifact, at most once, all on one delivery host; and has a
///    label, if it does, of at most [`MAX_LABEL_LEN`] bytes without control characters.
///
/// What depends on the host's configuration is the host's to check when it resolves the declarations: a queue no
/// larger than the native one, spilling only where disk storage is configured, and whether the credentials are
/// configured.
///
/// # Errors
///
/// Returns the first rule the document breaks.
pub fn validate(document: Value, catalog: &dyn Catalog) -> Result<ResourcesArtifact, ResourcesError> {
    let canonical = to_canonical_string(&document).map_err(|error| ResourcesError::Malformed {
        message: format!("it has no canonical form: {}", error),
    })?;
    let size = canonical.len() as u64;
    if size > RESOURCES_ARTIFACT_SIZE {
        return Err(invalid(format!(
            "it is {} bytes, more than the resources artifact size limit of {} bytes; declare fewer resources, or \
             shorten their endpoints and labels",
            size, RESOURCES_ARTIFACT_SIZE
        )));
    }
    let mut artifact =
        serde_json::from_value::<ResourcesArtifact>(document).map_err(|error| ResourcesError::Malformed {
            message: error.to_string(),
        })?;
    if artifact.resources_version != RESOURCES_VERSION {
        return Err(invalid(format!(
            "it is written in version {} of the format, and this host reads version {}",
            artifact.resources_version, RESOURCES_VERSION
        )));
    }
    if artifact.recipients.len() as u64 > MAX_DECLARED_RECIPIENTS {
        return Err(invalid(format!(
            "it declares {} recipients, more than the limit of {}",
            artifact.recipients.len(),
            MAX_DECLARED_RECIPIENTS
        )));
    }
    if artifact.outputs.len() as u64 > MAX_DECLARED_OUTPUTS {
        return Err(invalid(format!(
            "it declares {} outputs, more than the limit of {}",
            artifact.outputs.len(),
            MAX_DECLARED_OUTPUTS
        )));
    }

    // Each recipient's host, by name, and the recipient each identity belongs to.
    let mut hosts = BTreeMap::new();
    let mut identities = BTreeMap::new();
    for recipient in &mut artifact.recipients {
        if !is_identifier(&recipient.name) {
            return Err(invalid(format!(
                "recipient name `{}` is not an identifier: names are lowercase letters, digits, and single \
                 underscores, starting with a letter",
                recipient.name
            )));
        }
        let name = recipient.name.clone();
        let at = |message: String| invalid(format!("recipient `{}`: {}", name, message));
        if hosts.contains_key(&recipient.name) {
            return Err(at(
                "another recipient has this name; give each recipient its own".to_string()
            ));
        }
        match catalog.host(&recipient.host) {
            None => {
                return Err(at(format!(
                    "the catalog declares no delivery host `{}`; declare the recipient on a delivery host that \
                     accepts declared recipients",
                    recipient.host
                )))
            }
            Some(schema) if !schema.declared_recipients => {
                return Err(at(format!(
                    "delivery host `{}` does not accept declared recipients",
                    recipient.host
                )))
            }
            Some(_) => {}
        }
        recipient.endpoint = canonical_endpoint(&recipient.endpoint).map_err(|error| match error {
            ResourcesError::Invalid { message } | ResourcesError::Malformed { message } => at(message),
        })?;
        if recipient.delivery.and_then(|delivery| delivery.queue_bytes) == Some(0) {
            return Err(at(
                "its queue is bounded to 0 bytes, which holds nothing; bound it to at least 1 byte, or leave out \
                 `queue_bytes` to keep the native bound"
                    .to_string(),
            ));
        }
        let id = recipient.id()?;
        if let Some(other) = identities.insert(id.clone(), recipient.name.clone()) {
            return Err(at(format!(
                "it is the same recipient as `{}`, with identity `{}`: one endpoint with one credential is one \
                 recipient, so declare it once",
                other, id
            )));
        }
        hosts.insert(recipient.name.clone(), recipient.host.clone());
    }

    let mut outputs = BTreeSet::new();
    for output in &artifact.outputs {
        if output.id().is_err() {
            return Err(invalid(format!(
                "output name `{}` is not an identifier of at most {} bytes, so `{}<name>` would not be one",
                output.name, MAX_DECLARED_OUTPUT_NAME_LEN, DECLARED_OUTPUT_PREFIX
            )));
        }
        let at = |message: String| invalid(format!("output `{}`: {}", output.name, message));
        if !outputs.insert(&output.name) {
            return Err(at("another output has this name; give each output its own".to_string()));
        }
        if output.recipients.is_empty() {
            return Err(at(
                "it names no recipient; name the recipients it delivers to".to_string()
            ));
        }
        let mut named = BTreeSet::new();
        let mut host = None;
        for name in &output.recipients {
            let Some(recipient_host) = hosts.get(name) else {
                return Err(at(format!(
                    "it names recipient `{}`, which the artifact does not declare",
                    name
                )));
            };
            if !named.insert(name) {
                return Err(at(format!("it names recipient `{}` more than once", name)));
            }
            match host {
                Some(first) if first != recipient_host => {
                    return Err(at(format!(
                        "its recipients are on delivery hosts `{}` and `{}`, and an output delivers through one host",
                        first, recipient_host
                    )))
                }
                _ => host = Some(recipient_host),
            }
        }
        if let Some(label) = &output.label {
            if label.len() > MAX_LABEL_LEN || label.chars().any(char::is_control) {
                return Err(at(format!(
                    "its label is longer than {} bytes, or has control characters",
                    MAX_LABEL_LEN
                )));
            }
        }
    }
    Ok(artifact)
}

/// Returns the name of a credential reference in the [`CREDENTIALS_NAMESPACE`], if the reference is one and its name
/// is an identifier: `team_d` for `credentials/team_d`.
pub fn credential_name(credential: &CredentialRef) -> Option<&str> {
    credential
        .as_str()
        .strip_prefix(CREDENTIALS_NAMESPACE)
        .filter(|name| is_identifier(name))
}

/// Returns the identity of a declared recipient: `<host>/declared/<credential name>/<canonical endpoint>`.
///
/// A canonical endpoint longer than 400 bytes, or one that would make the identity longer than the 512 bytes a
/// recipient identity may have, is written as `sha256:` and the first 16 hexadecimal digits of its digest instead, as
/// a native recipient's long endpoint is. Two declarations of one endpoint with one credential are therefore one
/// recipient, with one queue and one directory, and a changed endpoint or credential is another recipient.
pub fn declared_recipient_id(host: &HostId, credential_name: &str, canonical_endpoint: &str) -> RecipientId {
    let id = |endpoint: &str| format!("{}/declared/{}/{}", host, credential_name, endpoint);
    let spelled_out = id(canonical_endpoint);
    let fits = canonical_endpoint.len() <= MAX_SPELLED_OUT_ENDPOINT_LEN
        && spelled_out.len() <= MAX_RECIPIENT_ID_LEN
        && !canonical_endpoint.is_empty()
        && canonical_endpoint.bytes().all(|b| b.is_ascii_graphic() && b != b'@')
        && !canonical_endpoint.starts_with("sha256:");
    let id = if fits {
        spelled_out
    } else {
        let digest = PlanHash::of_bytes(canonical_endpoint.as_bytes());
        let hex = digest.digest()[..8]
            .iter()
            .map(|byte| format!("{:02x}", byte))
            .collect::<String>();
        id(&format!("sha256:{}", hex))
    };
    // A host ID and a credential name are identifiers, and the endpoint part is printable, bounded ASCII.
    RecipientId::new(id).expect("declared recipient identities are printable ASCII of bounded length")
}

/// Returns an endpoint in canonical form: lowercase scheme and host, the scheme's default port removed, and the path
/// normalized, with its dot segments resolved and without a trailing slash.
///
/// Two declarations of one endpoint written differently are one recipient, with one queue and one directory. The
/// endpoint is the base URL a delivery host appends its intake paths to, so it carries no user information (credentials
/// go in the recipient's credential reference), no query, and no fragment. Percent-encoding is kept as written, apart
/// from characters a URL cannot hold as they are, which are percent-encoded; a host name outside ASCII is written in
/// its ASCII form.
///
/// # Errors
///
/// Returns an error if the endpoint is not an `http` or `https` URL with a host, has user information, a query, or a
/// fragment, or is longer than [`MAX_ENDPOINT_LEN`] bytes, as written or in canonical form. The error repeats the
/// endpoint only with its user information, query, and fragment replaced by `***`, since each may hold a credential.
pub fn canonical_endpoint(endpoint: &str) -> Result<String, ResourcesError> {
    const EXAMPLE: &str = "write the intake's base URL, such as `https://intake.example.com`";
    let too_long = |length: usize| {
        invalid(format!(
            "its endpoint is {} bytes, more than the limit of {}",
            length, MAX_ENDPOINT_LEN
        ))
    };
    if endpoint.len() > MAX_ENDPOINT_LEN {
        return Err(too_long(endpoint.len()));
    }
    let url = Url::parse(endpoint).map_err(|error| {
        invalid(format!(
            "endpoint `{}` is not a URL ({}); {}",
            redacted_endpoint(endpoint),
            error,
            EXAMPLE
        ))
    })?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid(format!(
            "endpoint `{}` carries user information, which may be a credential; remove it, and name the credential in \
             `credential` instead",
            redacted_endpoint(endpoint)
        )));
    }
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid(format!(
            "endpoint `{}` is not an `http` or `https` URL; {}",
            redacted_endpoint(endpoint),
            EXAMPLE
        )));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(invalid(format!(
            "endpoint `{}` has a query or a fragment, which an intake's base URL does not; {}",
            redacted_endpoint(endpoint),
            EXAMPLE
        )));
    }
    // An `http` or `https` URL always has a host.
    let host = url.host_str().unwrap_or_default();
    let mut canonical = format!("{}://{}", url.scheme(), host);
    if let Some(port) = url.port() {
        canonical.push(':');
        canonical.push_str(&port.to_string());
    }
    canonical.push_str(url.path().trim_end_matches('/'));
    if canonical.len() > MAX_ENDPOINT_LEN {
        return Err(too_long(canonical.len()));
    }
    Ok(canonical)
}

/// Returns an endpoint as an error message may repeat it, whether or not it is a URL: what may be user information
/// (everything after the scheme's `://`, or from the start without one, up to the last `@`) is written `***@`, and a
/// query or a fragment (from the first `?` or `#` after that) is written `?***` or `#***`.
///
/// User information may hold a password, and a query an API key, so neither is repeated, whatever else is wrong with
/// the endpoint. The last `@` is the end of the user information however the endpoint is malformed, since a password
/// may hold `/`, `?`, or `#` as they are; an `@` in a path or query only redacts more. Control characters are escaped.
fn redacted_endpoint(endpoint: &str) -> String {
    // A scheme is a letter followed by letters, digits, `+`, `-`, and `.`: anything else before a `://` may be user
    // information.
    let is_scheme = |scheme: &str| {
        scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    };
    let (scheme, rest) = match endpoint.find("://") {
        Some(at) if is_scheme(&endpoint[..at]) => endpoint.split_at(at + 3),
        _ => ("", endpoint),
    };
    let mut redacted = scheme.to_string();
    let rest = match rest.rfind('@') {
        Some(at) => {
            redacted.push_str("***@");
            &rest[at + 1..]
        }
        None => rest,
    };
    match rest.find(['?', '#']) {
        Some(at) => {
            redacted.push_str(&rest[..at]);
            redacted.push_str(&rest[at..at + 1]);
            redacted.push_str("***");
        }
        None => redacted.push_str(rest),
    }
    redacted.escape_debug().to_string()
}

/// Returns an output's fingerprint, version [`FINGERPRINT_VERSION`]: the digest of `{"v": 2, "host": <delivery host>,
/// "accepts": <port type>, "selector": <selector>}`.
///
/// A fingerprint covers what an output's declaration says it delivers to: for a role, whatever native configuration
/// resolves for it, so recipients added or removed under a role change nothing; for a routing group, its key; for a
/// declared output, its recipient list. Encoder parameters, delivery policies, recipient settings, and credentials are
/// never part of it.
pub fn fingerprint(host: &HostId, accepts: PortType, selector: &Selector) -> PlanHash {
    let document = json!({
        "v": FINGERPRINT_VERSION,
        "host": host,
        "accepts": accepts,
        "selector": selector,
    });
    // The document holds strings and one small integer, so it always has a canonical form.
    PlanHash::of_value(&document).expect("fingerprint documents have a canonical form")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{
        ir::{BindingName, InputName, ParamSpec, RoleName, ScopeName},
        model::{Kind, KindSet, TimestampForm},
        validate::{CatalogBinding, CatalogOutput, HostSchema, MemoryCatalog},
    };

    /// A catalog with `dd_out`, which accepts declared recipients, and `mrf_dd_out`, which does not.
    struct Hosts;

    impl Catalog for Hosts {
        fn output(&self, _: &OutputId) -> Option<&CatalogOutput> {
            None
        }

        fn binding(&self, _: &BindingName) -> Option<&CatalogBinding> {
            None
        }

        fn scope_input(&self, _: &ScopeName, _: &InputName) -> Option<PortType> {
            None
        }

        fn host(&self, host: &HostId) -> Option<HostSchema> {
            const NONE: &[ParamSpec] = &[];
            let schema = |declared_recipients| HostSchema {
                encoder: NONE,
                settings: NONE,
                protocol: NONE,
                declared_recipients,
            };
            match host.as_str() {
                "dd_out" => Some(schema(true)),
                "mrf_dd_out" => Some(schema(false)),
                _ => None,
            }
        }
    }

    fn recipient(name: &str, endpoint: &str, credential: &str) -> Value {
        json!({"name": name, "host": "dd_out", "endpoint": endpoint, "credential": credential})
    }

    fn document(recipients: Vec<Value>, outputs: Vec<Value>) -> Value {
        json!({"resources_version": 1, "recipients": recipients, "outputs": outputs})
    }

    fn team_d() -> Value {
        document(
            vec![recipient("team_d", "http://127.0.0.1:17105", "credentials/team_d")],
            vec![json!({"name": "team_d", "recipients": ["team_d"], "label": "Team D"})],
        )
    }

    #[track_caller]
    fn refused(document: Value, expected: &str) {
        let error = validate(document, &Hosts).unwrap_err().to_string();
        assert!(error.contains(expected), "expected `{}` in: {}", expected, error);
    }

    #[test]
    fn the_documented_example_reads_and_writes_back() {
        let document = json!({
            "resources_version": 1,
            "recipients": [{
                "name": "team_d",
                "host": "dd_out",
                "endpoint": "http://127.0.0.1:17105",
                "credential": "credentials/team_d",
                "protocol": {"v3_series": false, "v3_sketches": false},
                "delivery": {"queue_bytes": 4194304, "spill": "never", "on_retire": "attempt"}
            }],
            "outputs": [{"name": "team_d", "recipients": ["team_d"], "accepts": "any", "label": "Team D"}]
        });
        let artifact = serde_json::from_value::<ResourcesArtifact>(document.clone()).unwrap();
        assert_eq!(artifact.resources_version, RESOURCES_VERSION);
        assert_eq!(artifact.recipients[0].delivery.unwrap().queue_bytes, Some(4_194_304));
        assert!(artifact.recipients[0].protocol.is_default());
        let output = &artifact.outputs[0];
        assert_eq!(output.id().unwrap().as_str(), "resources_team_d");
        assert_eq!(output.accepts.port_type(), PortType::ANY);

        // Defaults are left out when written.
        let written = serde_json::to_value(&artifact).unwrap();
        assert_eq!(written["recipients"][0].get("protocol"), None);
        assert_eq!(serde_json::from_value::<ResourcesArtifact>(written).unwrap(), artifact);

        assert!(serde_json::from_value::<ResourcesArtifact>(json!({"resources_version": 1, "extra": 1})).is_err());

        // The example validates as written: its endpoint is already canonical.
        assert_eq!(validate(document, &Hosts).unwrap(), artifact);
    }

    #[test]
    fn a_valid_artifact_reads_with_its_endpoints_in_canonical_form_and_its_recipients_named_by_identity() {
        let document = document(
            vec![
                recipient(
                    "team_d",
                    "HTTP://Intake.Example.COM:80/v1/../intake/",
                    "credentials/team_d",
                ),
                recipient("team_e", "https://e.example.com:443", "credentials/team_e"),
            ],
            vec![json!({"name": "both", "recipients": ["team_d", "team_e"]})],
        );
        let artifact = validate(document, &Hosts).unwrap();
        assert_eq!(artifact.recipients[0].endpoint, "http://intake.example.com/intake");
        assert_eq!(artifact.recipients[1].endpoint, "https://e.example.com");
        assert_eq!(
            artifact.recipients[0].id().unwrap().as_str(),
            "dd_out/declared/team_d/http://intake.example.com/intake"
        );
        assert_eq!(
            artifact
                .credentials()
                .into_iter()
                .map(CredentialRef::as_str)
                .collect::<Vec<_>>(),
            vec!["credentials/team_d", "credentials/team_e"]
        );
        assert_eq!(artifact.recipient("team_e").unwrap().host.as_str(), "dd_out");
        assert_eq!(artifact.recipient("team_f"), None);
    }

    #[test]
    fn endpoints_are_put_in_canonical_form() {
        // Hand-written: what each spelling must become.
        let cases = [
            ("http://127.0.0.1:17105", "http://127.0.0.1:17105"),
            ("http://127.0.0.1:17105/", "http://127.0.0.1:17105"),
            ("HTTPS://APP.Example.com", "https://app.example.com"),
            ("https://app.example.com:443/", "https://app.example.com"),
            ("http://app.example.com:80", "http://app.example.com"),
            ("http://app.example.com:443", "http://app.example.com:443"),
            (
                "https://app.example.com:8443/intake/",
                "https://app.example.com:8443/intake",
            ),
            ("https://app.example.com/a/./b/../c//", "https://app.example.com/a/c"),
            ("https://app.example.com/team%20d", "https://app.example.com/team%20d"),
            ("https://app.example.com/team d", "https://app.example.com/team%20d"),
            ("  https://app.example.com  ", "https://app.example.com"),
            ("http://[::1]:8080/x", "http://[::1]:8080/x"),
            ("https://bücher.example", "https://xn--bcher-kva.example"),
        ];
        for (endpoint, canonical) in cases {
            assert_eq!(canonical_endpoint(endpoint).as_deref(), Ok(canonical), "{}", endpoint);
            // Canonical form is a fixed point.
            assert_eq!(canonical_endpoint(canonical).as_deref(), Ok(canonical), "{}", canonical);
        }
    }

    #[test]
    fn endpoints_that_are_not_http_base_urls_are_refused_and_user_information_is_never_repeated() {
        let cases = [
            ("127.0.0.1:17105", "is not a URL"),
            ("intake.example.com", "is not a URL"),
            ("ftp://intake.example.com", "is not an `http` or `https` URL"),
            ("unix:///var/run/intake.sock", "is not an `http` or `https` URL"),
            ("https://intake.example.com/?key=1", "has a query or a fragment"),
            ("https://intake.example.com/#top", "has a query or a fragment"),
            ("https://", "is not a URL"),
        ];
        for (endpoint, expected) in cases {
            let error = canonical_endpoint(endpoint).unwrap_err().to_string();
            assert!(error.contains(expected), "{}: {}", endpoint, error);
        }
        // Neither user information nor a query or a fragment is repeated, however malformed the endpoint is: each may
        // hold a credential. Each case gives what the message says the endpoint is.
        let cases = [
            (
                "https://user:hunter2@intake.example.com",
                "https://***@intake.example.com",
                "carries user information",
            ),
            (
                "https://hunter2@intake.example.com",
                "https://***@intake.example.com",
                "carries user information",
            ),
            (
                "ftp://user:hunter2@intake.example.com",
                "ftp://***@intake.example.com",
                "carries user information",
            ),
            (
                "https://user:hunter2@exa mple.com",
                "https://***@exa mple.com",
                "is not a URL",
            ),
            (
                "https://user:hunter2@example.com:99999",
                "https://***@example.com:99999",
                "is not a URL",
            ),
            // A password holding `/`, `?`, or `#` unencoded ends the authority early.
            (
                "https://user:hun?ter2@example.com",
                "https://***@example.com",
                "is not a URL",
            ),
            (
                "https://user:hun/ter2@example.com",
                "https://***@example.com",
                "is not a URL",
            ),
            // Without a `://` after a scheme, everything up to the last `@` may be user information.
            (
                "user:hunter2@example.com",
                "***@example.com",
                "is not an `http` or `https` URL",
            ),
            (
                "user:hun://ter2@example.com",
                "***@example.com",
                "is not an `http` or `https` URL",
            ),
            (
                "https://intake.example.com/api/v2/series?api_key=hunter2",
                "https://intake.example.com/api/v2/series?***",
                "has a query or a fragment",
            ),
            (
                "https://intake.example.com/#api_key=hunter2",
                "https://intake.example.com/#***",
                "has a query or a fragment",
            ),
            (
                "unix:///var/run/intake.sock?key=hunter2",
                "unix:///var/run/intake.sock?***",
                "is not an `http` or `https` URL",
            ),
        ];
        for (endpoint, redacted, expected) in cases {
            let error = canonical_endpoint(endpoint).unwrap_err().to_string();
            assert!(error.contains(expected), "{}: {}", endpoint, error);
            assert!(
                error.contains(&format!("endpoint `{}`", redacted)),
                "{}: {}",
                endpoint,
                error
            );
            assert!(
                !error.contains("hunter2") && !error.contains("ter2"),
                "{}: {}",
                endpoint,
                error
            );
        }
        let long = format!("https://intake.example.com/{}", "a".repeat(MAX_ENDPOINT_LEN));
        assert!(canonical_endpoint(&long)
            .unwrap_err()
            .to_string()
            .contains("more than the limit of 2048"));
    }

    #[test]
    fn declared_recipient_identities_spell_out_their_endpoint_unless_it_is_too_long() {
        let host = HostId::new("dd_out").unwrap();
        assert_eq!(
            declared_recipient_id(&host, "team_d", "http://127.0.0.1:17105").as_str(),
            "dd_out/declared/team_d/http://127.0.0.1:17105"
        );
        // A long endpoint is its digest: the first 16 hexadecimal digits of SHA-256 of the canonical endpoint.
        let long = format!("https://intake.example.com/{}", "a".repeat(400));
        let digest = saluki_tls::sha256(long.as_bytes());
        let hex = digest[..8]
            .iter()
            .map(|byte| format!("{:02x}", byte))
            .collect::<String>();
        assert_eq!(
            declared_recipient_id(&host, "team_d", &long).as_str(),
            format!("dd_out/declared/team_d/sha256:{}", hex)
        );
        // An endpoint short enough to spell out, but one that would make the identity longer than 512 bytes (63 + 10 +
        // 63 + 1 + 400 = 537), is a digest too.
        let (host, name) = ("h".repeat(63), "n".repeat(63));
        let endpoint = format!("https://intake.example.com/{}", "a".repeat(400 - 27));
        assert_eq!(endpoint.len(), 400);
        let digest = saluki_tls::sha256(endpoint.as_bytes());
        let hex = digest[..8]
            .iter()
            .map(|byte| format!("{:02x}", byte))
            .collect::<String>();
        assert_eq!(
            declared_recipient_id(&HostId::new(host.as_str()).unwrap(), &name, &endpoint).as_str(),
            format!("{}/declared/{}/sha256:{}", host, name, hex)
        );
        // One byte shorter on each side fits: 62 + 10 + 62 + 1 + 377 = 512.
        let endpoint = &endpoint[..377];
        let id = declared_recipient_id(&HostId::new(&host[..62]).unwrap(), &name[..62], endpoint);
        assert_eq!(
            id.as_str(),
            format!("{}/declared/{}/{}", &host[..62], &name[..62], endpoint)
        );
        assert_eq!(id.as_str().len(), 512);
    }

    #[test]
    fn documents_that_are_not_resources_artifacts_are_malformed() {
        for document in [
            json!([]),
            json!({"recipients": []}),
            json!({"resources_version": 1, "unknown": true}),
            json!({"resources_version": 1, "recipients": [{"name": "a", "host": "dd_out"}]}),
            json!({"resources_version": 1.5}),
        ] {
            assert!(
                matches!(
                    validate(document.clone(), &Hosts),
                    Err(ResourcesError::Malformed { .. })
                ),
                "{}",
                document
            );
        }
    }

    #[test]
    fn each_rule_of_a_resources_artifact_refuses_the_first_document_that_breaks_it() {
        assert!(validate(team_d(), &Hosts).is_ok());

        let mut version = team_d();
        version["resources_version"] = json!(2);
        refused(version, "version 2 of the format, and this host reads version 1");

        let many = (0..17)
            .map(|index| {
                recipient(
                    &format!("r{}", index),
                    &format!("http://r{}.example", index),
                    "credentials/a",
                )
            })
            .collect();
        refused(document(many, Vec::new()), "17 recipients, more than the limit of 16");
        let outputs = (0..17)
            .map(|index| json!({"name": format!("o{}", index), "recipients": ["team_d"]}))
            .collect();
        refused(
            document(vec![recipient("team_d", "http://d.example", "credentials/d")], outputs),
            "17 outputs, more than the limit of 16",
        );

        let large = json!({"name": "team_d", "recipients": ["team_d"], "label": "x".repeat(200)});
        let mut document_large = team_d();
        document_large["outputs"] = json!(vec![large; 400]);
        refused(
            document_large,
            "more than the resources artifact size limit of 65536 bytes",
        );

        refused(
            document(vec![recipient("Team", "http://d.example", "credentials/d")], Vec::new()),
            "recipient name `Team` is not an identifier",
        );
        refused(
            document(
                vec![
                    recipient("team_d", "http://d.example", "credentials/d"),
                    recipient("team_d", "http://e.example", "credentials/e"),
                ],
                Vec::new(),
            ),
            "recipient `team_d`: another recipient has this name",
        );
        let mut unknown = recipient("team_d", "http://d.example", "credentials/d");
        unknown["host"] = json!("elsewhere_out");
        refused(
            document(vec![unknown], Vec::new()),
            "the catalog declares no delivery host `elsewhere_out`",
        );
        let mut failover = recipient("team_d", "http://d.example", "credentials/d");
        failover["host"] = json!("mrf_dd_out");
        refused(
            document(vec![failover], Vec::new()),
            "delivery host `mrf_dd_out` does not accept declared recipients",
        );
        refused(
            document(
                vec![recipient("team_d", "ftp://d.example", "credentials/d")],
                Vec::new(),
            ),
            "recipient `team_d`: endpoint `ftp://d.example` is not an `http` or `https` URL",
        );
        for credential in ["native:api_key", "credentials/", "credentials/Team", "team_d"] {
            refused(
                document(vec![recipient("team_d", "http://d.example", credential)], Vec::new()),
                "is not one a resources artifact may use",
            );
        }
        let mut empty_queue = recipient("team_d", "http://d.example", "credentials/d");
        empty_queue["delivery"] = json!({"queue_bytes": 0});
        refused(document(vec![empty_queue], Vec::new()), "bounded to 0 bytes");
        refused(
            document(
                vec![
                    recipient("team_d", "http://d.example/", "credentials/d"),
                    recipient("team_e", "HTTP://D.example", "credentials/d"),
                ],
                Vec::new(),
            ),
            "recipient `team_e`: it is the same recipient as `team_d`, with identity \
             `dd_out/declared/d/http://d.example`",
        );

        let with_outputs =
            |outputs: Vec<Value>| document(vec![recipient("team_d", "http://d.example", "credentials/d")], outputs);
        refused(
            with_outputs(vec![json!({"name": "a".repeat(54), "recipients": ["team_d"]})]),
            "is not an identifier of at most 53 bytes",
        );
        refused(
            with_outputs(vec![
                json!({"name": "a", "recipients": ["team_d"]}),
                json!({"name": "a", "recipients": ["team_d"]}),
            ]),
            "output `a`: another output has this name",
        );
        refused(
            with_outputs(vec![json!({"name": "a", "recipients": []})]),
            "output `a`: it names no recipient",
        );
        refused(
            with_outputs(vec![json!({"name": "a", "recipients": ["team_e"]})]),
            "output `a`: it names recipient `team_e`, which the artifact does not declare",
        );
        refused(
            with_outputs(vec![json!({"name": "a", "recipients": ["team_d", "team_d"]})]),
            "output `a`: it names recipient `team_d` more than once",
        );
        refused(
            with_outputs(vec![json!({"name": "a", "recipients": ["team_d"], "label": "Team\nD"})]),
            "output `a`: its label is longer than 256 bytes, or has control characters",
        );
    }

    #[test]
    fn an_output_delivers_through_one_host() {
        // A catalog whose two hosts both accept declared recipients.
        struct TwoHosts;
        impl Catalog for TwoHosts {
            fn output(&self, _: &OutputId) -> Option<&CatalogOutput> {
                None
            }
            fn binding(&self, _: &BindingName) -> Option<&CatalogBinding> {
                None
            }
            fn scope_input(&self, _: &ScopeName, _: &InputName) -> Option<PortType> {
                None
            }
            fn host(&self, _: &HostId) -> Option<HostSchema> {
                Some(HostSchema {
                    encoder: &[],
                    settings: &[],
                    protocol: &[],
                    declared_recipients: true,
                })
            }
        }
        let mut other = recipient("team_e", "http://e.example", "credentials/e");
        other["host"] = json!("other_out");
        let document = document(
            vec![recipient("team_d", "http://d.example", "credentials/d"), other],
            vec![json!({"name": "a", "recipients": ["team_d", "team_e"]})],
        );
        let error = validate(document, &TwoHosts).unwrap_err().to_string();
        assert!(
            error.contains("its recipients are on delivery hosts `dd_out` and `other_out`"),
            "{}",
            error
        );
    }

    #[test]
    fn a_catalog_without_hosts_accepts_no_declared_recipient() {
        let error = validate(team_d(), &MemoryCatalog::new()).unwrap_err().to_string();
        assert!(
            error.contains("the catalog declares no delivery host `dd_out`"),
            "{}",
            error
        );
    }

    #[test]
    fn a_declared_output_name_must_leave_room_for_the_prefix() {
        let output = |name: &str| DeclaredOutput {
            name: name.to_string(),
            recipients: vec!["team_d".to_string()],
            accepts: DeclaredAccepts::Any,
            label: None,
        };
        assert!(output(&"a".repeat(53)).id().is_ok());
        assert!(output(&"a".repeat(54)).id().is_err());
        assert!(output("Team").id().is_err());
        assert!(output("").id().is_err());
    }

    /// The fingerprints of version 2, each the SHA-256 digest of the canonical document written out here by hand
    /// (members sorted, no whitespace), computed with `shasum -a 256`, independently of the code under test.
    #[test]
    fn fingerprints_are_the_digests_of_their_hand_written_canonical_documents() {
        let dd_out = HostId::new("dd_out").unwrap();
        let team_d = RecipientId::new("dd_out/declared/team_d/http://127.0.0.1:17105").unwrap();
        let team_e = RecipientId::new("dd_out/declared/team_e/https://e.example.com").unwrap();
        let group = "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let scalars = PortType {
            kinds: KindSet::of(&[Kind::Counter, Kind::Rate, Kind::Gauge, Kind::Set]),
            timestamps: TimestampForm::Mixed,
        };
        let cases = [
            // {"accepts":{"kinds":"all","timestamps":"mixed"},"host":"dd_out","selector":{"role":"main"},"v":2}
            (
                fingerprint(&dd_out, PortType::ANY, &Selector::Role(RoleName::new("main").unwrap())),
                "sha256:35b6c7483c20787a99a97a2a2bb2684fd1a0a8f0315e5bf73fad5b48b74595f7",
            ),
            // {"accepts":{...},"host":"dd_out","selector":{"group":"sha256:ba78...15ad"},"v":2}
            (
                fingerprint(&dd_out, PortType::ANY, &Selector::Group(group.parse().unwrap())),
                "sha256:cb8e7cce9492fb8586c4c8eb5ba80f900e475c06d6a5eaf601386c4df7daedb9",
            ),
            // {"accepts":{...},"host":"mrf_dd_out","selector":{"role":"multi_region_failover"},"v":2}
            (
                fingerprint(
                    &HostId::new("mrf_dd_out").unwrap(),
                    PortType::ANY,
                    &Selector::Role(RoleName::new("multi_region_failover").unwrap()),
                ),
                "sha256:0ff4cb7e0e1316c414965f9ca7858095cb8feff59bd696171455cbe959c77c08",
            ),
            // {"accepts":{"kinds":["counter","rate","gauge","set"],"timestamps":"mixed"},"host":"cluster_agent_out",
            //  "selector":{"role":"autoscaling_failover"},"v":2}
            (
                fingerprint(
                    &HostId::new("cluster_agent_out").unwrap(),
                    scalars,
                    &Selector::Role(RoleName::new("autoscaling_failover").unwrap()),
                ),
                "sha256:608361af9ff1be71390572e16f40425d91adeab874b3f89d17435918832514ff",
            ),
            // {"accepts":{...},"host":"dd_out",
            //  "selector":{"declared":["dd_out/declared/team_d/http://127.0.0.1:17105"]},"v":2}
            (
                fingerprint(&dd_out, PortType::ANY, &Selector::Declared(vec![team_d.clone()])),
                "sha256:361f843bc9a6b25663cc6da865dd3af0daf04bb8890e3717ae98d7d30bb1c838",
            ),
            // The same with team E's recipient added.
            (
                fingerprint(&dd_out, PortType::ANY, &Selector::Declared(vec![team_d, team_e])),
                "sha256:b89cad5700638679428a88f84402a07bf1af503cd043e823f14f7e1527ec4c1f",
            ),
        ];
        for (computed, expected) in cases {
            assert_eq!(computed.to_string(), expected);
        }
    }
}
