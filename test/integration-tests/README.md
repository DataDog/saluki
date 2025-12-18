# ADP Integration Tests

This directory contains integration tests for the Agent Data Plane (ADP). These tests run real ADP instances inside Docker containers and validate basic process health conditions such as stability, port binding, and log output.

## Overview

The integration tests are powered by **Panoramic**, a test runner that:

- Runs ADP inside Docker containers based on a "bundled" Datadog Agent/ADP image
- Validates assertions against each container
- Supports parallel test execution with configurable concurrency
- Provides a real-time TUI showing test progress

## Prerequisites

Before running the integration tests, you need to build the required Docker image:

```bash
make build-datadog-agent-image
```

This creates the `saluki-images/datadog-agent:latest` image used by the tests.

## Running the Tests

### Run all integration tests

```bash
make test-integration
```

This builds Panoramic and runs all integration tests.

### Run specific tests

```bash
./target/release/panoramic run --tests basic-startup,dogstatsd-enabled
```

### List available tests

```bash
make list-integration-tests
```

Or directly:

```bash
./target/release/panoramic list
```

### Additional options

```bash
# Run with custom parallelism (default: 4)
./target/release/panoramic run --parallelism 2

# Stop on first failure
./target/release/panoramic run --fail-fast

# Disable TUI for CI environments
./target/release/panoramic run --no-tui

# Output results as JSON
./target/release/panoramic run --output json

# Verbose output (show all assertion details)
./target/release/panoramic run --verbose
```

## Adding a New Test

### Directory Structure

Each test case lives in its own directory under `cases/`:

```
test/integration-tests/
  cases/
    my-new-test/
      config.yaml           # Test configuration (required)
      datadog.yaml          # Optional config files to mount
      other-config.conf     # Any additional files needed
```

### Configuration Schema

Create a `config.yaml` file in your test directory:

```yaml
# Required: unique name for the test
name: "my-new-test"

# Optional: description of what this test verifies
description: "Verifies that feature X works correctly"

# Required: overall timeout for the test
timeout: 60s

# Required: container configuration
container:
  # Docker image to use
  image: "saluki-images/datadog-agent:latest"

  # Optional: override entrypoint
  entrypoint: []

  # Optional: override command
  command: []

  # Environment variables
  env:
    DD_API_KEY: "test-api-key"
    DD_HOSTNAME: "integration-test"
    DD_DATA_PLANE_ENABLED: "true"

  # Files to mount (host_path:container_path)
  # Paths are relative to the test case directory
  files:
    - "datadog.yaml:/etc/datadog-agent/datadog.yaml"

  # Ports to expose (port/protocol)
  exposed_ports:
    - "8125/udp"
    - "5102/tcp"

# Required: list of assertions to run (executed sequentially)
assertions:
  - type: process_stable_for
    duration: 10s

  - type: port_listening
    port: 8125
    protocol: udp
    timeout: 30s

  - type: log_contains
    pattern: "Topology healthy"
    timeout: 30s
```

### Available Assertions

#### `process_stable_for`

Verifies the container process doesn't exit unexpectedly for a specified duration.

```yaml
- type: process_stable_for
  duration: 15s
```

#### `port_listening`

Waits for a port to become available.

```yaml
- type: port_listening
  port: 8125
  protocol: udp    # "tcp" or "udp"
  timeout: 30s
```

#### `log_contains`

Waits for a pattern to appear in the container logs.

```yaml
- type: log_contains
  pattern: "Agent started"
  regex: false           # Optional: treat pattern as regex (default: false)
  timeout: 30s
  stream: both           # Optional: "stdout", "stderr", or "both" (default: "both")
```

#### `log_not_contains`

Verifies a pattern does NOT appear in the logs for a duration.

```yaml
- type: log_not_contains
  pattern: "panic|PANIC"
  regex: true
  during: 15s
  stream: both
```

#### `health_check`

Checks an HTTP health endpoint.

```yaml
- type: health_check
  endpoint: "http://localhost:5102/health"
  expected_status: 200
  timeout: 30s
```

### Duration Format

Durations support human-readable formats:

- `500ms` - milliseconds
- `10s` - seconds
- `1m` - minutes
- `1m30s` - combined

### Tips for Writing Tests

1. **Start simple**: Begin with `process_stable_for` to verify basic startup.

2. **Use appropriate timeouts**: Container startup can take several seconds. Allow enough time for the process to initialize.

3. **Check for absence of errors**: Use `log_not_contains` to verify no panics or fatal errors occur.

4. **Assertions run sequentially**: If an assertion fails, subsequent assertions are skipped.

5. **File paths are relative**: Paths in the `files` list are relative to the test case directory.

## Troubleshooting

### Tests fail with "image not found"

Make sure you've built the Docker image:

```bash
make build-datadog-agent-image
```

### Tests hang or timeout

- Check that the container is starting correctly
- Verify environment variables are set properly
- Try running the container manually to debug:

```bash
docker run --rm -it saluki-images/datadog-agent:latest
```

### Port assertions fail

- Ensure the port is listed in `exposed_ports`
- Verify the service inside the container is configured to bind to that port
- Check that the protocol (tcp/udp) matches what the service uses
