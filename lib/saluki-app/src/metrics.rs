pub async fn initialize_metrics(
    metrics_prefix: impl Into<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // We forward to the implementation in `saluki_core` so that we can have this crate be the collection point of all
    // helpers/types that are specific to generic application setup/initialization. The implementation itself has to
    // live in `saluki_core`, however, to have access to all of the underlying types that are created and used to
    // install the global recorder, such that they need not be exposed publicly.
    saluki_core::observability::metrics::initialize_metrics(metrics_prefix.into()).await
}
