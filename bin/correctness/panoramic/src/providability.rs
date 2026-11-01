//! Checks selected plans against the host and reports unavailable realizations as case setup errors.

use std::{collections::BTreeMap, time::Duration};

use async_trait::async_trait;
use panoramic_core::{
    layout::Realization,
    result::{ErrorKind, TestResult},
};

use crate::{
    config::{typed_case::realizations, BuiltCase},
    test::{Test, TestContext},
};

/// Checks all selected plans together, replacing cases the host cannot provide with setup errors.
/// Returns each realization the host cannot provide, with the reason.
pub(crate) async fn check(built: &mut [BuiltCase]) -> BTreeMap<Realization, String> {
    let needed = built.iter().flat_map(|built| {
        realizations(&built.plan)
            .into_iter()
            .map(|(_, realization)| realization)
    });
    let unprovidable = panoramic_core::backend::unprovidable(needed).await;
    apply(built, &unprovidable);
    unprovidable
}

fn apply(built: &mut [BuiltCase], unprovidable: &BTreeMap<Realization, String>) {
    for built in built {
        let messages: Vec<_> = realizations(&built.plan)
            .into_iter()
            .filter_map(|(name, realization)| {
                unprovidable.get(&realization).map(|reason| {
                    let participant = name.map_or_else(String::new, |name| format!(" for participant '{name}'"));
                    format!(
                        "realization {} on {}{participant} cannot be provided on this host: {reason}",
                        realization.kind.name(),
                        realization.os.name()
                    )
                })
            })
            .collect();
        if !messages.is_empty() {
            built.runner = Box::new(UnprovidableCase {
                message: messages.join("; "),
            });
        }
    }
}

/// The setup error for a case this host cannot provide.
struct UnprovidableCase {
    message: String,
}

#[async_trait]
impl Test for UnprovidableCase {
    fn images(&self) -> BTreeMap<&str, String> {
        BTreeMap::new()
    }

    async fn run(&self, _tctx: TestContext) -> TestResult {
        TestResult::errored("", ErrorKind::Setup, &self.message, Duration::ZERO, vec![])
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use panoramic_core::{
        layout::{Os, RealizationKind},
        result::TestOutcome,
    };
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        config::tests::{plan, CORRECTNESS_FIELDS, PARTICIPANT_FIELDS},
        runner::test_support::{run_with_settings, test_runner_settings, Behavior, ScriptedTest},
    };

    struct ImageTest;

    #[async_trait]
    impl Test for ImageTest {
        fn images(&self) -> BTreeMap<&str, String> {
            BTreeMap::from([("target", "image:original".to_string())])
        }

        async fn run(&self, tctx: TestContext) -> TestResult {
            ScriptedTest::new("case", Behavior::Pass).run(tctx).await
        }
    }

    #[tokio::test]
    async fn check_returns_the_realizations_it_could_not_provide() {
        let mut built = [BuiltCase {
            plan: plan(&PARTICIPANT_FIELDS.replace("[linux]", "[macos]"), Os::MacOs),
            runner: Box::new(ImageTest),
        }];
        let unprovidable = check(&mut built).await;
        if Os::host() == Os::MacOs {
            assert!(unprovidable.is_empty());
            assert_eq!(built[0].runner.images(), ImageTest.images());
        } else {
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
            assert!(built[0].runner.images().is_empty());
        }
    }

    #[tokio::test]
    async fn only_cases_needing_an_unprovidable_realization_get_a_setup_error_runner() {
        let yaml = PARTICIPANT_FIELDS.replace("[linux]", "[macos]").replace(
            "  adp:",
            "  zebra: {type: agent-adp}\n  alpha: {type: agent-adp, os: linux}\n  adp:",
        );
        let mut built = [
            BuiltCase {
                plan: plan(&yaml, Os::MacOs),
                runner: Box::new(ImageTest),
            },
            BuiltCase {
                plan: plan(&format!("type: correctness\n{CORRECTNESS_FIELDS}"), Os::Linux),
                runner: Box::new(ImageTest),
            },
            BuiltCase {
                plan: plan(&PARTICIPANT_FIELDS.replace("[linux]", "[windows]"), Os::Windows),
                runner: Box::new(ImageTest),
            },
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
        apply(&mut built, &unavailable);
        let logs = tempfile::tempdir().unwrap();
        let participant_message = concat!(
            "realization host-process-group on macos for participant 'adp' cannot be provided on this host: wrong host; ",
            "realization linux-container on linux for participant 'alpha' cannot be provided on this host: no daemon; ",
            "realization host-process-group on macos for participant 'zebra' cannot be provided on this host: wrong host",
        );
        let typed_message = "realization linux-container on linux cannot be provided on this host: no daemon";
        for (built, expected) in built.iter().zip([Some(participant_message), Some(typed_message), None]) {
            let result = run_with_settings(
                &built.plan,
                built.runner.as_ref(),
                test_runner_settings(logs.path()),
                logs.path(),
                &CancellationToken::new(),
                Duration::from_secs(5),
            )
            .await;
            if let Some(expected) = expected {
                assert_eq!(result.outcome, TestOutcome::Errored);
                let error = result.error.unwrap();
                assert_eq!(error.kind, ErrorKind::Setup);
                assert_eq!(error.message, expected);
                assert_eq!(result.duration, Duration::ZERO);
                assert!(built.runner.images().is_empty());
            } else {
                assert_eq!(built.runner.images(), ImageTest.images());
                assert_eq!(result.outcome, TestOutcome::Passed);
            }
        }
    }

    #[tokio::test]
    async fn an_unprovidable_case_reports_errored_setup_and_writes_result_json() {
        let mut built = [BuiltCase {
            plan: plan(PARTICIPANT_FIELDS, Os::Linux),
            runner: Box::new(ImageTest),
        }];
        apply(
            &mut built,
            &BTreeMap::from([(
                Realization {
                    os: Os::Linux,
                    kind: RealizationKind::LinuxContainer,
                },
                "unavailable".to_string(),
            )]),
        );
        let logs = tempfile::tempdir().unwrap();
        let result = run_with_settings(
            &built[0].plan,
            built[0].runner.as_ref(),
            test_runner_settings(logs.path()),
            logs.path(),
            &CancellationToken::new(),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(result.name, "case");
        assert_eq!(result.case_path.as_deref(), Some(Path::new("/cases/case")));
        assert_eq!(result.outcome, TestOutcome::Errored);
        let report: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(result.log_dir.unwrap().join("result.json")).unwrap())
                .unwrap();
        assert_eq!(report["name"], "case");
        assert_eq!(report["outcome"], "errored");
        assert_eq!(report["error"]["kind"], "setup");
        assert_eq!(
            report["error"]["message"],
            "realization linux-container on linux for participant 'adp' cannot be provided on this host: unavailable"
        );
    }
}
