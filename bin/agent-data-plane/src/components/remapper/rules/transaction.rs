use crate::components::remapper::RemapperRule;

pub fn get_transaction_remappings() -> Vec<RemapperRule> {
    vec![
        // Transaction metrics.
        RemapperRule::by_name(
            "datadog.agent.adp.network_http_requests_failed_total",
            "transactions.dropped",
        )
        .with_original_tags(["domain", "endpoint"]),
        RemapperRule::by_name(
            "datadog.agent.adp.network_http_requests_success_total",
            "transactions.success",
        )
        .with_original_tags(["domain", "endpoint"]),
        RemapperRule::by_name(
            "datadog.agent.adp.network_http_requests_success_sent_bytes_total",
            "transactions.success_bytes",
        )
        .with_original_tags(["domain", "endpoint"]),
        RemapperRule::by_name_and_tags(
            "datadog.agent.adp.network_http_requests_errors_total",
            &["error_type:client_error"],
            "transactions.http_errors",
        )
        .with_original_tags(["domain", "endpoint", "code"]),
        RemapperRule::by_name(
            "datadog.agent.adp.network_http_requests_errors_total",
            "transactions.errors",
        )
        .with_original_tags(["domain", "endpoint", "error_type"]),
    ]
}
