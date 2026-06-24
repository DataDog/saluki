use super::RemapperRule;

pub fn get_serializer_remappings() -> Vec<RemapperRule> {
    vec![
        RemapperRule::by_name("adp.serializer.v3_column_size", "serializer.v3_column_size")
            .with_original_tags(["column", "compressed"])
            .with_help_text("Number of bytes occupied by each column"),
        RemapperRule::by_name("adp.serializer.v3_values_count", "serializer.v3_values_count")
            .with_original_tags(["type"])
            .with_help_text("Number of values encoded using a specific encoding type"),
        RemapperRule::by_name(
            "adp.serializer.v3_payload_split_reason",
            "serializer.v3_payload_split_reason",
        )
        .with_original_tags(["reason"])
        .with_help_text("Why payload was split"),
        RemapperRule::by_name("adp.serializer.v3_item_too_big", "sketch_series.sketch_too_big")
            .with_help_text("Number of payloads dropped because they were too big for the stream compressor"),
    ]
}
