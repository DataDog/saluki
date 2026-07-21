use std::io::Write;
use std::path::{Path, PathBuf};

use argh::FromArgs;
use async_trait::async_trait;
use saluki_error::{ErrorContext as _, GenericError};

use crate::cli::utils::DataPlaneAPIClient;
use crate::dogstatsd_contexts::read_report;

/// Displays the DogStatsD contexts with the highest cardinality.
#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "top")]
pub(super) struct TopCommand {
    /// read a context dump artifact instead of requesting a new dump
    #[argh(option, short = 'p', long = "path")]
    path: Option<PathBuf>,

    /// set the maximum number of metrics to display
    #[argh(option, short = 'm', long = "num-metrics", default = "10")]
    num_metrics: usize,

    /// set the maximum number of tags to display for each metric
    #[argh(option, short = 't', long = "num-tags")]
    num_tags: Option<usize>,

    /// use the legacy `--mum-tags` typo for `--num-tags`
    #[argh(option, long = "mum-tags")]
    legacy_num_tags: Option<usize>,
}

impl TopCommand {
    pub(super) fn validate(self) -> Result<ValidatedTopCommand, GenericError> {
        let num_tags = match (self.num_tags, self.legacy_num_tags) {
            (Some(_), Some(_)) => {
                return Err(saluki_error::generic_error!(
                    "Cannot use `--num-tags` and legacy `--mum-tags` together; use `--num-tags`."
                ));
            }
            (Some(limit), None) | (None, Some(limit)) => limit,
            (None, None) => 5,
        };

        Ok(ValidatedTopCommand {
            path: self.path,
            num_metrics: self.num_metrics,
            num_tags,
        })
    }
}

#[derive(Debug)]
pub(super) struct ValidatedTopCommand {
    path: Option<PathBuf>,
    num_metrics: usize,
    num_tags: usize,
}

impl ValidatedTopCommand {
    pub(super) fn is_offline(&self) -> bool {
        self.path.is_some()
    }
}

/// Writes a DogStatsD context dump artifact.
#[derive(FromArgs, Debug)]
#[argh(subcommand, name = "dump-contexts")]
pub(super) struct DumpContextsCommand {}

#[async_trait(?Send)]
pub(super) trait DogStatsDContextDumpRequester {
    async fn request_context_dump(&mut self) -> Result<PathBuf, GenericError>;
}

#[async_trait(?Send)]
impl DogStatsDContextDumpRequester for DataPlaneAPIClient {
    async fn request_context_dump(&mut self) -> Result<PathBuf, GenericError> {
        self.dogstatsd_contexts_dump().await
    }
}

pub(super) async fn handle_dogstatsd_top(
    requester: Option<&mut dyn DogStatsDContextDumpRequester>, cmd: ValidatedTopCommand, output: &mut dyn Write,
) -> Result<(), GenericError> {
    let path = match cmd.path {
        Some(path) => path,
        None => {
            let requester = requester.ok_or_else(|| {
                saluki_error::generic_error!("Online DogStatsD top requires a context dump requester.")
            })?;
            let path = requester
                .request_context_dump()
                .await
                .error_context("Failed to request a DogStatsD context dump.")?;
            write_dump_path(output, &path)?;
            path
        }
    };

    let report = read_report(&path)
        .with_error_context(|| format!("Failed to read DogStatsD context report from '{}'.", path.display()))?;
    let rendered = report.render(cmd.num_metrics, cmd.num_tags);
    output
        .write_all(rendered.as_bytes())
        .with_error_context(|| format!("Failed to write DogStatsD context report for '{}'.", path.display()))?;
    output
        .flush()
        .with_error_context(|| format!("Failed to flush DogStatsD context report for '{}'.", path.display()))?;
    Ok(())
}

pub(super) async fn handle_dogstatsd_dump_contexts(
    requester: &mut dyn DogStatsDContextDumpRequester, output: &mut dyn Write,
) -> Result<(), GenericError> {
    let path = requester
        .request_context_dump()
        .await
        .error_context("Failed to request a DogStatsD context dump.")?;
    write_dump_path(output, &path)
}

fn write_dump_path(output: &mut dyn Write, path: &Path) -> Result<(), GenericError> {
    writeln!(output, "Wrote {}", path.display())
        .with_error_context(|| format!("Failed to write the DogStatsD context dump path '{}'.", path.display()))?;
    output
        .flush()
        .with_error_context(|| format!("Failed to flush the DogStatsD context dump path '{}'.", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::path::{Path, PathBuf};

    use argh::FromArgs as _;
    use async_trait::async_trait;
    use saluki_error::{generic_error, GenericError};

    use super::{
        handle_dogstatsd_dump_contexts, handle_dogstatsd_top, DogStatsDContextDumpRequester, TopCommand,
        ValidatedTopCommand,
    };
    use crate::cli::dogstatsd::{DogstatsdCommand, DogstatsdSubcommand};

    const PLAIN_FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/dogstatsd_contexts_agent.ndjson"
    );
    const GOLDEN_REPORT: &str = concat!(
        "   Contexts\tMetric name\t(number of unique values for each tag)\n",
        "          3\ta.metric\t(2 env, 1 service)\n",
        "          1\tz.metric\t(1 bare, 1 env, 1 image)\n",
    );

    #[test]
    fn dogstatsd_top_parses_defaults() {
        let command = parse_dogstatsd(&["top"]).expect("top defaults should parse");
        let DogstatsdSubcommand::Top(top) = command.subcommand else {
            panic!("expected top subcommand");
        };

        assert_eq!(top.path, None);
        assert_eq!(top.num_metrics, 10);
        assert_eq!(top.num_tags, None);
        assert_eq!(top.legacy_num_tags, None);
        let validated = top.validate().expect("defaults should pass preflight");
        assert_eq!(validated.path, None);
        assert_eq!(validated.num_metrics, 10);
        assert_eq!(validated.num_tags, 5);
    }

    #[test]
    fn dogstatsd_top_parses_all_short_flags() {
        let command =
            parse_dogstatsd(&["top", "-p", PLAIN_FIXTURE, "-m", "7", "-t", "3"]).expect("top short flags should parse");
        let DogstatsdSubcommand::Top(top) = command.subcommand else {
            panic!("expected top subcommand");
        };

        assert_eq!(top.path, Some(PathBuf::from(PLAIN_FIXTURE)));
        assert_eq!(top.num_metrics, 7);
        assert_eq!(top.num_tags, Some(3));
        assert_eq!(top.legacy_num_tags, None);
        let validated = top.validate().expect("short options should pass preflight");
        assert_eq!(validated.path, Some(PathBuf::from(PLAIN_FIXTURE)));
        assert_eq!(validated.num_metrics, 7);
        assert_eq!(validated.num_tags, 3);
    }

    #[test]
    fn dogstatsd_top_parses_corrected_long_num_tags() {
        let command = parse_dogstatsd(&["top", "--num-tags", "4"]).expect("corrected option should parse");
        let DogstatsdSubcommand::Top(top) = command.subcommand else {
            panic!("expected top subcommand");
        };

        assert_eq!(top.num_tags, Some(4));
        assert_eq!(top.legacy_num_tags, None);
        assert_eq!(top.validate().unwrap().num_tags, 4);
    }

    #[test]
    fn dogstatsd_top_parses_legacy_mum_tags() {
        let command = parse_dogstatsd(&["top", "--mum-tags", "6"]).expect("legacy option should parse");
        let DogstatsdSubcommand::Top(top) = command.subcommand else {
            panic!("expected top subcommand");
        };

        assert_eq!(top.num_tags, None);
        assert_eq!(top.legacy_num_tags, Some(6));
        assert_eq!(top.validate().unwrap().num_tags, 6);
    }

    #[test]
    fn dogstatsd_top_preflight_rejects_both_num_tags_spellings() {
        let command = parse_dogstatsd(&["top", "--num-tags", "4", "--mum-tags", "6"])
            .expect("each spelling should parse before conflict validation");
        let DogstatsdSubcommand::Top(top) = command.subcommand else {
            panic!("expected top subcommand");
        };

        let error = top.validate().expect_err("preflight should reject conflicting options");
        assert_eq!(
            error.to_string(),
            "Cannot use `--num-tags` and legacy `--mum-tags` together; use `--num-tags`."
        );
    }

    #[test]
    fn dogstatsd_top_rejects_negative_limits_and_extra_arguments() {
        for args in [
            &["top", "--num-metrics", "-1"][..],
            &["top", "--num-tags", "-1"][..],
            &["top", "--mum-tags", "-1"][..],
            &["top", "artifact.ndjson"][..],
        ] {
            assert!(parse_dogstatsd(args).is_err(), "arguments should be rejected: {args:?}");
        }
    }

    #[test]
    fn dogstatsd_dump_contexts_parses_without_arguments_and_rejects_extras() {
        let command = parse_dogstatsd(&["dump-contexts"]).expect("dump-contexts should parse");
        assert!(matches!(command.subcommand, DogstatsdSubcommand::DumpContexts(_)));

        let error = parse_dogstatsd(&["dump-contexts", "extra"]).expect_err("extra argument should fail");
        assert!(
            error.output.contains("Unrecognized argument: extra"),
            "{}",
            error.output
        );
    }

    #[tokio::test]
    async fn dogstatsd_top_reads_an_offline_fixture_without_triggering_a_dump() {
        let command = top_command(Some(PathBuf::from(PLAIN_FIXTURE)), 10, None, None);
        let mut requester = FakeRequester::returning_path("unused");
        let mut output = RecordingWriter::default();

        handle_dogstatsd_top(Some(&mut requester), command, &mut output)
            .await
            .expect("offline top should succeed");

        assert_eq!(requester.calls, 0);
        assert_eq!(output.text(), GOLDEN_REPORT);
    }

    #[tokio::test]
    async fn dogstatsd_top_triggers_one_online_dump_before_rendering_the_report() {
        let mut requester = FakeRequester::returning_path(PLAIN_FIXTURE);
        let mut output = RecordingWriter::default();

        handle_dogstatsd_top(Some(&mut requester), top_command(None, 10, None, None), &mut output)
            .await
            .expect("online top should succeed");

        assert_eq!(requester.calls, 1);
        assert_eq!(output.text(), format!("Wrote {PLAIN_FIXTURE}\n{GOLDEN_REPORT}"));
        assert_eq!(
            output.flushes.first().unwrap(),
            format!("Wrote {PLAIN_FIXTURE}\n").as_bytes()
        );
    }

    #[tokio::test]
    async fn dogstatsd_dump_contexts_triggers_once_and_prints_only_the_path() {
        let mut requester = FakeRequester::returning_path(PLAIN_FIXTURE);
        let mut output = RecordingWriter::default();

        handle_dogstatsd_dump_contexts(&mut requester, &mut output)
            .await
            .expect("dump-contexts should succeed");

        assert_eq!(requester.calls, 1);
        assert_eq!(output.text(), format!("Wrote {PLAIN_FIXTURE}\n"));
        assert_eq!(output.flushes, vec![format!("Wrote {PLAIN_FIXTURE}\n").into_bytes()]);
    }

    #[tokio::test]
    async fn dogstatsd_dump_contexts_prints_nothing_when_transport_fails() {
        let mut requester = FakeRequester::returning_error("injected dump transport failure");
        let mut output = RecordingWriter::default();

        let error = handle_dogstatsd_dump_contexts(&mut requester, &mut output)
            .await
            .expect_err("transport failure should propagate");

        assert_eq!(requester.calls, 1);
        assert!(format!("{error:#}").contains("injected dump transport failure"));
        assert_eq!(output.text(), "");
        assert!(output.flushes.is_empty());
    }

    #[tokio::test]
    async fn dogstatsd_top_keeps_the_flushed_path_when_online_artifact_parsing_fails() {
        let artifact = tempfile::NamedTempFile::new().expect("temporary artifact should be created");
        std::fs::write(artifact.path(), b"not-json").expect("corrupt artifact should be written");
        let mut requester = FakeRequester::returning_path(artifact.path());
        let mut output = RecordingWriter::default();

        let error = handle_dogstatsd_top(Some(&mut requester), top_command(None, 10, None, None), &mut output)
            .await
            .expect_err("corrupt artifact should fail");

        let wrote_line = format!("Wrote {}\n", artifact.path().display());
        assert_eq!(requester.calls, 1);
        assert_eq!(output.text(), wrote_line);
        assert_eq!(output.flushes, vec![wrote_line.into_bytes()]);
        let error_chain = format!("{error:#}");
        assert!(error_chain.contains("DogStatsD context report"), "{error_chain}");
        assert!(error_chain.contains("decode record 1"), "{error_chain}");
    }

    #[tokio::test]
    async fn dogstatsd_top_redacts_malformed_artifact_values_from_errors() {
        const SENTINEL: &str = "SECRET_TENANT_TAG";

        let artifact = tempfile::NamedTempFile::new().expect("temporary artifact should be created");
        let record = format!(
            "{{\"Name\":\"metric\",\"Host\":\"host\",\"Type\":\"Gauge\",\"TaggerTags\":[],\"MetricTags\":[],\"NoIndex\":\"{SENTINEL}\",\"Source\":1}}\n"
        );
        std::fs::write(artifact.path(), record).expect("malformed artifact should be written");
        let mut output = RecordingWriter::default();

        let error = handle_dogstatsd_top(
            None,
            top_command(Some(artifact.path().to_owned()), 10, None, None),
            &mut output,
        )
        .await
        .expect_err("wrong-typed artifact field should fail");

        let error_chain = format!("{error:#}");
        assert!(!error_chain.contains(SENTINEL), "{error_chain}");
        assert!(
            error_chain.contains(&artifact.path().display().to_string()),
            "{error_chain}"
        );
        assert!(error_chain.contains("decode record 1"), "{error_chain}");
        assert!(error_chain.contains("line 1"), "{error_chain}");
        assert!(error_chain.contains("column"), "{error_chain}");
    }

    #[tokio::test]
    async fn dogstatsd_top_renders_only_the_heading_for_an_empty_artifact() {
        let artifact = tempfile::NamedTempFile::new().expect("temporary artifact should be created");
        let mut output = RecordingWriter::default();

        handle_dogstatsd_top(
            None,
            top_command(Some(artifact.path().to_owned()), 10, None, None),
            &mut output,
        )
        .await
        .expect("empty artifact should render");

        assert_eq!(
            output.text(),
            "   Contexts\tMetric name\t(number of unique values for each tag)\n"
        );
    }

    #[tokio::test]
    async fn dogstatsd_top_applies_custom_and_zero_report_limits() {
        let cases = [
            (
                top_command(Some(PathBuf::from(PLAIN_FIXTURE)), 0, Some(5), None),
                concat!(
                    "   Contexts\tMetric name\t(number of unique values for each tag)\n",
                    "          4\t(other 2 metrics)\n",
                ),
            ),
            (
                top_command(Some(PathBuf::from(PLAIN_FIXTURE)), 10, Some(0), None),
                concat!(
                    "   Contexts\tMetric name\t(number of unique values for each tag)\n",
                    "          3\ta.metric\t(3 values in 2 other tags)\n",
                    "          1\tz.metric\t(3 values in 3 other tags)\n",
                ),
            ),
            (
                top_command(Some(PathBuf::from(PLAIN_FIXTURE)), 1, Some(1), None),
                concat!(
                    "   Contexts\tMetric name\t(number of unique values for each tag)\n",
                    "          3\ta.metric\t(2 env, 1 service)\n",
                    "          1\tz.metric\t(1 bare, 2 values in 2 other tags)\n",
                ),
            ),
        ];

        for (command, expected) in cases {
            let mut output = RecordingWriter::default();
            handle_dogstatsd_top(None, command, &mut output)
                .await
                .expect("offline report should render");
            assert_eq!(output.text(), expected);
        }
    }

    fn parse_dogstatsd(args: &[&str]) -> Result<DogstatsdCommand, argh::EarlyExit> {
        DogstatsdCommand::from_args(&["agent-data-plane", "dogstatsd"], args)
    }

    fn top_command(
        path: Option<PathBuf>, num_metrics: usize, num_tags: Option<usize>, legacy_num_tags: Option<usize>,
    ) -> ValidatedTopCommand {
        TopCommand {
            path,
            num_metrics,
            num_tags,
            legacy_num_tags,
        }
        .validate()
        .expect("test command should pass preflight")
    }

    struct FakeRequester {
        calls: usize,
        response: Option<Result<PathBuf, GenericError>>,
    }

    impl FakeRequester {
        fn returning_path(path: impl AsRef<Path>) -> Self {
            Self {
                calls: 0,
                response: Some(Ok(path.as_ref().to_owned())),
            }
        }

        fn returning_error(message: &'static str) -> Self {
            Self {
                calls: 0,
                response: Some(Err(generic_error!(message))),
            }
        }
    }

    #[async_trait(?Send)]
    impl DogStatsDContextDumpRequester for FakeRequester {
        async fn request_context_dump(&mut self) -> Result<PathBuf, GenericError> {
            self.calls += 1;
            self.response.take().expect("fake requester called more than once")
        }
    }

    #[derive(Default)]
    struct RecordingWriter {
        bytes: Vec<u8>,
        flushes: Vec<Vec<u8>>,
    }

    impl RecordingWriter {
        fn text(&self) -> String {
            String::from_utf8(self.bytes.clone()).expect("test output should be UTF-8")
        }
    }

    impl io::Write for RecordingWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes.push(self.bytes.clone());
            Ok(())
        }
    }
}
