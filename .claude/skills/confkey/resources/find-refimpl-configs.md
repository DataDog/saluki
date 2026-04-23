# Find RefImpl Configs

This is a substep of the `/confkey` skill. Your job is to discover all configuration keys (ConfKeys)
registered in the datadog-agent Go codebase and in the example datadog.yaml file.

## Input

- `{{datadog-agent}}` = the path to the root of the datadog-agent repository
- `{{tmp}}` = your output directory

## System Overview

The Datadog Agent uses Viper (via a custom wrapper) for configuration. Keys are registered in Go
source code with default values and env var bindings, and can also appear in the example
`datadog.yaml` configuration file.

## Step 1: Discover the Config API Surface

Read `pkg/config/model/types.go` and build a complete list of every method on the `Setup` interface
(registration) and `Reader` interface (accessors). Do not rely solely on the examples in Steps 2-3
-- methods may have been added or renamed. Search for all of them.

Also check for wrapper functions that delegate to these interfaces (e.g.
`pkg/config/setup/config_accessor.go`).

## Step 2: Search for Config Key Registration

The primary config registry is `pkg/config/setup/common_settings.go`. Other files in
`pkg/config/setup/` register keys for specific subsystems (APM, system probe, etc.).

Search all `.go` files under `pkg/config/` for calls to each `Setup` method from Step 1. The first
string argument is the ConfKey.

Known registration patterns:

```go
// Most common (~95% of keys):
config.BindEnvAndSetDefault("dogstatsd_port", 8125)

// Default without env binding:
config.SetDefault("key_name", defaultValue)

// Env binding without default:
config.BindEnv("dogstatsd_mapper_profiles")

// Custom env parsing (key is still the first argument):
config.ParseEnvAsSlice("key_name", func(in string) []interface{} { ... })
config.ParseEnvAsStringSlice("key_name", func(string) []string { ... })
config.ParseEnvAsMapStringInterface("key_name", func(string) map[string]interface{} { ... })
```

For each match, extract the first string literal as the ConfKey and record file:line as the
location.

## Step 3: Search for Config Key Reads

Some keys may only appear at read sites, not at registration sites. Using the `Reader` interface
methods you discovered in Step 1, search `comp/dogstatsd/` and `pkg/config/setup/` for accessor
calls.

As of this writing, the known accessor patterns are:

```go
.GetString("key")
.GetBool("key")
.GetInt("key")
.GetFloat64("key")
.GetDuration("key")
.GetStringSlice("key")
.GetStringMap("key")
.GetStringMapString("key")
```

Only include keys from read sites that were NOT already found at registration sites.

## Step 4: Validate Completeness

- Verify these known keys appear in your output with correct locations: `dogstatsd_port`,
  `dogstatsd_buffer_size`, `use_dogstatsd`, `dogstatsd_socket`, `statsd_metric_namespace`.
- Skim `common_settings.go` for registration patterns you may have missed (loops, helpers, unusual
  call patterns).
- If you find a new pattern, search for it comprehensively.

## Source 2: Example YAML File

The file `cmd/agent/dist/datadog.yaml` is a ~1600-line example config with most sections commented
out. It uses standard YAML nesting.

Parse this file to extract all config key paths. Use dot-separated flattening to match the format
used in Go code:

```yaml
# In the YAML:
proxy:
  http: http://example.com
  https: https://example.com

# Becomes these ConfKeys:
# proxy.http
# proxy.https
```

Note: commented-out keys (lines starting with `#`) should still be included -- this is an example
file where most settings are intentionally commented out.

### What NOT to include

- Test files
- Keys that appear only in comments describing other keys
- Internal/framework keys not meant for user configuration

## Output

Write TWO files:

### `{{tmp}}/refimpl-go-config-keys.csv`

Keys discovered from Go source code:

```csv
"dogstatsd_port","pkg/config/setup/common_settings.go:1524"
"dogstatsd_buffer_size","pkg/config/setup/common_settings.go:1526"
"use_dogstatsd","pkg/config/setup/common_settings.go:1523"
```

### `{{tmp}}/refimpl-yaml-config-keys.csv`

Keys discovered from the example YAML file:

```csv
"api_key","cmd/agent/dist/datadog.yaml:15"
"proxy.http","cmd/agent/dist/datadog.yaml:42"
```
