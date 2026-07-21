use std::path::Path;

use self::artifact::ArtifactError;
#[allow(unused_imports, reason = "wired into the DogStatsD API by a subsequent feature task")]
pub(crate) use self::artifact::{publish_context_dump, CONTEXT_DUMP_FILENAME};
use self::report::ContextReport;

mod artifact;
mod report;

fn read_report(path: &Path) -> Result<ContextReport, ArtifactError> {
    let mut report = ContextReport::new();
    artifact::for_each_record(path, |record| report.ingest(record))?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::read_report;

    const HEADING: &str = "   Contexts\tMetric name\t(number of unique values for each tag)\n";
    const PLAIN_FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/dogstatsd_contexts_agent.ndjson"
    );

    #[test]
    fn renders_agent_fixture_as_a_golden_report() {
        let report = read_report(Path::new(PLAIN_FIXTURE)).expect("fixture should decode");

        assert_eq!(
            report.render(usize::MAX, usize::MAX),
            concat!(
                "   Contexts\tMetric name\t(number of unique values for each tag)\n",
                "          3\ta.metric\t(2 env, 1 service)\n",
                "          1\tz.metric\t(1 bare, 1 env, 1 image)\n",
            )
        );
    }

    #[test]
    fn empty_artifact_renders_only_the_heading() {
        let artifact = tempfile::NamedTempFile::new().expect("temporary artifact should be created");
        let report = read_report(artifact.path()).expect("empty artifact should decode");

        assert_eq!(report.render(10, 10), HEADING);
    }
}
