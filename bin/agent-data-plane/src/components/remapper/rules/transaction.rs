use crate::components::remapper::RemapperRule;

pub fn get_transaction_remappings() -> Vec<RemapperRule> {
    vec![
        // Transaction metrics.
        RemapperRule::by_name(
            "datadog.agent.adp.network_http_requests_failed_total",
            "datadog.agent.transactions.dropped",
        )
        .with_original_tags(["domain", "endpoint"]),
        RemapperRule::by_name(
            "datadog.agent.adp.network_http_requests_success_total",
            "datadog.agent.transactions.success",
        )
        .with_original_tags(["domain", "endpoint"]),
        RemapperRule::by_name(
            "datadog.agent.adp.network_http_requests_success_sent_bytes_total",
            "datadog.agent.transactions.success_bytes",
        )
        .with_original_tags(["domain", "endpoint"]),
        RemapperRule::by_name_and_tags(
            "datadog.agent.adp.network_http_requests_errors_total",
            &["error_type:client_error"],
            "datadog.agent.transactions.http_errors",
        )
        .with_original_tags(["domain", "endpoint", "code"])
        .with_remapped_tags([("error_type", "status_code")]),
        RemapperRule::by_name(
            "datadog.agent.adp.network_http_requests_errors_total",
            "datadog.agent.transactions.errors",
        )
        .with_original_tags(["domain", "endpoint", "error_type"]),
    ]
}
