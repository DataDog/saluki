use std::fs::File;
use std::path::{Path, PathBuf};

use argh::FromArgs;
use async_trait::async_trait;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio_util::sync::CancellationToken;

use crate::cli::{dogstatsd::open_regular_file, remote::CommandOutput, utils::DataPlaneAPIClient};
use crate::dogstatsd_contexts::read_report_from_file;

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
}

impl TopCommand {
    pub(super) fn validate(self) -> ValidatedTopCommand {
        ValidatedTopCommand {
            path: self.path,
            num_metrics: self.num_metrics,
            num_tags: self.num_tags.unwrap_or(5),
        }
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

#[async_trait]
pub(super) trait DogStatsDContextDumpRequester: Send {
    async fn request_context_dump(&mut self) -> Result<PathBuf, GenericError>;
}

#[async_trait]
impl DogStatsDContextDumpRequester for DataPlaneAPIClient {
    async fn request_context_dump(&mut self) -> Result<PathBuf, GenericError> {
        self.dogstatsd_contexts_dump().await
    }
}

pub(super) async fn handle_dogstatsd_top(
    requester: Option<&mut (dyn DogStatsDContextDumpRequester + Send)>, cmd: ValidatedTopCommand,
    output: &mut dyn CommandOutput, cancellation: &CancellationToken,
) -> Result<(), GenericError> {
    let path = match cmd.path {
        Some(path) => path,
        None => {
            let requester =
                requester.ok_or_else(|| generic_error!("Online DogStatsD top requires a context dump requester."))?;
            let path = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Ok(()),
                result = requester.request_context_dump() => result,
            }
            .error_context("Failed to request a DogStatsD context dump.")?;
            write_dump_path(output, &path).await?;
            path
        }
    };

    if cancellation.is_cancelled() {
        return Ok(());
    }

    let file = open_context_report_file(&path)?;
    let metric_limit = cmd.num_metrics;
    let tag_limit = cmd.num_tags;
    let task = tokio::task::spawn_blocking({
        let path = path.clone();
        move || render_report_from_file(&path, file, metric_limit, tag_limit)
    });
    let Some(rendered) =
        super::run_cancellable_blocking(cancellation, task, "DogStatsD context report rendering").await?
    else {
        return Ok(());
    };

    write_rendered_report(output, &path, rendered).await
}

fn open_context_report_file(path: &Path) -> Result<File, GenericError> {
    open_regular_file(
        path,
        || format!("Failed to open DogStatsD context report from '{}'.", path.display()),
        || format!("Failed to inspect DogStatsD context report from '{}'.", path.display()),
        || {
            generic_error!(
                "DogStatsD context report path '{}' is not a regular file.",
                path.display()
            )
        },
    )
}

fn render_report_from_file(
    path: &Path, file: File, metric_limit: usize, tag_limit: usize,
) -> Result<String, GenericError> {
    let report = read_report_from_file(path, file)
        .with_error_context(|| format!("Failed to read DogStatsD context report from '{}'.", path.display()))?;
    Ok(report.render(metric_limit, tag_limit))
}

async fn write_rendered_report(
    output: &mut dyn CommandOutput, path: &Path, rendered: String,
) -> Result<(), GenericError> {
    output
        .write_report(&rendered)
        .await
        .with_error_context(|| format!("Failed to write DogStatsD context report for '{}'.", path.display()))?;
    output
        .flush()
        .await
        .with_error_context(|| format!("Failed to flush DogStatsD context report for '{}'.", path.display()))?;
    Ok(())
}

pub(super) async fn handle_dogstatsd_dump_contexts(
    requester: &mut (dyn DogStatsDContextDumpRequester + Send), output: &mut dyn CommandOutput,
) -> Result<(), GenericError> {
    let path = requester
        .request_context_dump()
        .await
        .error_context("Failed to request a DogStatsD context dump.")?;
    write_dump_path(output, &path).await
}

async fn write_dump_path(output: &mut dyn CommandOutput, path: &Path) -> Result<(), GenericError> {
    output
        .write_report(&format!("Wrote {}\n", path.display()))
        .await
        .with_error_context(|| format!("Failed to write the DogStatsD context dump path '{}'.", path.display()))?;
    output
        .flush()
        .await
        .with_error_context(|| format!("Failed to flush the DogStatsD context dump path '{}'.", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use argh::FromArgs as _;
    use async_trait::async_trait;
    use saluki_error::{generic_error, GenericError};
    use tokio_util::sync::CancellationToken;

    use super::{
        handle_dogstatsd_dump_contexts, handle_dogstatsd_top, open_context_report_file, render_report_from_file,
        DogStatsDContextDumpRequester, TopCommand, ValidatedTopCommand,
    };
    use crate::cli::{
        dogstatsd::{DogstatsdCommand, DogstatsdSubcommand},
        remote::CommandOutput,
    };

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
        let validated = top.validate();
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
        let validated = top.validate();
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
        assert_eq!(top.validate().num_tags, 4);
    }

    #[test]
    fn dogstatsd_top_rejects_legacy_mum_tags() {
        assert!(parse_dogstatsd(&["top", "--mum-tags", "6"]).is_err());
    }

    #[test]
    fn dogstatsd_top_rejects_negative_limits_and_extra_arguments() {
        for args in [
            &["top", "--num-metrics", "-1"][..],
            &["top", "--num-tags", "-1"][..],
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
        let command = top_command(Some(PathBuf::from(PLAIN_FIXTURE)), 10, None);
        let mut requester = FakeRequester::returning_path("unused");
        let mut output = RecordingWriter::default();

        handle_dogstatsd_top(Some(&mut requester), command, &mut output, &CancellationToken::new())
            .await
            .expect("offline top should succeed");

        assert_eq!(requester.calls, 0);
        assert_eq!(output.text(), GOLDEN_REPORT);
    }

    #[test]
    fn offline_top_renders_from_the_descriptor_opened_before_path_changes() {
        let artifact = tempfile::NamedTempFile::new().expect("temporary artifact should be created");
        std::fs::write(
            artifact.path(),
            b"{\"Name\":\"original.metric\",\"Host\":\"\",\"Type\":\"Gauge\",\"TaggerTags\":[],\"MetricTags\":[],\"NoIndex\":false,\"Source\":1}\n",
        )
        .expect("original artifact should be written");
        let file = open_context_report_file(artifact.path()).expect("original artifact should open");
        let replacement = artifact.path().with_extension("replacement");
        std::fs::write(&replacement, b"not-json").expect("replacement artifact should be written");
        std::fs::rename(&replacement, artifact.path()).expect("artifact path should be replaced");

        let rendered = render_report_from_file(artifact.path(), file, 10, 5).expect("opened artifact should render");

        assert!(rendered.contains("original.metric"), "{rendered}");
    }

    #[tokio::test]
    async fn dogstatsd_top_offline_does_not_render_after_cancellation() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let mut output = RecordingWriter::default();

        handle_dogstatsd_top(
            None,
            top_command(Some(PathBuf::from("missing-context-dump.ndjson")), 10, None),
            &mut output,
            &cancellation,
        )
        .await
        .expect("cancelled offline top should not read or render the artifact");

        assert_eq!(output.text(), "");
        assert!(output.flushes.is_empty());
    }

    #[tokio::test]
    async fn dogstatsd_top_triggers_one_online_dump_before_rendering_the_report() {
        let mut requester = FakeRequester::returning_path(PLAIN_FIXTURE);
        let mut output = RecordingWriter::default();

        handle_dogstatsd_top(
            Some(&mut requester),
            top_command(None, 10, None),
            &mut output,
            &CancellationToken::new(),
        )
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

        let error = handle_dogstatsd_top(
            Some(&mut requester),
            top_command(None, 10, None),
            &mut output,
            &CancellationToken::new(),
        )
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
            top_command(Some(artifact.path().to_owned()), 10, None),
            &mut output,
            &CancellationToken::new(),
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
            top_command(Some(artifact.path().to_owned()), 10, None),
            &mut output,
            &CancellationToken::new(),
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
                top_command(Some(PathBuf::from(PLAIN_FIXTURE)), 0, Some(5)),
                concat!(
                    "   Contexts\tMetric name\t(number of unique values for each tag)\n",
                    "          4\t(other 2 metrics)\n",
                ),
            ),
            (
                top_command(Some(PathBuf::from(PLAIN_FIXTURE)), 10, Some(0)),
                concat!(
                    "   Contexts\tMetric name\t(number of unique values for each tag)\n",
                    "          3\ta.metric\t(3 values in 2 other tags)\n",
                    "          1\tz.metric\t(3 values in 3 other tags)\n",
                ),
            ),
            (
                top_command(Some(PathBuf::from(PLAIN_FIXTURE)), 1, Some(1)),
                concat!(
                    "   Contexts\tMetric name\t(number of unique values for each tag)\n",
                    "          3\ta.metric\t(2 env, 1 service)\n",
                    "          1\tz.metric\t(1 bare, 2 values in 2 other tags)\n",
                ),
            ),
        ];

        for (command, expected) in cases {
            let mut output = RecordingWriter::default();
            handle_dogstatsd_top(None, command, &mut output, &CancellationToken::new())
                .await
                .expect("offline report should render");
            assert_eq!(output.text(), expected);
        }
    }

    fn parse_dogstatsd(args: &[&str]) -> Result<DogstatsdCommand, argh::EarlyExit> {
        DogstatsdCommand::from_args(&["agent-data-plane", "dogstatsd"], args)
    }

    fn top_command(path: Option<PathBuf>, num_metrics: usize, num_tags: Option<usize>) -> ValidatedTopCommand {
        TopCommand {
            path,
            num_metrics,
            num_tags,
        }
        .validate()
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

    #[async_trait]
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

    #[async_trait]
    impl CommandOutput for RecordingWriter {
        async fn write_status(&mut self, _message: &str) -> std::io::Result<()> {
            Ok(())
        }

        async fn write_report(&mut self, output: &str) -> std::io::Result<()> {
            self.bytes.extend_from_slice(output.as_bytes());
            Ok(())
        }

        async fn flush(&mut self) -> std::io::Result<()> {
            self.flushes.push(self.bytes.clone());
            Ok(())
        }
    }
}
