// Command generate-render-vectors writes the byte-exact test vectors for rendering OTLP log content.
//
// Each vector is a translated OTLP log, and the message content for it: the `datadogV2.HTTPLogItem` without its
// `ddtags`, serialized by its own MarshalJSON. The status is set as the last additional property, as the reference
// translator (`transform.go`) sets it.
//
// The Go version and the datadog-api-client-go version are pinned in go.mod, and should match the versions that the
// reference implementation builds with. Regenerate the vectors from the repository root with:
//
//	go -C lib/saluki-components/src/common/otlp/logs/testdata/generate-render-vectors run . \
//	    > lib/saluki-components/src/common/otlp/logs/testdata/render_vectors.json
package main

import (
	"encoding/json"
	"fmt"
	"math"
	"os"
	"runtime"

	"github.com/DataDog/datadog-api-client-go/v2/api/datadog"
	"github.com/DataDog/datadog-api-client-go/v2/api/datadogV2"
)

const goVersion = "go1.26.9"

// input is a translated log: the fields of the `HTTPLogItem` that the translator sets, and its status.
type input struct {
	Hostname   *string        `json:"hostname,omitempty"`
	Service    *string        `json:"service,omitempty"`
	Message    string         `json:"message"`
	Status     string         `json:"status"`
	Properties map[string]any `json:"properties"`
}

type vector struct {
	Name    string `json:"name"`
	Input   input  `json:"input"`
	Content string `json:"content"`
}

func render(in input) (string, error) {
	item := datadogV2.HTTPLogItem{
		Hostname:             in.Hostname,
		Service:              in.Service,
		Message:              in.Message,
		AdditionalProperties: map[string]any{},
	}
	for key, value := range in.Properties {
		item.AdditionalProperties[key] = value
	}
	item.AdditionalProperties["status"] = in.Status
	// The exporter moves the tags to the message origin before serializing the item.
	item.Ddtags = nil
	content, err := item.MarshalJSON()
	return string(content), err
}

func main() {
	if runtime.Version() != goVersion {
		fmt.Fprintf(os.Stderr, "generate-render-vectors must run with %s, not %s\n", goVersion, runtime.Version())
		os.Exit(1)
	}

	host := datadog.PtrString
	inputs := []struct {
		name string
		in   input
	}{
		{"minimal", input{Message: "", Status: ""}},
		{"message-and-status", input{Message: "hello", Status: "debug"}},
		{"hostname-and-service", input{Hostname: host("host-a"), Service: host("svc"), Message: "hello", Status: "info"}},
		{"properties", input{Message: "hello", Status: "warn", Properties: map[string]any{
			"app":                  "test",
			"otel.severity_number": "13",
			"@timestamp":           "2023-11-20T16:55:03.397Z",
			"dd.trace_id":          "167772160",
			"Zebra":                "upper case sorts first",
			"a.b.c":                "dotted",
		}}},
		// Properties overwrite the top-level fields of the item.
		{"collision-message", input{Message: "from body", Status: "info", Properties: map[string]any{"message": "from resource"}}},
		{"collision-hostname", input{Hostname: host("host-a"), Message: "m", Status: "info", Properties: map[string]any{"hostname": "from attribute"}}},
		{"collision-service", input{Service: host("svc"), Message: "m", Status: "info", Properties: map[string]any{"service": "from attribute"}}},
		// The status is set after every other property, so it overwrites a `status` resource attribute.
		{"collision-status", input{Message: "m", Status: "error", Properties: map[string]any{"status": "from resource"}}},
		// Only the item's own tags are removed; a `ddtags` property stays in the content.
		{"collision-ddtags", input{Message: "m", Status: "info", Properties: map[string]any{"ddtags": "from:resource"}}},
		{"collision-log-source", input{Message: "m", Status: "info", Properties: map[string]any{"datadog.log.source": "custom_source"}}},
		{"empty-status", input{Hostname: host("host-a"), Message: "m", Status: "", Properties: map[string]any{"k": "v"}}},
		{"html-characters", input{Message: "<a href=\"x\">&amp;</a>", Status: "info", Properties: map[string]any{"<key>": "a & b > c"}}},
		{"line-separators", input{Message: "one\u2028two\u2029three", Status: "info"}},
		{"control-characters", input{Message: "tab\tnew\nline\rcr\x00nul\x1fus\x7fdel\"quote\\backslash/slash", Status: "info"}},
		{"non-ascii", input{Message: "café ☕ 日本語 😀", Status: "info", Properties: map[string]any{"ключ": "значение"}}},
		{"numbers", input{Message: "m", Status: "info", Properties: map[string]any{
			"int":          int64(1),
			"negative":     int64(-42),
			"int64-min":    int64(math.MinInt64),
			"int64-max":    int64(math.MaxInt64),
			"zero-float":   0.0,
			"integral":     3.0,
			"fraction":     0.1,
			"third":        1.0 / 3.0,
			"small":        1e-7,
			"small-edge":   1e-6,
			"large":        1e21,
			"large-edge":   1e20,
			"max-float":    math.MaxFloat64,
			"min-positive": 5e-324,
			"bool":         true,
			"null":         nil,
		}}},
		{"nested", input{Message: "m", Status: "debug", Properties: map[string]any{
			"array": []any{"value1", int64(2), 3.5, false, nil},
			"array_with_maps": []any{
				map[string]any{"item_name": "item1", "value": int64(100)},
				map[string]any{"value": int64(200), "item_name": "<item2>"},
			},
			"matrix": []any{[]any{map[string]any{"b": int64(1), "a": int64(0)}}, []any{}},
			"empty":  map[string]any{},
		}}},
	}

	vectors := make([]vector, 0, len(inputs))
	for _, entry := range inputs {
		content, err := render(entry.in)
		if err != nil {
			fmt.Fprintf(os.Stderr, "%s: %v\n", entry.name, err)
			os.Exit(1)
		}
		vectors = append(vectors, vector{Name: entry.name, Input: entry.in, Content: content})
	}

	encoder := json.NewEncoder(os.Stdout)
	encoder.SetIndent("", "  ")
	if err := encoder.Encode(vectors); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
