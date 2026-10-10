// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package record

import (
	"bytes"
	"encoding/json"
	"errors"
	"math"
	"testing"
	"time"
)

// goldenLines are the record format's example lines, one line each.
const goldenLines = `{"agent_commit":"281d921619d52ce7b99aef40607285992c9c2e89","container_image":"golang@sha256:e30143be198ab04cf7ba25fba83ab3a692ca584c994aad0bf131fa0eb32dd8c1","containerized":false,"features":[],"format":1,"go_version":"go1.26.7","goarch":"arm64","goos":"linux","inputs_digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000","type":"header"}
{"case":"additional-endpoints-env","group":"behavior","inputs":{"env":{"DD_ADDITIONAL_ENDPOINTS":"{\"https://x.test\": [\"k\"]}"}},"origin":"datadog.yaml","type":"case","why":["env-map-raw-string"]}
{"case":"additional-endpoints-env","key":"additional_endpoints","reads":{"snapshot":{"getters":[{"getter":"GetStringMapStringSlice","result":{"https://x.test":["k"]}}],"go_type":"string"}},"snapshot":{"source":"environment-variable","value":"{\"https://x.test\": [\"k\"]}"},"type":"key"}
{"case":"logs-enabled-yes-yaml","group":"behavior","inputs":{"yaml":"logs_enabled: \"yes\"\n"},"origin":"datadog.yaml","type":"case","why":["getter-bool-string-strict-parsebool","yaml-type-mismatch-scalar-leaf-keeps-raw"]}
{"case":"logs-enabled-yes-yaml","key":"logs_enabled","reads":{"snapshot":{"getters":[{"getter":"GetBool","result":false,"warnings":[{"level":"WARN","message":"failed to get configuration value for key \"logs_enabled\": strconv.ParseBool: parsing \"yes\": invalid syntax"}]}],"go_type":"string"}},"snapshot":{"source":"file","value":"yes"},"type":"key"}
`

func mustResult(t *testing.T, v interface{}) json.RawMessage {
	t.Helper()
	b, err := EncodeResult(v)
	if err != nil {
		t.Fatalf("encode %#v: %v", v, err)
	}
	return b
}

func strp(s string) *string { return &s }

func TestGoldenLines(t *testing.T) {
	envCase := mustParse(t, exampleEnvCase)
	yamlCase := mustParse(t, exampleYAMLCase)
	lines := []Line{
		&HeaderLine{
			AgentCommit:    "281d921619d52ce7b99aef40607285992c9c2e89",
			GOOS:           "linux",
			GOARCH:         "arm64",
			GoVersion:      "go1.26.7",
			ContainerImage: "golang@sha256:e30143be198ab04cf7ba25fba83ab3a692ca584c994aad0bf131fa0eb32dd8c1",
			InputsDigest:   "sha256:0000000000000000000000000000000000000000000000000000000000000000",
		},
		&CaseLine{Inputs: envCase, Origin: strp("datadog.yaml")},
		&KeyLine{
			Case:     envCase.Name,
			Key:      "additional_endpoints",
			Snapshot: &Setting{Source: "environment-variable", Value: json.RawMessage(`"{\"https://x.test\": [\"k\"]}"`)},
			SnapshotRead: Read{
				Getters: []GetterResult{{
					Getter: "GetStringMapStringSlice",
					Result: mustResult(t, map[string][]string{"https://x.test": {"k"}}),
				}},
				GoType: "string",
				Source: "environment-variable",
			},
		},
		&CaseLine{Inputs: yamlCase, Origin: strp("datadog.yaml")},
		&KeyLine{
			Case:     yamlCase.Name,
			Key:      "logs_enabled",
			Snapshot: &Setting{Source: "file", Value: json.RawMessage(`"yes"`)},
			SnapshotRead: Read{
				Getters: []GetterResult{{
					Getter: "GetBool",
					Result: mustResult(t, false),
					Warnings: []Warning{{
						Level:   "WARN",
						Message: `failed to get configuration value for key "logs_enabled": strconv.ParseBool: parsing "yes": invalid syntax`,
					}},
				}},
				GoType: "string",
				Source: "file",
			},
		},
	}
	var buf bytes.Buffer
	for _, l := range lines {
		if err := WriteLine(&buf, l); err != nil {
			t.Fatalf("write: %v", err)
		}
	}
	if got := buf.String(); got != goldenLines {
		t.Errorf("golden mismatch:\ngot:\n%s\nwant:\n%s", got, goldenLines)
	}
}

func TestLineDetails(t *testing.T) {
	c := mustParse(t, exampleLayersCase)
	lines := map[string]Line{
		// Update value omitted for unset, op written only for unset, getters override kept, <>& unescaped.
		`{"case":"dogstatsd-port-layers","group":"behavior","inputs":{"cli":[{"key":"cmd_port","value":"5099"}],"fleet_policy":"dogstatsd_port: 8130\n","keys":[{"key":"dogstatsd_port"},{"getters":["GetInt","GetString"],"key":"cmd_port"}],"updates":[{"key":"dogstatsd_port","source":"remote-config","value":8131},{"key":"dogstatsd_port","op":"unset","source":"remote-config"}]},"startup_error":"a <b> & c","type":"case","why":["fleet-policies-outranked","unset-always-notifies-even-if-unchanged"]}`: &CaseLine{
			Inputs: c, StartupError: strp("a <b> & c"),
		},
		// Final source compared with the last event; snapshot null forces source; update implied.
		`{"case":"k","events":[{"seq":1,"source":"remote-config","value":8131}],"key":"x","reads":{"final":{"getters":[],"go_type":"<nil>","source":"default"},"snapshot":{"getters":[{"getter":"Get","result":null}],"go_type":"<nil>","source":"default"}},"snapshot":null,"type":"key"}`: &KeyLine{
			Case: "k", Key: "x",
			Events: []Event{
				{Setting: Setting{Source: "remote-config", Value: json.RawMessage(`8131`)}, Seq: 1, Update: 0},
			},
			SnapshotRead: Read{Getters: []GetterResult{{Getter: "Get", Result: mustResult(t, nil)}}, GoType: "<nil>", Source: "default"},
			FinalRead:    &Read{GoType: "<nil>", Source: "default"},
		},
	}
	for _, l := range lines {
		if k, ok := l.(*KeyLine); ok {
			k.BindCase([]Update{{Op: "set", Key: "x", Source: "remote-config"}})
		}
	}
	for want, l := range lines {
		got, err := MarshalLine(l)
		if err != nil {
			t.Fatalf("marshal: %v", err)
		}
		if string(got) != want {
			t.Errorf("got:\n%s\nwant:\n%s", got, want)
		}
	}
}

func TestValidate(t *testing.T) {
	c := mustParse(t, exampleEnvCase)
	yes := true
	features := []string{"docker"}
	tests := []struct {
		name string
		line Line
		want error
	}{
		{"origin and startup_error", &CaseLine{Inputs: c, Origin: strp("datadog.yaml"), StartupError: strp("boom")}, ErrRecordOrigin},
		{"neither origin nor startup_error", &CaseLine{Inputs: c}, ErrRecordOrigin},
		{"features on startup failure", &CaseLine{Inputs: c, StartupError: strp("boom"), Features: &features}, ErrRecordStartupFailure},
		{"containerized on startup failure", &CaseLine{Inputs: c, StartupError: strp("boom"), Containerized: &yes}, ErrRecordStartupFailure},
		{"timed_out with seq_delta 0", &CaseLine{Inputs: c, Origin: strp("datadog.yaml"), Updates: []UpdateResult{{SeqDelta: 0, TimedOut: true}}}, ErrRecordTimedOut},
		{"side effects unsorted", &CaseLine{Inputs: c, Origin: strp("datadog.yaml"), SideEffects: []SideEffect{{Key: "b"}, {Key: "a"}}}, ErrRecordSideEffects},
		{"construction warning bad level", &CaseLine{Inputs: c, Origin: strp("datadog.yaml"), ConstructionWarnings: []Warning{{Level: "WARN+1", Message: "boom"}}}, ErrWarningLevel},
		{"getter warning bad level", &KeyLine{Case: "a", Key: "x", SnapshotRead: Read{Getters: []GetterResult{{Getter: "Get", Warnings: []Warning{{Level: "WARN+1", Message: "boom"}}}}}}, ErrWarningLevel},
		{"bad commit", &HeaderLine{AgentCommit: "281d921619d", GOOS: "l", GOARCH: "a", GoVersion: "g", ContainerImage: "i"}, ErrRecordCommit},
		{"bad inputs_digest", &HeaderLine{
			AgentCommit: "281d921619d52ce7b99aef40607285992c9c2e89", GOOS: "l", GOARCH: "a", GoVersion: "g", ContainerImage: "i",
			InputsDigest: "sha256:deadbeef",
		}, ErrRecordInputsDigest},
	}
	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			if err := tc.line.Validate(); !errors.Is(err, tc.want) {
				t.Fatalf("got %v, want %v", err, tc.want)
			}
			if _, err := MarshalLine(tc.line); err == nil {
				t.Fatal("MarshalLine wrote an invalid line")
			}
		})
	}
	ok := &CaseLine{Inputs: c, Origin: strp("datadog.yaml"), Updates: []UpdateResult{{SeqDelta: 1, TimedOut: true}}}
	if err := ok.Validate(); err != nil {
		t.Errorf("timed_out with seq_delta 1: %v", err)
	}
}

func TestEncodeResult(t *testing.T) {
	type named struct{ A int }
	var nilMap map[string]interface{}
	var nilSlice []string
	tests := []struct {
		name string
		in   interface{}
		want string
	}{
		{"duration", time.Duration(10 * time.Second), `10000000000`},
		{"int64 beyond 2^53", int64(1<<53 + 1), `9007199254740993`},
		{"uint64 max", uint64(math.MaxUint64), `18446744073709551615`},
		{"float one", float64(1), `1.0`},
		{"float fraction", 1.5, `1.5`},
		{"float large", 1e21, `1e+21`},
		{"float small", 1e-7, `1e-7`},
		{"float32", float32(0.1), `0.1`},
		{"nan", math.NaN(), `{"$float":"NaN"}`},
		{"+inf", math.Inf(1), `{"$float":"+Inf"}`},
		{"-inf", math.Inf(-1), `{"$float":"-Inf"}`},
		{"nil", nil, `null`},
		{"nil map", nilMap, `null`},
		{"empty map", map[string]interface{}{}, `{}`},
		{"nil slice", nilSlice, `null`},
		{"empty slice", []string{}, `[]`},
		{"string no html escape", "<a&b>", `"<a&b>"`},
		{"nested sorted", map[string]interface{}{"b": []interface{}{1, 2.0, nil}, "a": map[string]string{"y": "1", "x": "2"}}, `{"a":{"x":"2","y":"1"},"b":[1,2.0,null]}`},
		{"interface keys", map[interface{}]interface{}{1: true, "a": 3}, `{"1":true,"a":3}`},
		{"array", [2]int{1, 2}, `[1,2]`},
	}
	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			got, err := EncodeResult(tc.in)
			if err != nil {
				t.Fatal(err)
			}
			if string(got) != tc.want {
				t.Fatalf("got %s, want %s", got, tc.want)
			}
		})
	}

	failures := map[string]interface{}{
		"bytes":         []byte("x"),
		"struct":        named{A: 1},
		"pointer":       new(int),
		"invalid utf8":  "\xff",
		"key collision": map[interface{}]interface{}{1: "a", "1": "b"},
		"lone $float":   map[string]interface{}{"$float": "NaN"},
		"int map key":   map[int]string{1: "a"},
		"nested struct": []interface{}{named{}},
	}
	for name, in := range failures {
		t.Run("fail "+name, func(t *testing.T) {
			if _, err := EncodeResult(in); !errors.Is(err, ErrResultEncoding) {
				t.Fatalf("got %v, want ErrResultEncoding", err)
			}
		})
	}
}
