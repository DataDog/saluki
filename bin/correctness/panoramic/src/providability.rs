//! Checks selected plans against the host and reports unavailable realizations as case setup errors.

use std::collections::BTreeMap;

use panoramic_core::{backend::RunBackends, layout::Realization, plan::CasePlan};

/// Returns each realization the host cannot provide, with the reason.
pub(crate) async fn check(backends: &RunBackends, plans: &[CasePlan]) -> BTreeMap<Realization, String> {
    let needed = plans
        .iter()
        .flat_map(|plan| plan.layout().iter().map(|(_, realization)| realization));
    backends.unprovidable(needed).await
}

/// Whether the host can provide every realization in the plan.
pub(crate) fn is_providable(plan: &CasePlan, unprovidable: &BTreeMap<Realization, String>) -> bool {
    plan.layout()
        .iter()
        .all(|(_, realization)| !unprovidable.contains_key(&realization))
}

/// Reports every participant whose realization the host cannot provide.
pub(crate) fn error(plan: &CasePlan, unprovidable: &BTreeMap<Realization, String>) -> Option<String> {
    if is_providable(plan, unprovidable) {
        return None;
    }
    let messages: Vec<_> = plan
        .layout()
        .iter()
        .filter_map(|(name, realization)| {
            unprovidable.get(&realization).map(|reason| {
                format!(
                    "realization {} on {} for participant '{name}' cannot be provided on this host: {reason}",
                    realization.kind.name(),
                    realization.os.name()
                )
            })
        })
        .collect();
    Some(messages.join("; "))
}

#[cfg(test)]
mod tests {
    use std::{path::Path, time::Duration};

    use panoramic_core::{
        layout::{Os, RealizationKind},
        result::{ErrorKind, TestOutcome},
    };
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        config::tests::{plan, PARTICIPANT_FIELDS},
        runner::{
            test_support::{run_case, Behavior, ScriptedTest},
            BuiltCase,
        },
    };

    #[tokio::test]
    async fn check_returns_the_realizations_it_could_not_provide() {
        let plans = [plan(&PARTICIPANT_FIELDS.replace("[linux]", "[macos]"), Os::MacOs)];
        let unprovidable = check(&RunBackends::default(), &plans).await;
        if Os::host() == Os::MacOs {
            assert!(unprovidable.is_empty());
            assert!(is_providable(&plans[0], &unprovidable));
            assert_eq!(error(&plans[0], &unprovidable), None);
        } else {
            assert!(!is_providable(&plans[0], &unprovidable));
            assert_eq!(
                unprovidable,
                BTreeMap::from([(
                    Realization {
                        os: Os::MacOs,
                        kind: RealizationKind::HostProcessGroup
                    },
                    format!("this host runs {}, not macos", Os::host().name()),
                )])
            );
            assert_eq!(
                error(&plans[0], &unprovidable),
                Some(format!(
                    "realization host-process-group on macos for participant 'adp' cannot be provided on this host: this host runs {}, not macos",
                    Os::host().name()
                ))
            );
        }
    }

    #[tokio::test]
    async fn only_cases_needing_an_unprovidable_realization_get_a_setup_error() {
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[macos]").replace(
            "  adp:",
            "  zebra: {type: agent-adp}\n  alpha: {type: agent-adp, os: linux}\n  adp:",
        );
        let plans = [
            plan(&yaml, Os::MacOs),
            plan(PARTICIPANT_FIELDS, Os::Linux),
            plan(&PARTICIPANT_FIELDS.replace("[linux]", "[windows]"), Os::Windows),
        ];
        let unavailable = BTreeMap::from([
            (
                Realization {
                    os: Os::MacOs,
                    kind: RealizationKind::HostProcessGroup,
                },
                "wrong host".to_string(),
            ),
            (
                Realization {
                    os: Os::Linux,
                    kind: RealizationKind::LinuxContainer,
                },
                "no daemon".to_string(),
            ),
        ]);
        let logs = tempfile::tempdir().unwrap();
        let participant_message = concat!(
            "realization host-process-group on macos for participant 'adp' cannot be provided on this host: wrong host; ",
            "realization linux-container on linux for participant 'alpha' cannot be provided on this host: no daemon; ",
            "realization host-process-group on macos for participant 'zebra' cannot be provided on this host: wrong host",
        );
        let linux_message =
            "realization linux-container on linux for participant 'adp' cannot be provided on this host: no daemon";
        for (plan, expected) in plans
            .into_iter()
            .zip([Some(participant_message), Some(linux_message), None])
        {
            assert!(is_providable(&plan, &BTreeMap::new()));
            assert_eq!(is_providable(&plan, &unavailable), expected.is_none());
            let message = error(&plan, &unavailable);
            assert_eq!(message.as_deref(), expected);
            let built = match message {
                Some(message) => BuiltCase::unprovidable(plan, message),
                None => BuiltCase::new(plan, Box::new(ScriptedTest::new("case", Behavior::Pass))),
            };
            let result = run_case(&built, logs.path(), &CancellationToken::new(), Duration::from_secs(5)).await;
            if let Some(expected) = expected {
                assert_eq!(result.outcome, TestOutcome::Errored);
                let error = result.error.unwrap();
                assert_eq!(error.kind, ErrorKind::Setup);
                assert_eq!(error.message, expected);
                assert_eq!(result.duration, Duration::ZERO);
                assert!(result.reclaim.is_empty());
                assert!(result.issues.is_empty());
            } else {
                assert_eq!(result.outcome, TestOutcome::Passed);
            }
        }
    }

    #[tokio::test]
    async fn an_unprovidable_case_reports_errored_setup_and_writes_result_json() {
        let plan = plan(PARTICIPANT_FIELDS, Os::Linux);
        let message = error(
            &plan,
            &BTreeMap::from([(
                Realization {
                    os: Os::Linux,
                    kind: RealizationKind::LinuxContainer,
                },
                "unavailable".to_string(),
            )]),
        )
        .unwrap();
        let built = BuiltCase::unprovidable(plan, message);
        let logs = tempfile::tempdir().unwrap();
        let result = run_case(&built, logs.path(), &CancellationToken::new(), Duration::from_secs(5)).await;
        assert_eq!(result.name, "case");
        assert_eq!(
            result.case_path.as_deref(),
            Some(Path::new("/test/integration/cases/case"))
        );
        assert_eq!(result.outcome, TestOutcome::Errored);
        assert!(result.reclaim.is_empty());
        assert!(result.issues.is_empty());
        let report: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(result.log_dir.unwrap().join("result.json")).unwrap())
                .unwrap();
        assert_eq!(report["name"], "case");
        assert_eq!(report["outcome"], "errored");
        assert_eq!(report["error"]["kind"], "setup");
        assert_eq!(report["reclaim"], serde_json::json!([]));
        assert_eq!(
            report["error"]["message"],
            "realization linux-container on linux for participant 'adp' cannot be provided on this host: unavailable"
        );
    }
}
