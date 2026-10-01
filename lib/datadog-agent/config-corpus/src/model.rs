//! The typed corpus, with every member a writer may omit filled in.

use std::collections::BTreeMap;
use std::fmt;
use std::num::NonZeroU64;

use serde_json::Value;

use crate::getter::GetterResult;
use crate::lists::{Getter, Group, Level, Source};

/// The name of the baseline case, whose key lines are layer 1 of every first snapshot (record.md §3.2).
pub const BASELINE_CASE: &str = "baseline-default";

/// A whole corpus (record.md §1).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Corpus {
    /// The header line (record.md §2).
    pub header: Header,
    /// The cases, in file order, which is byte order of their names.
    pub cases: Vec<Case>,
}

/// The header line (record.md §2). The reader accepts only `format` 1, so it is not kept.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Header {
    /// The full 40-hex Agent commit the recorder built against.
    pub agent_commit: String,
    /// `runtime.GOOS` of the recorder processes.
    pub goos: String,
    /// `runtime.GOARCH` of the recorder processes.
    pub goarch: String,
    /// `runtime.Version()`.
    pub go_version: String,
    /// The digest-pinned image the recorder processes ran in.
    pub container_image: String,
    /// `env.IsContainerized()` in the baseline process.
    pub containerized: bool,
    /// Sorted detected features in the baseline process (record.md §6).
    pub features: Vec<String>,
    /// `sha256:` and 64 hex: the recorder inputs the corpus was made from.
    pub inputs_digest: String,
}

/// One case: its case line and its key lines (record.md §3, §5).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Case {
    /// The case name.
    pub name: String,
    /// The coverage group.
    pub group: Group,
    /// The 1-based `corpus.jsonl` line of the case line, which holds the case's `inputs`
    /// (record.md §3.1): where a diagnostic about generated inputs points.
    pub input_line: usize,
    /// Behavior catalog ids, in file order.
    pub why: Vec<String>,
    /// The inputs, with `keys` reconstructed when omitted (record.md §3.1).
    pub inputs: Inputs,
    /// Filtered warnings logged before the first snapshot (record.md §7).
    pub construction_warnings: Vec<Warning>,
    /// Whether the config was constructed, and what was recorded if so.
    pub outcome: Outcome,
}

/// How a case's construction ended (record.md §3.3, §4.1).
#[derive(Clone, Debug)]
pub enum Outcome {
    /// Construction succeeded and the first snapshot arrived.
    Started(Started),
    /// Construction returned this error; nothing else was recorded.
    StartupError(String),
}

/// What a case that started recorded.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Started {
    /// The first snapshot's `origin`.
    pub origin: String,
    /// `env.IsContainerized()`, taken from the header when the case line omits it.
    pub containerized: bool,
    /// Detected features, taken from the header when the case line omits them.
    pub features: Vec<String>,
    /// Settings that differ from the baseline's first snapshot, by key (record.md §3.2).
    pub side_effects: Vec<SideEffect>,
    /// One result per case update, in order (record.md §4.2).
    pub updates: Vec<UpdateResult>,
    /// The key lines, in byte order of key (record.md §5).
    pub keys: Vec<KeyLine>,
}

/// The case's inputs as the case file gave them (record.md §3.1).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Inputs {
    /// The whole process environment, when the case gave one.
    pub env: Option<BTreeMap<String, String>>,
    /// The verbatim `datadog.yaml`, when given.
    pub yaml: Option<String>,
    /// The verbatim fleet `datadog.yaml`, when given.
    pub fleet_policy: Option<String>,
    /// Startup CLI overrides.
    pub cli: Vec<CliOverride>,
    /// Runtime updates, in order.
    pub updates: Vec<Update>,
    /// The keys to record, in the case's order; reconstructed from the key lines when omitted.
    pub keys: Vec<KeySpec>,
}

/// A startup CLI override (case.md §4.5).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct CliOverride {
    /// The key.
    pub key: String,
    /// The typed value, in the JSON form of case.md §7.
    pub value: Value,
}

/// A runtime update (case.md §5).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Update {
    /// The key as passed to the Agent.
    pub key: String,
    /// What the update does.
    pub op: UpdateOp,
    /// The layer written or cleared; one of [`Source::UPDATE`].
    pub source: Source,
}

/// A runtime update's operation.
#[derive(Clone, Debug)]
pub enum UpdateOp {
    /// `cfg.Set` with this typed value (case.md §7).
    Set(Value),
    /// `cfg.UnsetForSource`.
    Unset,
}

/// An entry of `inputs.keys` (case.md §6).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct KeySpec {
    /// The key.
    pub key: String,
    /// The case's getter override, if it gave one.
    pub getters: Option<Vec<Getter>>,
}

/// One streamed `pb.ConfigSetting`, without its key (record.md §5.1).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Setting {
    /// The setting's source.
    pub source: Source,
    /// The source an unset cleared, if any.
    pub unset_source: Option<Source>,
    /// The protojson value, parsed; `None` when the proto field is unset.
    pub value: Option<Value>,
}

/// An element of `side_effects` (record.md §3.2).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct SideEffect {
    /// The key.
    pub key: String,
    /// The case's setting, or `None` when the key is absent from the case's first snapshot.
    pub setting: Option<Setting>,
}

/// The recorded outcome of one update (record.md §4.2).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct UpdateResult {
    /// Notifications the config issued: `after - before`.
    pub seq_delta: u64,
    /// Whether the wait for events ended at 5 s; never with `seq_delta` 0.
    pub timed_out: bool,
    /// Warnings logged during the update call.
    pub warnings: Vec<Warning>,
}

/// A key line (record.md §5).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct KeyLine {
    /// The key.
    pub key: String,
    /// The first snapshot's setting for the key, or `None` when absent.
    pub snapshot: Option<Setting>,
    /// Later stream events for the key, in arrival order.
    pub events: Vec<Event>,
    /// The getter reads.
    pub reads: Reads,
}

/// A stream event (record.md §5.2).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Event {
    /// The streamed setting.
    pub setting: Setting,
    /// `sequence_id - before` of its update; the first event of an update is 1.
    pub seq: NonZeroU64,
    /// The index of the update the event belongs to, reconstructed when omitted.
    pub update: usize,
}

/// A key line's reads (record.md §5.3).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Reads {
    /// The read after the first snapshot, before any update.
    pub snapshot: Read,
    /// The read after the last update, present exactly when the case has updates.
    pub final_: Option<Read>,
}

/// The getter reads at one checkpoint (record.md §5.3).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Read {
    /// One result per getter in the key's list, in order.
    pub getters: Vec<GetterRead>,
    /// `fmt.Sprintf("%T", cfg.Get(key))`.
    pub go_type: String,
    /// `cfg.GetSource(key)`, reconstructed from the streamed source when omitted.
    pub source: Source,
}

/// One getter call's result (record.md §5.3; getter-map.md §3).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct GetterRead {
    /// The getter called.
    pub getter: Getter,
    /// The decoded result.
    pub result: GetterResult,
    /// Warnings the call logged that mention the key.
    pub warnings: Vec<Warning>,
}

/// A recorded warning (record.md §7).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Warning {
    /// The slog level.
    pub level: Level,
    /// The record's message.
    pub message: String,
}

/// Which family of format rules a violation breaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    /// Encoding and line structure (record.md §1).
    File,
    /// Canonical JSON: whitespace, member order, escaping (record.md §1).
    Canonical,
    /// Line order (record.md §1).
    Order,
    /// A member's presence, type, `null`-ness or allowed values.
    Shape,
    /// The consistency rules between members and lines (record.md §3.3).
    Consistency,
    /// The rules for members a writer omits (record.md §3.1, §5.2, §5.3).
    Reconstruction,
    /// A getter result that does not match its getter's encoding (getter-map.md §3).
    GetterResult,
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Rule::File => "file",
            Rule::Canonical => "canonical form",
            Rule::Order => "order",
            Rule::Shape => "shape",
            Rule::Consistency => "consistency",
            Rule::Reconstruction => "reconstruction",
            Rule::GetterResult => "getter result",
        })
    }
}

/// A broken format rule.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Violation {
    /// The 1-based line, or 0 for the file as a whole.
    pub line: usize,
    /// The rule family.
    pub rule: Rule,
    /// What is wrong.
    pub message: String,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            write!(f, "file: {}: {}", self.rule, self.message)
        } else {
            write!(f, "line {}: {}: {}", self.line, self.rule, self.message)
        }
    }
}

impl Case {
    /// The stable origin of the case's checks: what produced them, stripped of the naming a
    /// schema bump reshuffles (case.md §3.2, §3.2.1).
    ///
    /// The `baseline`, `behavior` and `unknown` groups keep the case name. A `depth` case drops
    /// the `--<key>` suffix of a bisection part, leaving `depth-<variant>`. The batched groups
    /// (`breadth`, `unsupported`, `excluded`) drop the section batch and any `--<key>` suffix,
    /// leaving `<group>-<source>`, since a recorded read's own `key` member already names the
    /// exact setting. This is an assertion identity for replay expectations, not a runtime case
    /// filter or a category grouping.
    pub fn check_origin(&self) -> &str {
        match self.group {
            Group::Baseline | Group::Behavior | Group::Unknown => &self.name,
            Group::Depth => match self.name.split_once("--") {
                Some((origin, _)) => origin,
                None => &self.name,
            },
            Group::Breadth | Group::Unsupported | Group::Excluded => {
                let mut segments = self.name.splitn(3, '-');
                let len = match (segments.next(), segments.next()) {
                    (Some(group), Some(source)) => group.len() + 1 + source.len(),
                    _ => return &self.name,
                };
                self.name.get(..len).unwrap_or(&self.name)
            }
        }
    }
}

impl Corpus {
    /// The case with this name.
    pub fn case(&self, name: &str) -> Option<&Case> {
        self.cases
            .binary_search_by(|c| c.name.as_bytes().cmp(name.as_bytes()))
            .ok()
            .map(|i| &self.cases[i])
    }

    /// Rebuilds a case's first snapshot, key to setting, by the three layers of record.md §3.2:
    /// the `baseline-default` case's key-line snapshots, then the case's `side_effects` (an absent
    /// element removes the key), then the case's own key-line snapshots (`null` removes the key).
    ///
    /// Keys the corpus neither models nor saw change are not in the result. Returns `None` when the
    /// case does not exist or did not start. Without a started baseline case, layer 1 is empty.
    pub fn first_snapshot(&self, case: &str) -> Option<BTreeMap<&str, &Setting>> {
        let Outcome::Started(started) = &self.case(case)?.outcome else {
            return None;
        };
        let mut snapshot = BTreeMap::new();
        if let Some(Case {
            outcome: Outcome::Started(baseline),
            ..
        }) = self.case(BASELINE_CASE)
        {
            for k in &baseline.keys {
                if let Some(s) = &k.snapshot {
                    snapshot.insert(k.key.as_str(), s);
                }
            }
        }
        for e in &started.side_effects {
            match &e.setting {
                Some(s) => snapshot.insert(e.key.as_str(), s),
                None => snapshot.remove(e.key.as_str()),
            };
        }
        for k in &started.keys {
            match &k.snapshot {
                Some(s) => snapshot.insert(k.key.as_str(), s),
                None => snapshot.remove(k.key.as_str()),
            };
        }
        Some(snapshot)
    }
}
