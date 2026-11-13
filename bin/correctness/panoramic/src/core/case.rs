//! Loading a case: the test case schema, decoded strictly.
//!
//! A case is a directory with a `config.yaml` and the files it references. Every key has one owner: the harness core,
//! a participant type (the keys under `with`) or a step (its own fields). A key its owner does not know is an error
//! before anything starts. Loading checks what does not depend on the subject OS; planning checks the rest.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use super::participant::{ConnectionTarget, Enclosure, Os};
use super::registry::Registry;
use super::spec::{
    strict_optional_string, strict_string, CollectFile, CommandSpec, Contents, EndpointSpec, Evidence, ExitChannel,
    FileSource, FileSpec, HostMount, OutputChannel, ParticipantSpec, ProcessSpec, Program,
};
use super::step::{ProcedureNode, StepEntry};
use super::template::Template;

/// The file in a case directory that defines the case.
pub(crate) const CASE_FILE: &str = "config.yaml";

/// A problem with a case, found before anything starts.
#[derive(Clone, Debug)]
pub(crate) struct CaseError {
    pub(crate) case: String,
    pub(crate) file: PathBuf,
    /// The subject OS the problem was found for, or `None` if it holds for every OS.
    pub(crate) subject_os: Option<Os>,
    /// The participant or step the problem is in.
    pub(crate) at: Option<String>,
    pub(crate) message: String,
}

impl fmt::Display for CaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "case '{}' ({})", self.case, self.file.display())?;
        if let Some(os) = self.subject_os {
            write!(f, ", subject OS {}", os)?;
        }
        if let Some(at) = &self.at {
            write!(f, ", {}", at)?;
        }
        write!(f, ": {}", self.message)
    }
}

/// Where and what a case is.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct CaseInfo {
    /// The name of the case directory.
    pub(crate) name: String,
    pub(crate) file: PathBuf,
    pub(crate) description: Option<String>,
    pub(crate) timeout: Timeout,
}

/// A time budget, written with a unit: `20m`, `180s`. A case's timeout and a step's durations are written this way.
#[derive(Clone, Debug, Serialize)]
#[serde(into = "String")]
pub(crate) struct Timeout {
    pub(crate) text: String,
    pub(crate) duration: Duration,
}

impl From<Timeout> for String {
    fn from(timeout: Timeout) -> Self {
        timeout.text
    }
}

impl<'de> Deserialize<'de> for Timeout {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let text = strict_string(deserializer)?;
        let digits = text.trim_end_matches(|c: char| c.is_ascii_alphabetic());
        let unit = &text[digits.len()..];
        let millis_per_unit = match unit {
            "ms" => 1,
            "s" => 1_000,
            "m" => 60_000,
            "h" => 3_600_000,
            _ => {
                return Err(serde::de::Error::custom(format!(
                    "timeout '{}' needs a number and a unit: ms, s, m or h, such as 20m",
                    text
                )))
            }
        };
        match digits
            .parse::<u64>()
            .ok()
            .and_then(|count| count.checked_mul(millis_per_unit))
        {
            Some(millis) if millis > 0 => Ok(Self {
                duration: Duration::from_millis(millis),
                text,
            }),
            _ => Err(serde::de::Error::custom(format!(
                "timeout '{}' needs a whole number above zero before its unit, such as 20m",
                text
            ))),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CaseFile {
    #[serde(default, deserialize_with = "strict_optional_string")]
    description: Option<String>,
    timeout: Timeout,
    subject_os: Vec<Os>,
    participants: BTreeMap<String, ParticipantEntry>,
    procedure: Vec<Value>,
}

/// A participant as the case wrote it.
///
/// The first fields place the participant in the case. The rest are its data, the fields of [`ParticipantSpec`]:
/// serde cannot deny unknown fields and flatten at once, so both structs list them.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ParticipantEntry {
    #[serde(rename = "type", default)]
    participant_type: Option<String>,
    #[serde(default)]
    subject: bool,
    #[serde(default)]
    realization: Option<RealizationRequest>,
    #[serde(default)]
    connect: BTreeMap<String, ConnectionTarget>,
    #[serde(default)]
    with: BTreeMap<String, Value>,

    #[serde(default, deserialize_with = "strict_optional_string")]
    image: Option<String>,
    #[serde(default)]
    command: Option<Vec<Template>>,
    #[serde(default)]
    args: Option<Vec<Template>>,
    #[serde(default)]
    programs: Option<Vec<Program>>,
    #[serde(default)]
    env: BTreeMap<String, Template>,
    #[serde(default)]
    files: BTreeMap<String, FileSpec>,
    #[serde(default)]
    mounts: Option<Vec<HostMount>>,
    #[serde(default)]
    endpoints: BTreeMap<String, EndpointSpec>,
    #[serde(default)]
    ready: BTreeMap<String, Evidence>,
    #[serde(default)]
    processes: Option<Vec<ProcessSpec>>,
    #[serde(default)]
    commands: BTreeMap<String, CommandSpec>,
    #[serde(default)]
    collect: Option<Vec<CollectFile>>,
}

impl ParticipantEntry {
    fn split(self) -> (Placement, ParticipantSpec) {
        let Self {
            participant_type,
            subject,
            realization,
            connect,
            with,
            image,
            command,
            args,
            programs,
            env,
            files,
            mounts,
            endpoints,
            ready,
            processes,
            commands,
            collect,
        } = self;
        let placement = Placement {
            participant_type,
            subject,
            realization,
            connect,
            with,
        };
        let spec = ParticipantSpec {
            image,
            command,
            args,
            programs,
            env,
            files,
            mounts,
            endpoints,
            ready,
            processes,
            commands,
            collect,
            aliases: BTreeMap::new(),
            consumes: BTreeMap::new(),
            operations: &[],
            wiring: BTreeMap::new(),
        };
        (placement, spec)
    }
}

/// Where a participant sits in its case.
#[derive(Debug)]
pub(crate) struct Placement {
    pub(crate) participant_type: Option<String>,
    pub(crate) subject: bool,
    pub(crate) realization: Option<RealizationRequest>,
    pub(crate) connect: BTreeMap<String, ConnectionTarget>,
    pub(crate) with: BTreeMap<String, Value>,
}

/// A realization the case requests, because it is under test.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RealizationRequest {
    pub(crate) enclosure: Enclosure,
    #[serde(default)]
    pub(crate) options: BTreeMap<String, bool>,
}

/// One participant of a loaded case.
#[derive(Debug)]
pub(crate) struct LoadedParticipant {
    pub(crate) placement: Placement,
    /// The case's data fields: the case layer of the spec.
    pub(crate) spec: ParticipantSpec,
}

/// A case, loaded and checked for what does not depend on the subject OS.
#[derive(Debug)]
pub(crate) struct LoadedCase {
    pub(crate) info: CaseInfo,
    pub(crate) subject_os: Vec<Os>,
    pub(crate) participants: BTreeMap<String, LoadedParticipant>,
    pub(crate) procedure: Vec<ProcedureNode<StepEntry>>,
    pub(crate) sources: CaseSources,
}

/// The files and directories a case supplies, read once at load and keyed by their source as written. A file a
/// participant places as a template is held parsed.
#[derive(Debug, Default)]
pub(crate) struct CaseSources {
    sources: BTreeMap<PathBuf, Source>,
}

/// The contents of a file or directory.
#[derive(Clone, Debug)]
pub(crate) enum Source {
    File(Contents),
    /// Every file under a directory, by its path relative to the directory, with `/` separators.
    Dir(Vec<(String, Contents)>),
}

impl CaseSources {
    /// A copy of what `from` places. A file held as a template is plain text unless `template` is set.
    ///
    /// # Errors
    ///
    /// Returns an error if `from` was not read with the case.
    pub(crate) fn placed(&self, from: &Path, template: bool) -> Result<Source, String> {
        match self.sources.get(from) {
            Some(Source::File(Contents::Template(parsed))) if !template => {
                Ok(Source::File(Contents::Text(parsed.text().to_string())))
            }
            Some(source) => Ok(source.clone()),
            None => Err(format!("'{}' was not read with the case", from.display())),
        }
    }

    /// The text of a file source.
    pub(crate) fn text(&self, from: &Path) -> Result<&str, String> {
        match self.sources.get(from) {
            Some(Source::File(Contents::Text(text))) => Ok(text),
            Some(Source::File(Contents::Template(template))) => Ok(template.text()),
            Some(Source::File(Contents::Bytes(_))) => Err(format!("file '{}' is not UTF-8 text", from.display())),
            Some(Source::Dir(_)) => Err(format!("'{}' is a directory, not a file", from.display())),
            None => Err(format!("'{}' was not read with the case", from.display())),
        }
    }

    /// The paths of the files under a directory source, relative to it, or `None` for a file source.
    pub(crate) fn dir_entries(&self, from: &Path) -> Option<Vec<&str>> {
        match self.sources.get(from)? {
            Source::Dir(files) => Some(files.iter().map(|(path, _)| path.as_str()).collect()),
            Source::File(_) => None,
        }
    }

    /// Reads `from`, relative to `dir`, once.
    fn read(&mut self, dir: &Path, suite: &Path, from: &Path, template: bool) -> Result<(), String> {
        if !self.sources.contains_key(from) {
            let path = dir.join(from);
            let real = path
                .canonicalize()
                .map_err(|e| format!("file source '{}' cannot be read: {}", from.display(), e))?;
            if !real.starts_with(suite) {
                return Err(format!(
                    "file source '{}' is outside the suite directory {}",
                    from.display(),
                    suite.display()
                ));
            }
            let source = if real.is_dir() {
                let mut files = Vec::new();
                read_dir(suite, &real, &real, &mut vec![real.clone()], &mut files)
                    .map_err(|e| format!("directory source '{}': {}", from.display(), e))?;
                Source::Dir(files)
            } else {
                let bytes =
                    fs::read(&real).map_err(|e| format!("file source '{}' cannot be read: {}", from.display(), e))?;
                Source::File(Contents::of(bytes))
            };
            self.sources.insert(from.to_path_buf(), source);
        }
        if template {
            let parsed =
                Template::parse(self.text(from)?).map_err(|e| format!("template '{}': {}", from.display(), e))?;
            self.sources
                .insert(from.to_path_buf(), Source::File(Contents::Template(parsed)));
        }
        Ok(())
    }
}

/// Reads every file under `dir` into `out`, keyed by its path relative to `root`.
///
/// Each entry, a symlink included, must resolve inside `suite`. `open` holds the directories being read, so a link back
/// to one of them is an error rather than an endless walk.
fn read_dir(
    suite: &Path, root: &Path, dir: &Path, open: &mut Vec<PathBuf>, out: &mut Vec<(String, Contents)>,
) -> Result<(), String> {
    let mut entries = fs::read_dir(dir)
        .and_then(|entries| {
            entries
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<Result<Vec<_>, _>>()
        })
        .map_err(|e| format!("cannot be read: {}", e))?;
    entries.sort();
    for path in entries {
        let relative = path.strip_prefix(root).unwrap_or(&path);
        let relative: Vec<_> = relative.iter().map(|part| part.to_string_lossy()).collect();
        let relative = relative.join("/");
        let real = path
            .canonicalize()
            .map_err(|e| format!("'{}' cannot be read: {}", relative, e))?;
        if !real.starts_with(suite) {
            return Err(format!(
                "'{}' is outside the suite directory {}",
                relative,
                suite.display()
            ));
        }
        if real.is_dir() {
            if open.contains(&real) {
                return Err(format!("'{}' links back to a directory that holds it", relative));
            }
            open.push(real);
            read_dir(suite, root, &path, open, out)?;
            open.pop();
        } else {
            let bytes = fs::read(&real).map_err(|e| format!("'{}' cannot be read: {}", relative, e))?;
            out.push((relative, Contents::of(bytes)));
        }
    }
    Ok(())
}

/// The text of every field of `spec` that never holds references, by field.
fn literal_fields(spec: &ParticipantSpec) -> Vec<(String, String)> {
    let mut fields = Vec::new();
    let mut add = |field: String, text: &str| fields.push((field, text.to_string()));
    if let Some(image) = &spec.image {
        add("image".to_string(), image);
    }
    for program in spec.programs.iter().flatten() {
        add("programs.name".to_string(), &program.name);
        add("programs.path".to_string(), &program.path);
    }
    for (key, file) in &spec.files {
        add("a files key".to_string(), key);
        if let FileSource::Case { from, .. } = &file.source {
            add(format!("files.{}", key), &from.to_string_lossy());
        }
    }
    for mount in spec.mounts.iter().flatten() {
        add("mounts.host".to_string(), &mount.host);
        add("mounts.path".to_string(), &mount.path);
    }
    for file in spec.collect.iter().flatten() {
        add("collect.path".to_string(), &file.path);
        add("collect.name".to_string(), &file.name);
    }
    for process in spec.processes.iter().flatten() {
        add("processes.name".to_string(), process.name());
        if let ExitChannel::Record { file } = process.exit() {
            add("processes.exit".to_string(), file);
        }
        if let OutputChannel::File { file } = process.output() {
            add("processes.output".to_string(), file);
        }
    }
    for (endpoint, evidence) in &spec.ready {
        match evidence {
            Evidence::Http { path } => add(format!("ready.{}.http", endpoint), path),
            Evidence::Output { process, .. } => add(format!("ready.{}.process", endpoint), process),
            Evidence::Accepts | Evidence::Exists | Evidence::None => {}
        }
    }
    for (name, command) in &spec.commands {
        if let Some(program) = &command.program {
            add(format!("commands.{}.program", name), program);
        }
    }
    fields
}

/// The rule [`is_name`] checks, worded to follow a name in a message.
pub(crate) const NAME_RULE: &str = "must start with a lowercase letter and hold only lowercase letters, digits and '_'";

/// Whether `name` is a name references can hold: see [`NAME_RULE`].
pub(crate) fn is_name(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Whether `name` is a participant name: a lowercase DNS label, because it becomes a container, pod and host name.
/// 'localhost', a name of only digits and a hexadecimal number written with '0x' are left out, because a resolver
/// answers them with an address of its own instead of the container's.
pub(crate) fn is_participant_name(name: &str) -> bool {
    (1..=63).contains(&name.len())
        && name != "self"
        && !name.eq_ignore_ascii_case("localhost")
        && !name.chars().all(|c| c.is_ascii_digit())
        && name
            .strip_prefix("0x")
            .is_none_or(|hex| u32::from_str_radix(hex, 16).is_err())
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.ends_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Loads the case in `dir` and checks what does not depend on the subject OS.
///
/// # Errors
///
/// Returns every problem found. A file that does not decode stops at its first problem.
pub(crate) fn load(dir: &Path, registry: &Registry) -> Result<LoadedCase, Vec<CaseError>> {
    let file = dir.join(CASE_FILE);
    let name = dir
        .canonicalize()
        .ok()
        .and_then(|dir| dir.file_name().map(|name| name.to_string_lossy().into_owned()))
        .unwrap_or_else(|| dir.display().to_string());
    let error = |at: Option<String>, message: String| CaseError {
        case: name.clone(),
        file: file.clone(),
        subject_os: None,
        at,
        message,
    };

    let text = fs::read_to_string(&file).map_err(|e| vec![error(None, format!("cannot be read: {}", e))])?;
    let shape: Value = serde_saphyr::from_str(&text).map_err(|e| vec![error(None, e.to_string())])?;
    if shape.get("type").is_some() {
        return Err(vec![error(
            None,
            "this is a legacy case file (it has a top-level 'type'), which the participant case schema does not \
             read"
                .to_string(),
        )]);
    }
    let case: CaseFile = serde_saphyr::from_str(&text).map_err(|e| vec![error(None, e.to_string())])?;
    let suite = dir
        .canonicalize()
        .ok()
        .and_then(|dir| dir.parent().map(Path::to_path_buf))
        .ok_or_else(|| {
            vec![error(
                None,
                "the case directory has no suite directory above it".to_string(),
            )]
        })?;

    let mut errors = Vec::new();
    if case.subject_os.is_empty() {
        errors.push(error(None, "subject_os must name at least one OS".to_string()));
    }
    let mut seen = BTreeSet::new();
    for os in &case.subject_os {
        if !seen.insert(os) {
            errors.push(error(None, format!("subject_os names {} twice", os)));
        }
    }
    if case.participants.is_empty() {
        errors.push(error(None, "a case needs at least one participant".to_string()));
    }
    let subjects: Vec<&String> = case
        .participants
        .iter()
        .filter(|(_, entry)| entry.subject)
        .map(|(name, _)| name)
        .collect();
    if subjects.len() != 1 {
        errors.push(error(
            None,
            format!(
                "exactly one participant sets 'subject: true', but {} do{}",
                subjects.len(),
                if subjects.is_empty() {
                    String::new()
                } else {
                    format!(": {}", join(&subjects))
                }
            ),
        ));
    }

    let names: BTreeSet<String> = case.participants.keys().cloned().collect();
    let mut sources = CaseSources::default();
    let mut participants = BTreeMap::new();
    for (participant, entry) in case.participants {
        let at = Some(format!("participant '{}'", participant));
        let mut problem = |message: String| errors.push(error(at.clone(), message));
        if !is_participant_name(&participant) {
            problem(
                "a participant name must be a lowercase DNS label (letters, digits and '-', at most 63 \
                 characters) other than 'self', 'localhost', a name of only digits and a hexadecimal number written \
                 with '0x', which a resolver does not answer with the container"
                    .to_string(),
            );
        }
        let (placement, spec) = entry.split();
        match &placement.participant_type {
            Some(participant_type) if registry.participant_type(participant_type).is_none() => {
                problem(format!("unknown participant type '{}'", participant_type))
            }
            None if !placement.with.is_empty() => problem(
                "'with' holds a participant type's parameters, and this participant has no participant \
                 type"
                    .to_string(),
            ),
            _ => {}
        }
        if let Some(request) = &placement.realization {
            for option in request.options.keys() {
                if !request.enclosure.facts().options.contains(&option.as_str()) {
                    problem(format!(
                        "realization option '{}' is not supported on {}",
                        option, request.enclosure
                    ));
                }
            }
        }
        for (connection, target) in &placement.connect {
            if !is_name(connection) {
                problem(format!("connection name '{}' {}", connection, NAME_RULE));
            }
            if !names.contains(&target.participant) {
                problem(format!(
                    "connect.{} names participant '{}', which the case does not have",
                    connection, target.participant
                ));
            }
        }
        for (field, text) in literal_fields(&spec) {
            if text.contains("{{") {
                problem(format!(
                    "{} '{}' holds no references, so it cannot contain '{{{{'",
                    field, text
                ));
            }
        }
        for (key, file) in &spec.files {
            if let FileSource::Case { from, template } = &file.source {
                if let Err(message) = sources.read(dir, &suite, from, *template) {
                    problem(format!("files.{}: {}", key, message));
                }
            }
        }
        participants.insert(participant, LoadedParticipant { placement, spec });
    }

    let mut procedure = Vec::new();
    for (index, node) in case.procedure.into_iter().enumerate() {
        match StepEntry::decode_node(node) {
            Ok(node) => procedure.push(node),
            Err(message) => errors.push(error(Some(format!("step {}", index + 1)), message)),
        }
    }

    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(LoadedCase {
        info: CaseInfo {
            name,
            file,
            description: case.description,
            timeout: case.timeout,
        },
        subject_os: case.subject_os,
        participants,
        procedure,
        sources,
    })
}

/// Joins names for a message: `'a', 'b'`.
pub(crate) fn join<T>(names: &[T]) -> String
where
    T: fmt::Display,
{
    let names: Vec<String> = names.iter().map(|name| format!("'{}'", name)).collect();
    names.join(", ")
}

#[cfg(test)]
mod tests {
    use std::fmt;

    use serde::de::{self, Visitor};

    use super::*;

    #[test]
    fn participant_names_a_resolver_answers_itself_are_rejected() {
        for name in ["localhost", "127", "0x7f000001", "0x7f"] {
            assert!(!is_participant_name(name), "{name}");
        }
        for name in ["adp", "0x-host", "0xzz", "intake-1"] {
            assert!(is_participant_name(name), "{name}");
        }
    }

    /// A deserializer that records the field names serde asks a struct for, then fails.
    struct FieldNames<'a>(&'a mut Vec<&'static str>);

    #[derive(Debug)]
    struct Stop;

    impl fmt::Display for Stop {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("stop")
        }
    }

    impl std::error::Error for Stop {}

    impl de::Error for Stop {
        fn custom<T: fmt::Display>(_: T) -> Self {
            Stop
        }
    }

    impl<'de> Deserializer<'de> for FieldNames<'_> {
        type Error = Stop;

        fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value, Stop> {
            Err(Stop)
        }

        fn deserialize_struct<V: Visitor<'de>>(
            self, _: &'static str, fields: &'static [&'static str], _: V,
        ) -> Result<V::Value, Stop> {
            self.0.extend(fields);
            Err(Stop)
        }

        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf option unit
            unit_struct newtype_struct seq tuple tuple_struct map enum identifier ignored_any
        }
    }

    fn field_names<'de, T: Deserialize<'de>>() -> BTreeSet<&'static str> {
        let mut fields = Vec::new();
        let _ = T::deserialize(FieldNames(&mut fields));
        fields.into_iter().collect()
    }

    #[test]
    fn participant_entry_lists_the_placement_fields_and_every_spec_field() {
        let placement = BTreeSet::from(["type", "subject", "realization", "connect", "with"]);
        let spec = field_names::<ParticipantSpec>();
        assert!(spec.contains("image") && spec.contains("collect"), "{:?}", spec);
        let expected: BTreeSet<_> = placement.union(&spec).copied().collect();
        assert_eq!(field_names::<ParticipantEntry>(), expected);
    }

    // Not a case under `testdata/bad/`: a Windows checkout writes a committed symlink as a plain file.
    #[test]
    fn a_symlink_in_a_directory_source_cannot_leave_the_suite() {
        let root = tempfile::tempdir().unwrap();
        let outside = root.path().join("outside.yaml");
        fs::write(&outside, "secret: true\n").unwrap();
        let case = root.path().join("suite").join("leaky");
        fs::create_dir_all(case.join("d")).unwrap();
        fs::write(
            case.join(CASE_FILE),
            r#"
timeout: 1m
subject_os: [linux]
participants:
  app:
    subject: true
    image: alpine:3.20
    files:
      /etc/app/: d/
procedure: []
"#,
        )
        .unwrap();
        let link = case.join("d").join("leak");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&outside, &link).unwrap();

        let Err(errors) = load(&case, &Registry::default()) else {
            panic!("a link out of the suite loaded");
        };
        let errors: Vec<String> = errors.iter().map(ToString::to_string).collect();
        assert!(
            errors
                .iter()
                .any(|error| error.contains("'leak' is outside the suite directory")),
            "{:?}",
            errors
        );
    }
}
