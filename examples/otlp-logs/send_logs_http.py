#!/usr/bin/env python3
"""
Send OTLP logs to ADP via HTTP/protobuf (port 4318).

Identical log payload to send_logs_grpc.py, but uses the OTLP HTTP exporter.
Useful when gRPC is blocked by a firewall or when debugging with a proxy.

Usage:
    pip install -r requirements.txt
    python send_logs_http.py

ADP must be running with OTLP enabled (see README.md).
"""

import logging

from opentelemetry._logs import set_logger_provider
from opentelemetry.exporter.otlp.proto.http._log_exporter import OTLPLogExporter
from opentelemetry.sdk._logs import LoggerProvider, LoggingHandler
from opentelemetry.sdk._logs.export import BatchLogRecordProcessor
from opentelemetry.sdk.resources import Resource

# ---------------------------------------------------------------------------
# Resource attributes — same semantics as the gRPC example.
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
# OTLP HTTP exporter.
#   endpoint: ADP's HTTP port (4318). The SDK appends /v1/logs automatically.
#   The body is binary protobuf (Content-Type: application/x-protobuf).
# ---------------------------------------------------------------------------
exporter = OTLPLogExporter(
    endpoint="http://localhost:4318",
)

logger_provider = LoggerProvider(resource=resource)
logger_provider.add_log_record_processor(BatchLogRecordProcessor(exporter))
set_logger_provider(logger_provider)

handler = LoggingHandler(level=logging.DEBUG, logger_provider=logger_provider)
logger = logging.getLogger("adp.example.http")
logger.setLevel(logging.DEBUG)
logger.addHandler(handler)

print("Sending log records to ADP (HTTP localhost:4318)...")

logger.info(
    "order placed successfully",
    extra={
        "order.id": "ord-99812",
        "order.total_usd": 149.99,
        "customer.tier": "premium",
        "ddtags": "team:commerce,region:us-east-1",
    },
)

logger.error(
    "payment gateway timeout",
    extra={
        "gateway": "stripe",
        "timeout_ms": 5000,
        "order.id": "ord-99813",
        "ddtags": "team:commerce,tier:critical",
    },
)

logger.debug(
    "cache miss for product catalog",
    extra={
        "cache.key": "catalog:v3:electronics",
        "cache.backend": "redis",
        "ddtags": "team:platform",
    },
)

logger_provider.shutdown()

print("Done. Check Datadog Logs (service:adp-example-service) within ~15 seconds.")
