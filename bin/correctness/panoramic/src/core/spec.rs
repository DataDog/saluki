//! The participant spec: one participant as data.
//!
//! A [`ParticipantSpec`] says what runs, what it is given, where it listens and how it is observed. It is the same
//! struct on every realization. A participant type returns one, and the data fields of a case's participant entry
//! decode into one. Every field may be left unset, which says nothing at that layer.
//!
//! Layers join with [`ParticipantSpec::lay`]: a map merges by key, and an entry in the upper layer replaces the entry
//! with the same key below, whole. Every other field replaces the field below, whole. Nothing can be removed.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::de::{DeserializeOwned, Error as _};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use super::step::Operation;
use super::template::Template;

/// One participant as data.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ParticipantSpec {
    /// An image reference. A run may replace it by its text.
    #[serde(deserialize_with = "strict_optional_string")]
    pub(crate) image: Option<String>,
    /// Replaces the image's entrypoint.
    pub(crate) command: Option<Vec<Template>>,
    /// Replaces the image's default arguments.
    pub(crate) args: Option<Vec<Template>>,
    /// What runs as host processes, in start order. Each program is one observed process.
    pub(crate) programs: Option<Vec<Program>>,
    /// Environment of every process of the participant.
    pub(crate) env: BTreeMap<String, Template>,
    /// Files placed before start, keyed by destination or by a name the participant type declares.
    pub(crate) files: BTreeMap<String, FileSpec>,
    pub(crate) mounts: Option<Vec<HostMount>>,
    pub(crate) endpoints: BTreeMap<String, EndpointSpec>,
    /// Readiness evidence for an endpoint, where the default for its protocol is not enough.
    pub(crate) ready: BTreeMap<String, Evidence>,
    /// The processes steps may name. Unset, planning derives them from the launch.
    pub(crate) processes: Option<Vec<ProcessSpec>>,
    /// Command lines of the participant's software, by name.
    pub(crate) commands: BTreeMap<String, CommandSpec>,
    /// Files copied out after the procedure.
    pub(crate) collect: Option<Vec<CollectFile>>,

    // Only a participant type declares these. A case uses them and cannot define them.
    /// File names a case may write as `files` keys, each with its one destination.
    #[serde(skip)]
    pub(crate) aliases: BTreeMap<&'static str, &'static str>,
    /// Connection names the participant type knows, and what each delivers.
    #[serde(skip)]
    pub(crate) consumes: BTreeMap<&'static str, Consumed>,
    /// Capabilities steps call by name.
    #[serde(skip)]
    pub(crate) operations: &'static [&'static dyn Operation],
    /// How a case wires an endpoint the participant type declares and the default environment does not wire, by
    /// endpoint.
    #[serde(skip)]
    pub(crate) wiring: BTreeMap<String, Wiring>,
}

impl ParticipantSpec {
    /// Lays `over` on top of `self`: maps merge by key, every other field replaces.
    ///
    /// A layer that sets `command` and not `args` also removes the `args` below, so a new program does not inherit
    /// the old program's arguments.
    ///
    /// Only a participant type declares `aliases`, `consumes` and `operations`, so a case layer never sets them and
    /// they are never laid over.
    pub(crate) fn lay(&mut self, over: ParticipantSpec) {
        if over.command.is_some() && over.args.is_none() {
            self.args = None;
        }
        replace(&mut self.image, over.image);
        replace(&mut self.command, over.command);
        replace(&mut self.args, over.args);
        replace(&mut self.programs, over.programs);
        replace(&mut self.mounts, over.mounts);
        replace(&mut self.processes, over.processes);
        replace(&mut self.collect, over.collect);
        self.env.extend(over.env);
        self.files.extend(over.files);
        self.endpoints.extend(over.endpoints);
        self.ready.extend(over.ready);
        self.commands.extend(over.commands);
    }

    /// Whether something inside the participant writes the exit record or output file of one of its processes.
    pub(crate) fn supervised(&self) -> bool {
        self.processes.iter().flatten().any(ProcessSpec::supervised)
    }
}

fn replace<T>(below: &mut Option<T>, over: Option<T>) {
    if over.is_some() {
        *below = over;
    }
}

/// One host program of a participant.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Program {
    /// The process name steps use.
    #[serde(default = "default_process_name", deserialize_with = "strict_string")]
    pub(crate) name: String,
    /// `./x` under the participant's own directory, a bare name on the runner host's `PATH`, or a path as written.
    /// A run may replace it by its text.
    #[serde(deserialize_with = "strict_string")]
    pub(crate) path: String,
    #[serde(default)]
    pub(crate) args: Vec<Template>,
    /// Environment of this program only, over the participant's.
    #[serde(default)]
    pub(crate) env: BTreeMap<String, Template>,
}

/// The name of a participant's only process when nothing names it.
pub(crate) const DEFAULT_PROCESS: &str = "main";

fn default_process_name() -> String {
    DEFAULT_PROCESS.to_string()
}

/// A file placed before start.
#[derive(Clone, Debug)]
pub(crate) struct FileSpec {
    pub(crate) source: FileSource,
    /// `None` is 0644. An error on a Windows participant.
    pub(crate) mode: Option<u32>,
    /// Part of what writes exit records or output files. Only a participant type sets it.
    pub(crate) supervises: bool,
}

/// Where the content of a placed file comes from.
#[derive(Clone, Debug)]
pub(crate) enum FileSource {
    /// A file or directory of the case, relative to the case directory. A template is a UTF-8 file whose
    /// references the harness resolves.
    Case { from: PathBuf, template: bool },
    /// Bytes a participant type supplies, with a label for the printed plan.
    Bytes { label: &'static str, bytes: &'static [u8] },
    /// Text a participant type supplies, with a label for the printed plan.
    // TODO: remove the dead_code allowance once a participant type builds a file from the case's text.
    #[allow(dead_code)]
    Text { label: &'static str, text: String },
}

/// The contents of a placed file.
///
/// Serializes as `{"text": ...}` for UTF-8 bytes, `{"template": ...}` for a template as [`Template`] serializes, and
/// `{"base64": ...}` for any other bytes, in standard base64 with padding.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Contents {
    Text(String),
    /// A UTF-8 file whose references the harness resolves.
    Template(Template),
    #[serde(rename = "base64", serialize_with = "base64")]
    Bytes(Vec<u8>),
}

impl Contents {
    /// Text when `bytes` are UTF-8, otherwise bytes.
    pub(crate) fn of(bytes: Vec<u8>) -> Self {
        match String::from_utf8(bytes) {
            Ok(text) => Self::Text(text),
            Err(e) => Self::Bytes(e.into_bytes()),
        }
    }
}

fn base64<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use base64::{engine::general_purpose::STANDARD, Engine as _};

    serializer.serialize_str(&STANDARD.encode(bytes))
}

impl<'de> Deserialize<'de> for FileSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Entry {
            from: PathBuf,
            #[serde(default)]
            template: bool,
            #[serde(default)]
            mode: Option<Value>,
        }

        let entry = match Value::deserialize(deserializer)? {
            Value::String(from) => Entry {
                from: from.into(),
                template: false,
                mode: None,
            },
            value => serde_json::from_value(value).map_err(D::Error::custom)?,
        };
        let mode = entry.mode.map(parse_mode).transpose().map_err(D::Error::custom)?;
        Ok(Self {
            source: FileSource::Case {
                from: entry.from,
                template: entry.template,
            },
            mode,
            supervises: false,
        })
    }
}

fn parse_mode(mode: Value) -> Result<u32, String> {
    let text = match mode {
        Value::String(text) => text,
        _ => return Err("mode must be a quoted octal string such as \"0755\"".to_string()),
    };
    let digits = text.strip_prefix('0').filter(|digits| (3..=4).contains(&digits.len()));
    digits
        .and_then(|digits| u32::from_str_radix(digits, 8).ok())
        .ok_or_else(|| format!("mode \"{}\" must be an octal string such as \"0755\"", text))
}

/// Where a participant listens.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(try_from = "Value")]
pub(crate) struct EndpointSpec {
    pub(crate) protocol: Protocol,
    pub(crate) address: Address,
    /// The listener serves TLS. Only on `http`.
    pub(crate) tls: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Protocol {
    Tcp,
    Udp,
    Http,
    Grpc,
    UnixDatagram,
    UnixStream,
    NamedPipe,
}

impl Protocol {
    const ALL: [Self; 7] = [
        Self::Tcp,
        Self::Udp,
        Self::Http,
        Self::Grpc,
        Self::UnixDatagram,
        Self::UnixStream,
        Self::NamedPipe,
    ];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
            Self::Http => "http",
            Self::Grpc => "grpc",
            Self::UnixDatagram => "unix_datagram",
            Self::UnixStream => "unix_stream",
            Self::NamedPipe => "named_pipe",
        }
    }
}

/// The part of an endpoint the harness owns.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Address {
    Port(u16),
    Path(Template),
    Pipe(String),
}

impl TryFrom<Value> for EndpointSpec {
    type Error = String;

    fn try_from(value: Value) -> Result<Self, String> {
        let Value::Object(mut entry) = value else {
            return Err("an endpoint is a map of one protocol to its address, such as {http: 8080}".to_string());
        };
        let tls = match entry.remove("tls") {
            None => false,
            Some(Value::Bool(tls)) => tls,
            Some(other) => return Err(format!("tls must be true or false, not {}", other)),
        };
        let mut protocols = entry.into_iter();
        let (Some((protocol, address)), None) = (protocols.next(), protocols.next()) else {
            return Err("an endpoint has exactly one protocol, such as {http: 8080}".to_string());
        };
        let protocol = Protocol::ALL
            .into_iter()
            .find(|candidate| candidate.as_str() == protocol)
            .ok_or_else(|| {
                let protocols: Vec<_> = Protocol::ALL.iter().map(|protocol| protocol.as_str()).collect();
                format!(
                    "unknown endpoint protocol '{}': expected one of {}",
                    protocol,
                    protocols.join(", ")
                )
            })?;
        let address = match protocol {
            Protocol::Tcp | Protocol::Udp | Protocol::Http | Protocol::Grpc => match decode::<u16>(address)? {
                0 => return Err("an endpoint's port must not be 0".to_string()),
                port => Address::Port(port),
            },
            Protocol::UnixDatagram | Protocol::UnixStream => Address::Path(decode(address)?),
            Protocol::NamedPipe => Address::Pipe(decode(address)?),
        };
        if tls && protocol != Protocol::Http {
            return Err(format!("tls applies to http endpoints, not {}", protocol.as_str()));
        }
        Ok(Self { protocol, address, tls })
    }
}

/// Readiness evidence for an endpoint.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", try_from = "Value")]
pub(crate) enum Evidence {
    /// The endpoint accepts a connection.
    Accepts,
    /// The socket exists.
    Exists,
    /// Nothing is checked.
    None,
    /// The endpoint answers 200 at this path.
    Http { path: String },
    /// A process of the participant writes a line that matches.
    Output { pattern: Template, process: String },
}

impl TryFrom<Value> for Evidence {
    type Error = String;

    fn try_from(value: Value) -> Result<Self, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Map {
            http: Option<String>,
            output: Option<Template>,
            process: Option<String>,
        }

        let expected = "evidence is accepts, exists, none, {http: <path>} or {output: <pattern>, process: <name>}";
        match value {
            Value::String(text) => match text.as_str() {
                "accepts" => Ok(Self::Accepts),
                "exists" => Ok(Self::Exists),
                "none" => Ok(Self::None),
                _ => Err(format!("unknown evidence '{}': {}", text, expected)),
            },
            value => match decode::<Map>(value)? {
                Map {
                    http: Some(path),
                    output: None,
                    process: None,
                } => Ok(Self::Http { path }),
                Map {
                    http: None,
                    output: Some(pattern),
                    process: Some(process),
                } => Ok(Self::Output { pattern, process }),
                _ => Err(expected.to_string()),
            },
        }
    }
}

/// A process steps may name, and how its exit and output are observed.
///
/// Built only through [`ProcessSpec::new`], which rejects a name or a file a step could not use.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(try_from = "Value")]
pub(crate) struct ProcessSpec {
    name: String,
    exit: ExitChannel,
    output: OutputChannel,
}

/// How a process's exit is observed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExitChannel {
    /// What the realization reports: the container's exit, or the wait result of a host process.
    Container,
    /// A record file a supervisor inside the participant appends to.
    Record { file: String },
}

/// How a process's output is observed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OutputChannel {
    /// What the realization reports: the container log, or the pipes of a host process.
    Container,
    /// A log file a supervisor inside the participant writes.
    File { file: String },
}

impl ProcessSpec {
    /// Builds a process observed through `exit` and `output`.
    ///
    /// # Errors
    ///
    /// Returns an error if `name` is not a name, or a record or log file is not a plain file name.
    pub(crate) fn new(name: impl Into<String>, exit: ExitChannel, output: OutputChannel) -> Result<Self, String> {
        let name = name.into();
        if !super::case::is_name(&name) {
            return Err(format!("process name '{}' {}", name, super::case::NAME_RULE));
        }
        let files = [
            match &exit {
                ExitChannel::Record { file } => Some(file),
                ExitChannel::Container => None,
            },
            match &output {
                OutputChannel::File { file } => Some(file),
                OutputChannel::Container => None,
            },
        ];
        if let Some(file) = files.into_iter().flatten().find(|file| !is_file_name(file)) {
            return Err(format!(
                "process '{}': '{}' must be a plain file name, with no directory",
                name, file
            ));
        }
        Ok(Self { name, exit, output })
    }

    /// A process observed through what its realization reports.
    pub(crate) fn observed_by_realization(name: &str) -> Result<Self, String> {
        Self::new(name, ExitChannel::Container, OutputChannel::Container)
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn exit(&self) -> &ExitChannel {
        &self.exit
    }

    pub(crate) fn output(&self) -> &OutputChannel {
        &self.output
    }

    /// Whether something inside the participant writes this process's exit record or output file.
    pub(crate) fn supervised(&self) -> bool {
        self.exit != ExitChannel::Container || self.output != OutputChannel::Container
    }
}

impl TryFrom<Value> for ProcessSpec {
    type Error = String;

    fn try_from(value: Value) -> Result<Self, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Entry {
            name: String,
            exit: Value,
            output: Value,
        }

        let entry: Entry = decode(value)?;
        let exit = match entry.exit {
            Value::String(text) if text == "container" => ExitChannel::Container,
            Value::Object(map) => match decode_one(map, "record")? {
                Some(file) => ExitChannel::Record { file },
                None => return Err("exit is container or {record: <file>}".to_string()),
            },
            _ => return Err("exit is container or {record: <file>}".to_string()),
        };
        let output = match entry.output {
            Value::String(text) if text == "container" => OutputChannel::Container,
            Value::Object(map) => match decode_one(map, "file")? {
                Some(file) => OutputChannel::File { file },
                None => return Err("output is container or {file: <file>}".to_string()),
            },
            _ => return Err("output is container or {file: <file>}".to_string()),
        };
        Self::new(entry.name, exit, output)
    }
}

/// Decodes a map that holds exactly `key`, or returns `None` if it holds anything else.
fn decode_one(mut map: serde_json::Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match (map.remove(key), map.is_empty()) {
        (Some(value), true) => decode(value).map(Some),
        _ => Ok(None),
    }
}

/// A command line of the participant's software, for steps that run it by name.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(try_from = "Value")]
pub(crate) struct CommandSpec {
    /// As host processes: run this named program of the participant, with `argv` as its arguments.
    pub(crate) program: Option<String>,
    pub(crate) argv: Vec<Template>,
    pub(crate) env: BTreeMap<String, Template>,
}

impl TryFrom<Value> for CommandSpec {
    type Error = String;

    fn try_from(value: Value) -> Result<Self, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Entry {
            program: Option<String>,
            #[serde(default)]
            argv: Vec<Template>,
            #[serde(default)]
            env: BTreeMap<String, Template>,
        }

        let entry = match value {
            Value::Array(_) => Entry {
                program: None,
                argv: decode(value)?,
                env: BTreeMap::new(),
            },
            value => decode(value)?,
        };
        if entry.program.is_none() && entry.argv.is_empty() {
            return Err("a command needs a program or a non-empty argv".to_string());
        }
        Ok(Self {
            program: entry.program,
            argv: entry.argv,
            env: entry.env,
        })
    }
}

/// A host path mounted into a Linux container or pod.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HostMount {
    #[serde(deserialize_with = "strict_string")]
    pub(crate) host: String,
    #[serde(deserialize_with = "strict_string")]
    pub(crate) path: String,
    #[serde(default)]
    pub(crate) read_only: bool,
}

/// A file copied out of the participant after the procedure.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CollectFile {
    #[serde(deserialize_with = "strict_string")]
    pub(crate) path: String,
    /// The file name it is kept under.
    #[serde(deserialize_with = "strict_string")]
    pub(crate) name: String,
    pub(crate) keep: Keep,
}

/// When a collected file is kept.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Keep {
    Always,
    OnFailure,
}

/// A connection name a participant type knows, and what it delivers.
#[derive(Clone, Debug)]
pub(crate) struct Consumed {
    pub(crate) accepts: &'static [Protocol],
    // TODO: check required connections with the first participant type that requires one.
    #[allow(dead_code)]
    pub(crate) required: bool,
    /// Environment added when the case makes the connection, as template text.
    pub(crate) env: &'static [(&'static str, &'static str)],
}

/// How a case wires an endpoint that the participant type's default environment does not, for the error that says
/// why the endpoint is not wired.
#[derive(Clone, Debug)]
pub(crate) struct Wiring {
    /// Why the default environment entry that wired the endpoint was withdrawn, if one was.
    pub(crate) withdrawn: Option<String>,
    /// The line a case writes to wire it.
    pub(crate) write: String,
    /// Why the participant's image listens somewhere else whatever the case writes, if it does. The endpoint is then
    /// not wired, even where the case refers to it.
    pub(crate) overridden: Option<String>,
}

/// Whether `name` is a plain file name, with no directory.
pub(crate) fn is_file_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\'])
}

/// Decodes a string, rejecting a YAML value of any other type rather than reading it as text.
pub(crate) fn strict_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    match Value::deserialize(deserializer)? {
        Value::String(text) => Ok(text),
        other => Err(D::Error::custom(format!(
            "expected a string, found {}: quote a value that would otherwise read as a boolean or a number",
            other
        ))),
    }
}

/// Decodes a string that may be left unset, as [`strict_string`] does.
pub(crate) fn strict_optional_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    strict_string(deserializer).map(Some)
}

fn decode<T>(value: Value) -> Result<T, String>
where
    T: DeserializeOwned,
{
    serde_json::from_value(value).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(yaml: &str) -> ParticipantSpec {
        serde_saphyr::from_str(yaml).unwrap()
    }

    #[test]
    fn laying_merges_maps_by_key_and_replaces_every_other_field_whole() {
        let mut below = spec("{image: a, command: [x], args: [w], env: {A: '1', B: '2'}, programs: [{path: p}]}");
        below.lay(spec(
            "{env: {B: '3', C: '4'}, programs: [{path: q}, {name: r, path: r}]}",
        ));
        let env: Vec<_> = below
            .env
            .iter()
            .map(|(key, value)| format!("{}={}", key, value))
            .collect();
        assert_eq!(env, ["A=1", "B=3", "C=4"]);
        assert_eq!(below.programs.as_ref().map(Vec::len), Some(2));
        assert_eq!(below.image.as_deref(), Some("a"));
        assert!(below.args.is_some());

        // A new command does not inherit the arguments of the one it replaces.
        below.lay(spec("{command: [z]}"));
        assert!(below.args.is_none());
    }
}
