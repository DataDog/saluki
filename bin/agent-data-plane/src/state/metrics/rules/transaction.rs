use super::RemapperRule;

pub fn get_transaction_remappings() -> Vec<RemapperRule> {
    vec![
        // Transaction metrics.
        RemapperRule::by_name(
            "adp.network_http_requests_input_bytes_total",
            "transactions.input_bytes",
        )
        .with_original_tags(["domain", "endpoint"])
        .with_help_text("Incoming transaction sizes in bytes"),
        RemapperRule::by_name("adp.network_http_requests_input_total", "transactions.input_count")
            .with_original_tags(["domain", "endpoint"])
            .with_help_text("Incoming transaction count"),
        RemapperRule::by_name("adp.network_http_requests_failed_total", "transactions.dropped")
            .with_original_tags(["domain", "endpoint"])
            .with_help_text("Transaction drop count"),
        RemapperRule::by_name("adp.network_http_requests_success_total", "transactions.success")
            .with_original_tags(["domain", "endpoint"])
            .with_help_text("Successful transaction count"),
        RemapperRule::by_name(
            "adp.network_http_requests_success_sent_bytes_total",
            "transactions.success_bytes",
        )
        .with_original_tags(["domain", "endpoint"])
        .with_help_text("Successful transaction sizes in bytes"),
        RemapperRule::by_name("adp.component_data_points_sent_total", "point.sent").with_original_tags(["domain"]),
        RemapperRule::by_name("adp.component_data_points_dropped_total", "point.dropped")
            .with_original_tags(["domain"]),
        RemapperRule::by_name_and_tags(
            "adp.network_http_requests_errors_total",
            &["error_type:client_error"],
            "transactions.http_errors",
        )
        .with_original_tags(["domain", "endpoint", "code"])
        .with_help_text("Count of transactions http errors per http code"),
        RemapperRule::by_name("adp.network_http_requests_errors_total", "transactions.errors")
            .with_original_tags(["domain", "endpoint", "error_type"])
            .with_help_text("Count of transactions errored grouped by type of error"),
    ]
}
