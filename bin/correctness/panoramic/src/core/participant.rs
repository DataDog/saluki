//! Participant types, realizations and the operating systems participants run.
//!
//! A participant type is one function that fills in a [`ParticipantSpec`] for one piece of software on one
//! realization. Core lays the case's own fields over the result, so a participant type holds defaults, and a case can
//! set or replace anything it sets. A participant with no participant type starts from an empty spec.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};

use super::case::{is_name, is_participant_name, CaseSources};
use super::spec::{FileSource, ParticipantSpec};

/// An operating system a participant runs.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Os {
    Linux,
    Macos,
    Windows,
}

impl Os {
    /// The OS of the runner host.
    pub(crate) fn host() -> Self {
        if cfg!(target_os = "macos") {
            Self::Macos
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::Linux
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::Macos => "macos",
            Self::Windows => "windows",
        }
    }
}

impl fmt::Display for Os {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Os {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        [Self::Linux, Self::Macos, Self::Windows]
            .into_iter()
            .find(|os| os.as_str() == s)
            .ok_or_else(|| format!("unknown OS '{}': expected linux, macos or windows", s))
    }
}

/// One of the four ways a participant runs.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Enclosure {
    LinuxContainer,
    WindowsContainer,
    KubernetesPod,
    HostProcessGroup,
}

impl Enclosure {
    /// The enclosure a subject gets on `os` when the case does not request one.
    pub(crate) fn default_for(os: Os) -> Self {
        match os {
            Os::Linux => Self::LinuxContainer,
            Os::Macos => Self::HostProcessGroup,
            Os::Windows => Self::WindowsContainer,
        }
    }

    /// The OS software in this enclosure runs, or `None` for host processes, which run the runner host's.
    pub(crate) fn os(self) -> Option<Os> {
        match self {
            Self::LinuxContainer | Self::KubernetesPod => Some(Os::Linux),
            Self::WindowsContainer => Some(Os::Windows),
            Self::HostProcessGroup => None,
        }
    }

    /// Whether what runs is an image with a command, rather than host programs.
    pub(crate) fn runs_an_image(self) -> bool {
        !matches!(self, Self::HostProcessGroup)
    }

    /// The realization options a case may set on this kind.
    // TODO: add each option with the case that first needs it: `host_cgroup_namespace` on a Linux container, and
    // `host_pid` and `cri_socket` on a pod.
    pub(crate) fn options(self) -> &'static [&'static str] {
        &[]
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::LinuxContainer => "linux-container",
            Self::WindowsContainer => "windows-container",
            Self::KubernetesPod => "kubernetes-pod",
            Self::HostProcessGroup => "host-process-group",
        }
    }
}

impl fmt::Display for Enclosure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How one participant runs: its OS and its enclosure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct Realization {
    pub(crate) os: Os,
    pub(crate) enclosure: Enclosure,
}

/// The endpoint of another participant that a participant consumes: `<participant>.<endpoint>`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ConnectionTarget {
    pub(crate) participant: String,
    pub(crate) endpoint: String,
}

impl fmt::Display for ConnectionTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.participant, self.endpoint)
    }
}

impl<'de> Deserialize<'de> for ConnectionTarget {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let text = super::spec::strict_string(deserializer)?;
        match text.split_once('.') {
            Some((participant, endpoint)) if is_participant_name(participant) && is_name(endpoint) => Ok(Self {
                participant: participant.to_string(),
                endpoint: endpoint.to_string(),
            }),
            _ => Err(serde::de::Error::custom(format!(
                "connection target '{}' must be written '<participant>.<endpoint>'",
                text
            ))),
        }
    }
}

/// The Rust definition a participant is an instance of.
pub(crate) trait ParticipantType: Send + Sync {
    /// The name a case writes as `type`.
    fn name(&self) -> &'static str;

    /// The (participant OS, enclosure) pairs this participant type has a spec for.
    fn supports(&self) -> &'static [(Os, Enclosure)];

    /// Fills in the spec for one participant on one realization.
    ///
    /// Core calls this while planning, once per case and declared subject OS. It does no I/O. Core then lays the
    /// case's own fields over the result, so it holds defaults, not final values. It may hold references; core
    /// resolves them when their values exist.
    ///
    /// # Errors
    ///
    /// Returns an error for a `with` parameter, an option or a case value this participant type cannot honor on
    /// this realization.
    fn spec(&self, cx: &SpecContext<'_>) -> Result<ParticipantSpec, String>;
}

/// What a participant type may read while it fills in a spec.
// TODO: remove the dead_code allowance once a participant type reads its context.
#[allow(dead_code)]
pub(crate) struct SpecContext<'a> {
    pub(crate) realization: Realization,
    /// The realization options the case set.
    pub(crate) options: &'a BTreeMap<String, bool>,
    /// What the case wrote for this OS, with references unresolved.
    pub(crate) case: &'a ParticipantSpec,
    pub(crate) connect: &'a BTreeMap<String, ConnectionTarget>,
    pub(super) with: &'a BTreeMap<String, serde_json::Value>,
    pub(super) sources: &'a CaseSources,
}

// TODO: remove the dead_code allowance once a participant type reads its context.
#[allow(dead_code)]
impl SpecContext<'_> {
    /// Decodes `with` into the participant type's own struct, which should deny unknown fields.
    ///
    /// # Errors
    ///
    /// Returns an error if `with` does not decode into `T`.
    pub(crate) fn with<T>(&self) -> Result<T, String>
    where
        T: DeserializeOwned,
    {
        let with = serde_json::Value::Object(self.with.clone().into_iter().collect());
        serde_json::from_value(with).map_err(|e| format!("with: {}", e))
    }

    /// The text of the file the case supplies under a name the participant type declares, as written.
    ///
    /// Returns `None` when the case supplies no file under that name.
    ///
    /// # Errors
    ///
    /// Returns an error if the case supplies a directory or a file that is not UTF-8 under that name.
    pub(crate) fn file_text(&self, name: &str) -> Result<Option<&str>, String> {
        let Some(file) = self.case.files.get(name) else {
            return Ok(None);
        };
        match &file.source {
            FileSource::Case { from, .. } => self.sources.text(from).map(Some),
            FileSource::Bytes { .. } | FileSource::Text { .. } => Ok(None),
        }
    }
}
