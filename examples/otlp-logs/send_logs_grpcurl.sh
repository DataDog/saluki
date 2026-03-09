#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# Send a single OTLP log record to ADP via gRPC using grpcurl.
#
# Requirements:
#   brew install grpcurl          # macOS
#   apt-get install grpcurl       # Ubuntu (from GitHub releases otherwise)
#
# ADP must be running with OTLP enabled (see README.md).
# ---------------------------------------------------------------------------
set -euo pipefail

ADP_GRPC_ADDR="${ADP_GRPC_ADDR:-localhost:4317}"

echo "[*] Sending OTLP log record to ADP at ${ADP_GRPC_ADDR} ..."

# grpcurl sends the JSON payload to the OTLP LogsService/Export RPC.
# ADP decodes this using prost (binary protobuf), but grpcurl handles
# JSON → protobuf encoding automatically when given the proto descriptor.
#
# The JSON structure mirrors the OTLP protobuf schema:
#   ExportLogsServiceRequest
#     └── resource_logs[]
#           ├── resource { attributes[] }     ← resource-level metadata
#           └── scope_logs[]
#                 └── log_records[]           ← individual log events
#
# Attribute key/value semantics (as interpreted by ADP):
#   resource.attributes:
#     "service.name"  → Log.service
#     "host.name"     → Log.hostname
#     anything else   → additional_properties
#
#   log_records.attributes:
#     "ddtags"        → parsed and merged into ddtags field
#     "level"/"status"/"severity" → drives Log.status
#     anything else   → additional_properties (dot-flattened if nested)
#
#   severity_number:
#     1-4:Trace  5-8:Debug  9-12:Info  13-16:Warning  17-20:Error  21-24:Fatal
# ---------------------------------------------------------------------------

grpcurl \
  -plaintext \
  -d '{
    "resource_logs": [
      {
        "resource": {
          "attributes": [
            { "key": "service.name",              "value": { "string_value": "adp-example-service" } },
            { "key": "service.version",           "value": { "string_value": "1.0.0" } },
            { "key": "host.name",                 "value": { "string_value": "my-dev-host" } },
            { "key": "deployment.environment",    "value": { "string_value": "dev" } }
          ]
        },
        "scope_logs": [
          {
            "scope": {
              "name": "adp.example.grpcurl",
              "version": "1.0.0"
            },
            "log_records": [
              {
                "time_unix_nano": "1705312200000000000",
                "severity_number": 9,
                "severity_text": "INFO",
                "body": { "string_value": "grpcurl test log — payment processed" },
                "attributes": [
                  { "key": "payment.id",     "value": { "string_value": "pay-78341" } },
                  { "key": "payment.method", "value": { "string_value": "card" } },
                  { "key": "amount_usd",     "value": { "double_value": 59.99 } },
                  { "key": "ddtags",         "value": { "string_value": "team:payments,region:eu-west-1" } }
                ]
              },
              {
                "time_unix_nano": "1705312201000000000",
                "severity_number": 17,
                "severity_text": "ERROR",
                "body": { "string_value": "grpcurl test log — refund failed" },
                "attributes": [
                  { "key": "payment.id",  "value": { "string_value": "pay-78341" } },
                  { "key": "error.type",  "value": { "string_value": "InsufficientFunds" } },
                  { "key": "ddtags",      "value": { "string_value": "team:payments,tier:critical" } }
                ]
              }
            ]
          }
        ]
      }
    ]
  }' \
  "${ADP_GRPC_ADDR}" \
  opentelemetry.proto.collector.logs.v1.LogsService/Export

echo "[*] Done. Check Datadog Logs (service:adp-example-service) within ~15 seconds."
