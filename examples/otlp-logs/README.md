# OTLP Logs — End-to-End Example

This directory shows how to send logs to ADP over OTLP, which ADP will translate and forward to the Datadog Logs intake (`POST /api/v2/logs`).

## Prerequisites

| Requirement | Install |
|-------------|---------|
| A built ADP binary | `make build-adp` |
| A Datadog API key | https://app.datadoghq.com/organization-settings/api-keys |
| Python 3.9+ (for Python example) | https://python.org |
| `grpcurl` (for gRPC shell example) | `brew install grpcurl` |

---

## Step 1 — Run ADP in standalone mode with OTLP enabled

ADP needs a minimal config file and a set of environment variables.

```bash
# Create a minimal (empty) config file
echo "{}" > /tmp/adp-empty-config.yaml

# Run ADP — replace <YOUR_API_KEY> with a real Datadog API key
DD_DATA_PLANE_ENABLED=true \
DD_DATA_PLANE_STANDALONE_MODE=true \
DD_DATA_PLANE_OTLP_ENABLED=true \
DD_API_KEY=<YOUR_API_KEY> \
DD_HOSTNAME=my-dev-host \
./target/devel/agent-data-plane --config /tmp/adp-empty-config.yaml run
```

ADP will now listen on:

| Protocol | Default address |
|----------|----------------|
| OTLP gRPC | `0.0.0.0:4317` |
| OTLP HTTP | `0.0.0.0:4318` |

You should see log output like:

```
INFO  agent_data_plane: Agent Data Plane starting...
INFO  agent_data_plane: Topology running. Waiting for interrupt...
INFO  agent_data_plane: Topology healthy.
```

---

## Step 2 — Send logs

Pick whichever sender fits your workflow.

### Option A: Python (OpenTelemetry SDK) — gRPC

Sends three log records with different severity levels over gRPC.

```bash
cd examples/otlp-logs
pip install -r requirements.txt
python send_logs_grpc.py
```

### Option B: Python (OpenTelemetry SDK) — HTTP

Same logs, sent over HTTP/protobuf to port 4318.

```bash
cd examples/otlp-logs
pip install -r requirements.txt
python send_logs_http.py
```

### Option C: grpcurl (shell, no Python needed)

Sends a single log record using `grpcurl` with JSON input.

```bash
bash send_logs_grpcurl.sh
```

---

## Step 3 — Verify in Datadog

Navigate to **Logs → Search** in the Datadog UI.
Filter by `service:adp-example-service` or `host:my-dev-host`.

You should see the log entries within ~15 seconds (ADP flushes every 100 ms; the intake pipeline introduces additional latency).

---

## What the log looks like after translation

ADP translates each OTLP `LogRecord` into a JSON object POSTed to `/api/v2/logs`:

```json
{
  "message": "{\"message\":\"user login succeeded\",\"service\":\"adp-example-service\"}",
  "status": "Info",
  "hostname": "my-dev-host",
  "service": "adp-example-service",
  "ddsource": "otlp_log_ingestion",
  "ddtags": "otel_source:datadog_agent,env:dev,version:1.0.0",
  "@timestamp": "2024-01-15T10:30:00.000Z",
  "user.id": "u-42",
  "http.method": "POST",
  "otel.severity_number": "9"
}
```

Key translation rules (see `docs/reference/architecture/log-pipeline.md` for full details):

- `severity_number` 9–12 → `status: Info`
- `service.name` resource attribute → `service` field
- `host.name` resource attribute → `hostname` field
- `ddtags` attribute → merged into `ddtags` field as comma-separated tags
- All other attributes → `additional_properties`, flattened with dot-notation
