#!/usr/bin/env python3
"""
Send OTLP logs to ADP via gRPC (port 4317).

This script uses the OpenTelemetry Python SDK to build and export log records
to ADP's OTLP gRPC endpoint. ADP translates them and forwards to Datadog.

Usage:
    pip install -r requirements.txt
    python send_logs_grpc.py

ADP must be running with OTLP enabled (see README.md).
"""

import logging
import time

from opentelemetry._logs import set_logger_provider
from opentelemetry.exporter.otlp.proto.grpc._log_exporter import OTLPLogExporter
from opentelemetry.sdk._logs import LoggerProvider, LoggingHandler
from opentelemetry.sdk._logs.export import BatchLogRecordProcessor
from opentelemetry.sdk.resources import Resource

# ---------------------------------------------------------------------------
# 1. Describe the service emitting these logs.
#    ADP maps resource attributes as follows:
#      service.name  → Log.service  (top-level "service" field in Datadog)
#      host.name     → Log.hostname (top-level "hostname" field in Datadog)
#    All other resource attributes become additional_properties / ddtags.
# ---------------------------------------------------------------------------
resource = Resource.create(
    {
        "service.name": "adp-example-service",
        "service.version": "1.0.0",
        "host.name": "my-dev-host",
        "deployment.environment": "dev",
    }
)

# ---------------------------------------------------------------------------
# 2. Configure the OTLP gRPC exporter pointing at ADP's default gRPC port.
#    insecure=True because ADP listens on plain TCP in standalone mode.
# ---------------------------------------------------------------------------
exporter = OTLPLogExporter(
    endpoint="http://localhost:4317",
    insecure=True,
)

# ---------------------------------------------------------------------------
# 3. Wire up the SDK: LoggerProvider → BatchProcessor → Exporter.
#    BatchLogRecordProcessor buffers records and exports in the background.
# ---------------------------------------------------------------------------
logger_provider = LoggerProvider(resource=resource)
logger_provider.add_log_record_processor(BatchLogRecordProcessor(exporter))
set_logger_provider(logger_provider)

# ---------------------------------------------------------------------------
# 4. Attach the OpenTelemetry handler to a standard Python logger.
#    Python log levels map to OTLP severity numbers:
#      DEBUG=5, INFO=9, WARNING=13, ERROR=17, CRITICAL=21
#    ADP maps these to Datadog statuses:
#      5→Debug, 9→Info, 13→Warning, 17→Error, 21→Fatal
# ---------------------------------------------------------------------------
handler = LoggingHandler(level=logging.DEBUG, logger_provider=logger_provider)

logger = logging.getLogger("adp.example")
logger.setLevel(logging.DEBUG)
logger.addHandler(handler)

# ---------------------------------------------------------------------------
# 5. Emit example log records.
#    Attributes on individual log records become additional_properties in ADP.
#    The "ddtags" attribute (if present) is parsed as comma-separated k:v tags
#    and merged into the top-level "ddtags" field in Datadog.
# ---------------------------------------------------------------------------
print("Sending log records to ADP (gRPC localhost:4317)...")

logger.info(
    "user login succeeded",
    extra={
        "otelSpanID": "0" * 16,
        "otelTraceID": "0" * 32,
        # These become additional_properties in ADP:
        "user.id": "u-42",
        "http.method": "POST",
        "http.route": "/api/v1/login",
        # "ddtags" is special: ADP merges these into the ddtags field.
        "ddtags": "team:backend,feature:auth",
    },
)

logger.warning(
    "rate limit approaching threshold",
    extra={
        "limit": 1000,
        "current": 950,
        "window_seconds": 60,
        "ddtags": "team:platform,alert:rate-limit",
    },
)

logger.error(
    "database connection failed",
    extra={
        "db.system": "postgresql",
        "db.name": "users",
        "db.host": "db.internal",
        "error.type": "ConnectionRefused",
        "ddtags": "team:backend,tier:critical",
    },
)

# ---------------------------------------------------------------------------
# 6. Flush and shut down.
#    shutdown() forces a final export before the process exits.
# ---------------------------------------------------------------------------
logger_provider.shutdown()

print("Done. Check Datadog Logs (service:adp-example-service) within ~15 seconds.")
