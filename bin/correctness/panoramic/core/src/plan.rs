//! Compiles loaded cases for a selected subject operating system.

use saluki_error::{generic_error, GenericError};

use crate::{
    case::{CaseConfig, CaseSchema, Registries},
    layout::{lay_out, Layout, Os},
};

/// A loaded case with the layout selected for execution.
pub struct CasePlan<C> {
    case: CaseConfig<C>,
    subject_os: Os,
    layout: Option<Layout>,
}

impl<C> CasePlan<C> {
    /// Validates every declared subject OS, returning a plan only for a declared selection.
    /// Participant cases carry a layout; typed cases retain their loader-supplied data.
    ///
    /// # Errors
    ///
    /// Returns the first layout error, naming the case, source and failing subject OS,
    /// even when the selected OS is excluded by the case.
    pub fn compile(
        case: CaseConfig<C>, subject_os: Os, registries: &Registries<'_>,
    ) -> Result<Option<Self>, GenericError> {
        let mut layout = None;
        if let CaseSchema::Participants { file, .. } = case.schema() {
            for &os in case.subject_os() {
                let candidate = lay_out(&file.participants, registries.participant_types, os).map_err(|message| {
                    generic_error!(
                        "case '{}' in {}: on subject OS '{}', {}",
                        case.name(),
                        case.source().display(),
                        os.name(),
                        message
                    )
                })?;
                if os == subject_os {
                    layout = Some(candidate);
                }
            }
        }
        if !case.subject_os().contains(&subject_os) {
            return Ok(None);
        }
        Ok(Some(Self {
            case,
            subject_os,
            layout,
        }))
    }

    /// Returns the loaded case and its metadata.
    pub fn case(&self) -> &CaseConfig<C> {
        &self.case
    }

    /// Returns the subject OS selected for execution.
    pub fn subject_os(&self) -> Os {
        self.subject_os
    }

    /// Returns `Some(layout)` for participant cases and `None` for typed cases.
    pub fn layout(&self) -> Option<&Layout> {
        self.layout.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::{
        case::fixtures::{LOADER, PARTICIPANT_FIELDS, REGISTRIES},
        layout::{Realization, RealizationKind},
    };

    #[test]
    fn a_participant_case_is_planned_only_for_a_subject_os_it_declares() {
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[linux, windows]");
        for (os, kind) in [
            (Os::Linux, Some(RealizationKind::LinuxContainer)),
            (Os::MacOs, None),
            (Os::Windows, Some(RealizationKind::WindowsContainer)),
        ] {
            let case = LOADER
                .load_content(&yaml, "alpha", Path::new("cases/alpha/config.yaml"))
                .unwrap()
                .remove(0);
            let plan = CasePlan::compile(case, os, &REGISTRIES).unwrap();
            if let Some(kind) = kind {
                let plan = plan.unwrap();
                assert_eq!(plan.case().name(), "alpha");
                assert_eq!(plan.subject_os(), os);
                assert_eq!(
                    plan.layout().unwrap().iter().collect::<Vec<_>>(),
                    [("subject", Realization { os, kind })]
                );
            } else {
                assert!(plan.is_none());
            }
        }
    }

    #[test]
    fn a_layout_error_on_any_declared_subject_os_fails_compile_whatever_os_is_selected() {
        let yaml = PARTICIPANT_FIELDS
            .replace("[linux]", "[linux, windows]")
            .replace("subject: true", "subject: true, realization: {kind: linux-container}");
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            let case = LOADER
                .load_content(&yaml, "alpha", Path::new("cases/alpha/config.yaml"))
                .unwrap()
                .remove(0);
            let message = CasePlan::compile(case, os, &REGISTRIES).err().unwrap().to_string();
            assert!(
                message.contains(
                    "on subject OS 'windows', participant 'subject' of type 'server' \
                     cannot run as linux-container on windows"
                ),
                "{message}"
            );
        }
    }

    #[test]
    fn a_typed_case_is_planned_without_a_layout_for_its_declared_subject_os() {
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            let case = LOADER
                .load_content("type: fixture", "alpha", Path::new("config.yaml"))
                .unwrap()
                .remove(0);
            let plan = CasePlan::compile(case, os, &REGISTRIES).unwrap();
            if os == Os::Linux {
                assert!(plan.is_none());
            } else {
                let plan = plan.unwrap();
                assert_eq!(plan.subject_os(), os);
                assert_eq!(plan.case().name(), "alpha");
                assert!(plan.layout().is_none());
            }
        }
    }

    #[test]
    fn compile_errors_name_the_case_source_and_subject_os() {
        let yaml = PARTICIPANT_FIELDS.replace("subject: true", "subject: true, realization: {kind: kubernetes-pod}");
        let case = LOADER
            .load_content(&yaml, "alpha", Path::new("cases/alpha/config.yaml"))
            .unwrap()
            .remove(0);
        let message = CasePlan::compile(case, Os::Linux, &REGISTRIES)
            .err()
            .unwrap()
            .to_string();
        assert_eq!(
            message,
            "case 'alpha' in cases/alpha/config.yaml: on subject OS 'linux', \
             participant 'subject' of type 'server' cannot run as kubernetes-pod on linux; \
             it supports linux-container on linux, host-process-group on macos, windows-container on windows"
        );
    }
}
