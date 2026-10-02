// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package gen

// pinnedDepthReps keeps each shape on the same setting across schema updates. Generation rejects
// new classes and stale entries; review the affected settings before updating this table.
var pinnedDepthReps = map[depthRepKey]string{
	// yaml and set classes, keyed by default-layer Go type.
	{sourceYAML, "[]interface {}", ""}:      "dogstatsd_mapper_profiles",
	{sourceSet, "[]interface {}", ""}:       "dogstatsd_mapper_profiles",
	{sourceYAML, "[]map[string]string", ""}: "apm_config.replace_tags",
	{sourceSet, "[]map[string]string", ""}:  "apm_config.replace_tags",
	{sourceYAML, "[]string", ""}:            "apm_config.features",
	{sourceSet, "[]string", ""}:             "apm_config.features",
	{sourceYAML, "bool", ""}:                "allow_arbitrary_tags",
	{sourceSet, "bool", ""}:                 "allow_arbitrary_tags",
	{sourceYAML, "float64", ""}:             "apm_config.errors_per_second",
	{sourceSet, "float64", ""}:              "apm_config.errors_per_second",
	{sourceYAML, "int", ""}:                 "agent_ipc.grpc_max_message_size",
	{sourceSet, "int", ""}:                  "agent_ipc.grpc_max_message_size",
	{sourceYAML, "int64", ""}:               "cri_connection_timeout",
	{sourceSet, "int64", ""}:                "cri_connection_timeout",
	{sourceYAML, "map[string][]string", ""}: "additional_endpoints",
	{sourceSet, "map[string][]string", ""}:  "additional_endpoints",
	{sourceYAML, "map[string]string", ""}:   "use_v3_api.series.endpoints",
	{sourceSet, "map[string]string", ""}:    "use_v3_api.series.endpoints",
	{sourceYAML, "string", ""}:              "api_key",
	{sourceSet, "string", ""}:               "api_key",
	{sourceYAML, "time.Duration", ""}:       "expected_tags_duration",
	{sourceSet, "time.Duration", ""}:        "expected_tags_duration",
	// env classes, keyed by default-layer Go type and the schema env parser.
	{sourceEnv, "[]interface {}", ""}:                       "metric_tag_filterlist",
	{sourceEnv, "[]interface {}", "json"}:                   "dogstatsd_mapper_profiles",
	{sourceEnv, "[]map[string]string", "json"}:              "apm_config.replace_tags",
	{sourceEnv, "[]string", ""}:                             "apm_config.obfuscation.elasticsearch.keep_values",
	{sourceEnv, "[]string", "comma_then_space_separated"}:   "apm_config.features",
	{sourceEnv, "[]string", "json"}:                         "apm_config.peer_tags",
	{sourceEnv, "[]string", "json_list_or_space_separated"}: "apm_config.obfuscation.credit_cards.keep_values",
	{sourceEnv, "bool", ""}:                                 "allow_arbitrary_tags",
	{sourceEnv, "float64", ""}:                              "apm_config.errors_per_second",
	{sourceEnv, "int", ""}:                                  "agent_ipc.grpc_max_message_size",
	{sourceEnv, "int64", ""}:                                "cri_connection_timeout",
	{sourceEnv, "map[string][]string", ""}:                  "additional_endpoints",
	{sourceEnv, "map[string]string", ""}:                    "use_v3_api.series.endpoints",
	{sourceEnv, "string", ""}:                               "api_key",
	{sourceEnv, "time.Duration", ""}:                        "expected_tags_duration",
}

// depthRepKey identifies one depth class: the source whose variants split on it (yaml and set
// share their classes), the default-layer Go type, and the schema env parser for env classes.
type depthRepKey struct {
	Source    depthSource
	Type      string
	EnvParser string
}
