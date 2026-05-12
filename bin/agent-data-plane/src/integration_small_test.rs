use std::{path::PathBuf, sync::LazyLock};

use agent_data_plane::fuzz::{
    DogStatsDInput, PROFILE, aggregate_metric_lines, metric_line_from_raw, inner, saluki_path
};
use barkus_core::ir::GrammarIr;
use bytes::Bytes;
use rand::{rngs::SmallRng, SeedableRng};
use saluki_error::GenericError;
use tracing::{info, level_filters::LevelFilter};
use tracing_subscriber::EnvFilter;

static GRAMMAR_SINGLE: LazyLock<GrammarIr> = LazyLock::new(|| {
    let grammar_path: PathBuf = saluki_path().join("fuzz/dogstatsd_offset.ebnf");
    let grammar = std::fs::read_to_string(grammar_path).expect("Grammar file not found");
    barkus_ebnf::compile(&grammar).expect("failed to compile dogstatsd single-line EBNF grammar")
});

/// Anchors the simulated clock. Must be called once from within the tokio runtime.
static TOKIO_EPOCH: OnceLock<tokio::time::Instant> = OnceLock::new();

/// Displays wall-clock time followed by `[sim+Xs]` when called from inside
/// a tokio runtime that has a time driver (real or paused).
struct WallAndSim;

impl tracing_subscriber::fmt::time::FormatTime for WallAndSim {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> fmt::Result {
        write!(w, "{}", chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ"))?;
        if let (Some(epoch), Ok(_)) = (TOKIO_EPOCH.get(), tokio::runtime::Handle::try_current()) {
            let sim = tokio::time::Instant::now().duration_since(*epoch);
            write!(w, "[sim+{:.3}s]", sim.as_secs_f64())?;
        }
        Ok(())
    }
}

fn generate_corpus_random() -> Result<DogStatsDInput, GenericError> {
    let seed = Some(1234u64);
    let count = 100;
    let mut rng: SmallRng = match seed {
        Some(s) => SmallRng::seed_from_u64(s),
        None => SmallRng::from_rng(&mut rand::rng()),
    };
    let generated_packets: Vec<(u64, Bytes)> = (0..count)
        .map(|_| {
            let (ast, _tape, _map) = barkus_core::generate::generate(&GRAMMAR_SINGLE, &PROFILE, &mut rng)
                .map_err(|e| GenericError::msg("Barkus - failed to generate").context(e))?;
            let ast_bytes = ast.serialize();
            metric_line_from_raw(&ast_bytes)
        })
        .collect::<Result<_, GenericError>>()?;
    Ok(DogStatsDInput { messages: aggregate_metric_lines(generated_packets) })
}


fn main() {
    tracing_subscriber::fmt()
        .compact()
        .with_timer(WallAndSim)
        .with_env_filter(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .with_ansi(true)
        .with_target(true)
        .init();
    info!("hello!");
    let corpus = generate_corpus_random().expect("failed to generate random corpus");
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime")
        .block_on(async {
            TOKIO_EPOCH.get_or_init(tokio::time::Instant::now);
            inner(corpus).await;
        });
    info!("goodbye!");
}
