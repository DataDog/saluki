//! Places case participants on operating systems and execution mechanisms.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::{
    case::ParticipantEntry,
    participant::{find_type, unknown_type_message, ParticipantType},
};

/// An operating system a case's processes run on.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "lowercase")]
pub enum Os {
    /// Linux.
    Linux,
    /// macOS.
    #[allow(
        clippy::enum_variant_names,
        reason = "macOS is one word; the Os suffix is part of its name."
    )]
    MacOs,
    /// Windows.
    Windows,
}

impl Os {
    /// Returns the runner host's OS: macOS, Windows, or Linux on other hosts.
    pub fn host() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::Linux
        }
    }

    /// Returns the OS spelling used in case files.
    pub fn name(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::MacOs => "macos",
            Self::Windows => "windows",
        }
    }
}

/// Execution mechanisms a case may request.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "kebab-case")]
pub enum RealizationKind {
    /// Processes running directly on the host.
    HostProcessGroup,
    /// A Linux container.
    LinuxContainer,
    /// A Windows container.
    WindowsContainer,
    /// A Kubernetes pod.
    KubernetesPod,
}

impl RealizationKind {
    /// Returns the execution mechanism's spelling in case files.
    pub fn name(self) -> &'static str {
        match self {
            Self::HostProcessGroup => "host-process-group",
            Self::LinuxContainer => "linux-container",
            Self::WindowsContainer => "windows-container",
            Self::KubernetesPod => "kubernetes-pod",
        }
    }
}

/// One participant's operating system and execution mechanism.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Realization {
    /// Operating system of the participant's processes.
    pub os: Os,
    /// Execution mechanism for the participant.
    pub kind: RealizationKind,
}

/// Assigns each participant an OS and realization kind for one subject OS.
#[derive(Debug)]
pub struct Layout {
    participants: BTreeMap<String, Realization>,
}

impl Layout {
    pub(crate) fn get(&self, name: &str) -> Option<Realization> {
        self.participants.get(name).copied()
    }

    /// Visits participants and their realizations in name order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, Realization)> {
        self.participants
            .iter()
            .map(|(name, realization)| (name.as_str(), *realization))
    }
}

/// Formats realizations for diagnostics, preserving their order.
pub(crate) fn describe_realizations(realizations: impl IntoIterator<Item = (Os, RealizationKind)>) -> String {
    let descriptions: Vec<_> = realizations
        .into_iter()
        .map(|(os, kind)| format!("{} on {}", kind.name(), os.name()))
        .collect();
    if descriptions.is_empty() {
        "none".to_string()
    } else {
        descriptions.join(", ")
    }
}

/// The realization kind a subject on `os` gets when the case requests none.
fn default_kind(os: Os) -> RealizationKind {
    match os {
        Os::Linux => RealizationKind::LinuxContainer,
        Os::MacOs => RealizationKind::HostProcessGroup,
        Os::Windows => RealizationKind::WindowsContainer,
    }
}

/// Places participants from a validated case for `subject_os`.
///
/// The subject runs on `subject_os`, using its requested kind or that OS's default.
/// Other participants use their `os` override or the subject OS. A requested kind wins;
/// otherwise they follow the subject's kind on the same OS, or use their own OS's default.
/// Every chosen (OS, kind) pair must appear in the type's [`ParticipantType::supports`];
/// an unsupported choice fails the layout.
///
/// # Errors
///
/// Errors name the participant and its unknown type or unsupported realization.
/// Unknown-type errors list registered types. Unsupported-realization errors give the OS, kind
/// and supported pairs, with an override hint when the participant cannot follow the subject.
pub(crate) fn lay_out(
    participants: &BTreeMap<String, ParticipantEntry>, types: &[&dyn ParticipantType], subject_os: Os,
) -> Result<Layout, String> {
    let subject = participants
        .iter()
        .find(|(_, entry)| entry.subject)
        .expect("loaded cases have exactly one subject");
    let subject_kind = subject
        .1
        .realization
        .as_ref()
        .map_or(default_kind(subject_os), |realization| realization.kind);
    let mut laid_out = BTreeMap::new();
    for (name, entry) in std::iter::once(subject).chain(participants.iter().filter(|(_, entry)| !entry.subject)) {
        let participant_type = find_type(types, &entry.participant_type)
            .ok_or_else(|| unknown_type_message(name, &entry.participant_type, types))?;
        let os = if entry.subject {
            subject_os
        } else {
            entry.os.unwrap_or(subject_os)
        };
        let follows_subject = !entry.subject && entry.realization.is_none() && os == subject_os;
        let kind = entry.realization.as_ref().map_or_else(
            || {
                if os == subject_os {
                    subject_kind
                } else {
                    default_kind(os)
                }
            },
            |realization| realization.kind,
        );
        if !participant_type.supports().contains(&(os, kind)) {
            let supported = describe_realizations(participant_type.supports().iter().copied());
            let reason = if follows_subject {
                format!(
                    "cannot follow the subject's {} on {}; set its 'os' or 'realization'",
                    kind.name(),
                    os.name()
                )
            } else {
                format!("cannot run as {} on {}", kind.name(), os.name())
            };
            return Err(format!(
                "participant '{name}' of type '{}' {reason}; it supports {supported}",
                participant_type.name()
            ));
        }
        laid_out.insert(name.clone(), Realization { os, kind });
    }
    Ok(Layout { participants: laid_out })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Type {
        name: &'static str,
        supports: &'static [(Os, RealizationKind)],
    }

    impl ParticipantType for Type {
        fn name(&self) -> &'static str {
            self.name
        }

        fn supports(&self) -> &'static [(Os, RealizationKind)] {
            self.supports
        }

        fn processes(&self) -> &'static [&'static str] {
            &[]
        }
    }

    const FULL: Type = Type {
        name: "full",
        supports: &[
            (Os::Linux, RealizationKind::LinuxContainer),
            (Os::MacOs, RealizationKind::HostProcessGroup),
            (Os::Windows, RealizationKind::WindowsContainer),
            (Os::Linux, RealizationKind::KubernetesPod),
        ],
    };
    const PORTABLE: Type = Type {
        name: "portable",
        supports: &[
            (Os::Linux, RealizationKind::LinuxContainer),
            (Os::MacOs, RealizationKind::HostProcessGroup),
            (Os::Linux, RealizationKind::KubernetesPod),
        ],
    };
    const FIXTURE: Type = Type {
        name: "fixture-like",
        supports: &[
            (Os::Linux, RealizationKind::LinuxContainer),
            (Os::MacOs, RealizationKind::HostProcessGroup),
        ],
    };
    const TYPES: &[&dyn ParticipantType] = &[&FULL, &PORTABLE, &FIXTURE];

    fn participants(subject: &str, probe: &str) -> BTreeMap<String, ParticipantEntry> {
        serde_saphyr::from_str(&format!("subject: {{subject: true, {subject}}}\nprobe: {{{probe}}}")).unwrap()
    }

    #[test]
    fn participants_follow_the_subject_realization_on_every_subject_platform() {
        for (os, requested, expected, supported) in [
            (Os::Linux, "", RealizationKind::LinuxContainer, [true, true, true]),
            (Os::MacOs, "", RealizationKind::HostProcessGroup, [true, true, true]),
            (Os::Windows, "", RealizationKind::WindowsContainer, [true, false, false]),
            (
                Os::Linux,
                ", realization: {kind: kubernetes-pod}",
                RealizationKind::KubernetesPod,
                [true, true, false],
            ),
        ] {
            for (participant_type, supported) in TYPES.iter().zip(supported) {
                let participants = participants(
                    &format!("type: full{requested}"),
                    &format!("type: {}", participant_type.name()),
                );
                let result = lay_out(&participants, TYPES, os);
                if supported {
                    assert_eq!(
                        result.unwrap().iter().collect::<Vec<_>>(),
                        [
                            ("probe", Realization { os, kind: expected }),
                            ("subject", Realization { os, kind: expected }),
                        ]
                    );
                } else {
                    let message = result.unwrap_err();
                    assert!(
                        message.contains(&format!(
                            "participant 'probe' of type '{}' cannot follow the subject's {} on {}",
                            participant_type.name(),
                            expected.name(),
                            os.name()
                        )),
                        "{message}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_subject_takes_the_default_kind_of_each_subject_os() {
        let participants = participants("type: full", "type: full");
        for (os, kind) in [
            (Os::Linux, RealizationKind::LinuxContainer),
            (Os::MacOs, RealizationKind::HostProcessGroup),
            (Os::Windows, RealizationKind::WindowsContainer),
        ] {
            assert_eq!(
                lay_out(&participants, TYPES, os).unwrap().iter().last(),
                Some(("subject", Realization { os, kind }))
            );
        }
    }

    #[test]
    fn a_participant_on_another_os_takes_that_os_default_kind() {
        let participants = participants("type: full", "type: fixture-like, os: linux");
        assert_eq!(
            lay_out(&participants, TYPES, Os::MacOs)
                .unwrap()
                .iter()
                .collect::<Vec<_>>(),
            [
                (
                    "probe",
                    Realization {
                        os: Os::Linux,
                        kind: RealizationKind::LinuxContainer
                    }
                ),
                (
                    "subject",
                    Realization {
                        os: Os::MacOs,
                        kind: RealizationKind::HostProcessGroup
                    }
                ),
            ]
        );
    }

    #[test]
    fn a_requested_realization_overrides_the_subject_kind() {
        let participants = participants(
            "type: full, realization: {kind: kubernetes-pod}",
            "type: fixture-like, realization: {kind: linux-container}",
        );
        assert_eq!(
            lay_out(&participants, TYPES, Os::Linux)
                .unwrap()
                .iter()
                .collect::<Vec<_>>(),
            [
                (
                    "probe",
                    Realization {
                        os: Os::Linux,
                        kind: RealizationKind::LinuxContainer
                    }
                ),
                (
                    "subject",
                    Realization {
                        os: Os::Linux,
                        kind: RealizationKind::KubernetesPod
                    }
                ),
            ]
        );
    }

    #[test]
    fn a_participant_that_cannot_follow_the_subject_is_an_error_naming_it_its_type_and_the_realization() {
        let participants = participants("type: full, realization: {kind: kubernetes-pod}", "type: fixture-like");
        assert_eq!(
            lay_out(&participants, TYPES, Os::Linux).unwrap_err(),
            "participant 'probe' of type 'fixture-like' cannot follow the subject's kubernetes-pod on linux; \
             set its 'os' or 'realization'; it supports linux-container on linux, host-process-group on macos"
        );
    }

    #[test]
    fn a_subject_whose_type_does_not_support_its_kind_is_an_error() {
        for (os, requested, kind) in [
            (Os::Linux, ", realization: {kind: kubernetes-pod}", "kubernetes-pod"),
            (Os::Windows, "", "windows-container"),
        ] {
            let participants = participants(&format!("type: fixture-like{requested}"), "type: full");
            assert_eq!(
                lay_out(&participants, TYPES, os).unwrap_err(),
                format!(
                    "participant 'subject' of type 'fixture-like' cannot run as {kind} on {}; \
                     it supports linux-container on linux, host-process-group on macos",
                    os.name()
                )
            );
        }
    }

    #[test]
    fn a_requested_realization_the_type_does_not_support_is_an_error_on_any_os() {
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            for override_os in ["", ", os: linux", ", os: macos", ", os: windows"] {
                let participants = participants(
                    "type: full",
                    &format!("type: fixture-like, realization: {{kind: kubernetes-pod}}{override_os}"),
                );
                let participant_os = participants["probe"].os.unwrap_or(os);
                assert_eq!(
                    lay_out(&participants, TYPES, os).unwrap_err(),
                    format!(
                        "participant 'probe' of type 'fixture-like' cannot run as kubernetes-pod on {}; \
                         it supports linux-container on linux, host-process-group on macos",
                        participant_os.name()
                    )
                );
            }
        }
    }

    #[test]
    fn a_participant_on_another_os_whose_type_lacks_that_os_default_kind_is_an_error() {
        let participants = participants("type: full", "type: fixture-like, os: windows");
        assert_eq!(
            lay_out(&participants, TYPES, Os::MacOs).unwrap_err(),
            "participant 'probe' of type 'fixture-like' cannot run as windows-container on windows; \
             it supports linux-container on linux, host-process-group on macos"
        );
    }

    #[test]
    fn a_type_that_declares_no_support_cannot_be_laid_out() {
        struct Empty;
        impl ParticipantType for Empty {
            fn name(&self) -> &'static str {
                "empty"
            }
            fn processes(&self) -> &'static [&'static str] {
                &[]
            }
        }
        let participants = participants("type: empty", "type: full");
        assert_eq!(
            lay_out(&participants, &[&Empty, &FULL], Os::Linux).unwrap_err(),
            "participant 'subject' of type 'empty' cannot run as linux-container on linux; it supports none"
        );
    }

    #[test]
    fn a_participant_of_an_unregistered_type_cannot_be_laid_out() {
        let participants = participants("type: empty", "type: full");
        assert_eq!(
            lay_out(&participants, TYPES, Os::Linux).unwrap_err(),
            "participant 'subject' has unknown type 'empty'; registered types: full, portable, fixture-like"
        );
    }
}
