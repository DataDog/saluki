# Dreamweaver

Dreamweaver is a distributed system OTLP workload generator. It generates realistic, correlated telemetry (traces,
metrics, and logs) by simulating a user-defined distributed system architecture.

Users describe their imaginary system in YAML—defining services, their types, how they connect to each other, and what
telemetry they emit—and Dreamweaver brings it to life by generating realistic OTLP telemetry and sending it to a
downstream receiver.

## Current Status

This tool is currently in development and should be considered beta quality: it generally does the right thing, but it
may have bugs, and the data it generates is still somewhat shallow.

## Usage

```bash
dreamweaver <config-file>
```

Example:
```bash
dreamweaver example-config.yaml
```

## Key Features

- **Correlated telemetry**: Traces, metrics, and logs share context (trace IDs, span IDs)
- **Realistic distributed traces**: Proper parent-child span relationships across service calls
- **Composable templates**: Define service types once, reuse across multiple service instances
- **Deterministic generation**: Seeded RNG ensures reproducible workloads
- **Configurable workload**: Control request rate and duration

## Configuration Schema

The configuration file is YAML with the following top-level fields:

```yaml
seed: String                             # Required: Seed string for deterministic RNG
templates: Map<String, ServiceTemplate>  # Required: Service type definitions
services: List<ServiceDefinition>        # Required: Service instances
workload: WorkloadConfig                 # Required: Rate and duration
output: OutputConfig                     # Required: OTLP endpoint
```

---

### `seed`

An arbitrary string used to seed the random number generator. The string is hashed using SHA3-256 to produce a 32-byte
seed. Using the same seed string produces identical output across runs.

```yaml
seed: "my-reproducible-workload-2024"
```

---

### `templates`

A map of template names to `ServiceTemplate` definitions. Templates define what telemetry a service type emits.

```yaml
templates:
  <template-name>:
    traces: TraceTemplate       # Optional
    metrics: List<MetricTemplate>  # Optional
    logs: List<LogTemplate>     # Optional
```

#### `TraceTemplate`

Defines trace span generation for a service type.

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `span_kind` | SpanKind | Yes | - | The kind of span this service generates |
| `operations` | List<String> | Yes | - | Operation names to randomly select from |
| `error_rate` | float | No | `0.0` | Probability of span having error status (0.0-1.0) |
| `base_duration_ms` | integer | No | `50` | Base duration in milliseconds (actual varies 50%-200%) |

**`span_kind` values:**

| Value | OTLP Kind | Description |
|-------|-----------|-------------|
| `server` | SPAN_KIND_SERVER | Server-side of a synchronous request (e.g., HTTP server) |
| `client` | SPAN_KIND_CLIENT | Client-side of a synchronous request (e.g., HTTP client, DB client) |
| `producer` | SPAN_KIND_PRODUCER | Producer of an asynchronous message (e.g., queue producer) |
| `consumer` | SPAN_KIND_CONSUMER | Consumer of an asynchronous message (e.g., queue consumer) |
| `internal` | SPAN_KIND_INTERNAL | Internal operation within an application |

Example:
```yaml
traces:
  span_kind: server
  operations:
    - "GET /api/users"
    - "POST /api/orders"
  error_rate: 0.01
  base_duration_ms: 50
```

#### `MetricTemplate`

Defines a metric to generate for this service type.

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `name` | String | Yes | - | The metric name |
| `type` | MetricType | Yes | - | The type of metric |
| `unit` | String | No | `""` | Unit of measurement (e.g., "ms", "bytes") |

**`type` values:**

| Value | Description | Behavior |
|-------|-------------|----------|
| `counter` | Monotonically increasing counter | Increments by 1 per request, cumulative temporality |
| `gauge` | Point-in-time value | Random value based on metric name heuristics |
| `histogram` | Distribution of values | Uses request duration for `*duration*` metrics |

Example:
```yaml
metrics:
  - name: http.server.request.duration
    type: histogram
    unit: ms
  - name: http.server.active_requests
    type: gauge
  - name: http.server.request.total
    type: counter
```

#### `LogTemplate`

Defines log patterns to generate for this service type.

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `patterns` | List<String> | Yes | - | Log message patterns (supports placeholders) |
| `severity` | LogSeverity | No | `info` | Log severity level |

**`severity` values:**

| Value | OTLP Severity Number | Description |
|-------|---------------------|-------------|
| `trace` | 1 | Trace-level debugging |
| `debug` | 5 | Debug information |
| `info` | 9 | Informational messages |
| `warn` | 13 | Warning conditions |
| `error` | 17 | Error conditions |
| `fatal` | 21 | Fatal/critical errors |

**Pattern placeholders:**

| Placeholder | Description | Example Output |
|-------------|-------------|----------------|
| `{ip}` | Random IPv4 address | `192.168.1.42` |
| `{duration}` | Random duration (1-500) | `127` |
| `{error}` | Random error message | `Connection timeout` |
| `{user_id}` | Random user ID (1000-9999) | `4521` |
| `{request_id}` | Random hex request ID | `a1b2c3d4e5f67890` |

Example:
```yaml
logs:
  - patterns:
      - "Request received from {ip}"
      - "Processing request {request_id}"
    severity: info
  - patterns:
      - "Request failed: {error}"
    severity: error
```

---

### `services`

A list of service instances in your architecture.

#### `ServiceDefinition`

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `name` | String | Yes | - | Unique service instance name |
| `type` | String | Yes | - | Template name to use (must exist in `templates`) |
| `downstream` | List<String> | No | `[]` | Names of services this service calls |

Example:
```yaml
services:
  - name: api-gateway
    type: http-server
    downstream:
      - user-service
      - order-service

  - name: user-service
    type: http-server
    downstream:
      - users-db

  - name: users-db
    type: database-client
```

**Constraints:**
- Service names must be unique
- All `type` references must exist in `templates`
- All `downstream` references must exist in `services`
- The service graph must be acyclic (no circular dependencies)
- At least one service must have no incoming edges (entry point)

---

### `workload`

Controls the rate and duration of request generation.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `rate` | RateSpec | Yes | How many requests to generate per time unit |
| `duration` | Duration | Yes | How long to run the simulation |

#### `RateSpec` format

```
<count>/<unit>
```

| Unit | Description |
|------|-------------|
| `s` | Per second |
| `m` | Per minute |
| `h` | Per hour |

Examples:
- `100/s` - 100 requests per second
- `6000/m` - 6000 requests per minute (100/s)
- `360000/h` - 360000 requests per hour (100/s)

#### `Duration` format

```
<number><unit>
```

| Unit | Aliases | Description |
|------|---------|-------------|
| `s` | `sec`, `secs` | Seconds |
| `m` | `min`, `mins` | Minutes |
| `h` | `hr`, `hrs`, `hour`, `hours` | Hours |
| `d` | `day`, `days` | Days |

Examples:
- `30s` - 30 seconds
- `5m` - 5 minutes
- `1h` - 1 hour
- `1d` - 1 day

Example:
```yaml
workload:
  rate: 100/s
  duration: 5m
```

---

### `output`

Configures where to send the generated OTLP telemetry.

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `endpoint` | String | Yes | - | OTLP gRPC endpoint URL |
| `batch_size` | integer | No | `100` | Number of requests to batch before sending |

#### `endpoint` format

Supported formats:
- `grpc://host:port` - gRPC endpoint (converted to `http://host:port`)
- `http://host:port` - HTTP/2 gRPC endpoint
- `https://host:port` - HTTPS gRPC endpoint
- `host:port` - Assumed to be `http://host:port`

Example:
```yaml
output:
  endpoint: grpc://localhost:4317
  batch_size: 50
```

---

## Complete Example

```yaml
seed: "ecommerce-load-test-v1"

templates:
  http-server:
    traces:
      span_kind: server
      operations: ["GET /api/users", "POST /api/orders"]
      error_rate: 0.01
      base_duration_ms: 50
    metrics:
      - name: http.server.request.duration
        type: histogram
        unit: ms
      - name: http.server.request.total
        type: counter
    logs:
      - patterns: ["Request from {ip}", "Completed in {duration}ms"]
        severity: info

  database-client:
    traces:
      span_kind: client
      operations: ["SELECT", "INSERT", "UPDATE"]
      error_rate: 0.005
      base_duration_ms: 25
    metrics:
      - name: db.client.query.duration
        type: histogram
        unit: ms

services:
  - name: api-gateway
    type: http-server
    downstream: [user-service]

  - name: user-service
    type: http-server
    downstream: [users-db]

  - name: users-db
    type: database-client

workload:
  rate: 10/s
  duration: 1m

output:
  endpoint: grpc://localhost:4317
  batch_size: 50
```

## How It Works

1. **Configuration Loading**: Parses and validates the YAML configuration
2. **Architecture Resolution**: Builds the service graph and identifies entry points
3. **Simulation Loop**: At the configured rate, generates requests starting at random entry points
4. **Trace Propagation**: As requests flow through services, trace context (trace_id, span_id) propagates correctly
5. **Telemetry Generation**: Each service generates spans, metrics, and logs based on its template
6. **Batching & Sending**: Telemetry is batched and sent via gRPC to the OTLP endpoint
