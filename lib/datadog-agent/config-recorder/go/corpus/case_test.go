// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package corpus

import (
	"errors"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

const exampleEnvCase = `name: additional-endpoints-env
group: behavior
why: [env-map-raw-string]
env:
  DD_ADDITIONAL_ENDPOINTS: '{"https://x.test": ["k"]}'
keys: [additional_endpoints]
`

const exampleYAMLCase = `name: logs-enabled-yes-yaml
group: behavior
why: [getter-bool-string-strict-parsebool, yaml-type-mismatch-scalar-leaf-keeps-raw]
yaml: |
  logs_enabled: "yes"
keys: [logs_enabled]
`

const exampleLayersCase = `name: dogstatsd-port-layers
group: behavior
why: [fleet-policies-outranked, unset-always-notifies-even-if-unchanged]
fleet_policy: |
  dogstatsd_port: 8130
cli:
  - {key: cmd_port, value: "5099"}
updates:
  - {key: dogstatsd_port, value: 8131, source: remote-config}
  - {op: unset, key: dogstatsd_port, source: remote-config}
keys:
  - dogstatsd_port
  - {key: cmd_port, getters: [GetInt, GetString]}
`

func mustParse(t *testing.T, src string) *Case {
	t.Helper()
	c, err := ParseCase([]byte(src))
	if err != nil {
		t.Fatalf("parse: %v", err)
	}
	return c
}

func TestParseExamples(t *testing.T) {
	c := mustParse(t, exampleEnvCase)
	if c.Env["DD_ADDITIONAL_ENDPOINTS"] != `{"https://x.test": ["k"]}` || c.Keys[0].Key != "additional_endpoints" {
		t.Errorf("env case: %+v", c)
	}

	c = mustParse(t, exampleYAMLCase)
	if c.YAML == nil || *c.YAML != "logs_enabled: \"yes\"\n" || len(c.Why) != 2 {
		t.Errorf("yaml case: %+v", c)
	}

	c = mustParse(t, exampleLayersCase)
	if c.FleetPolicy == nil || *c.FleetPolicy != "dogstatsd_port: 8130\n" {
		t.Errorf("fleet_policy: %v", c.FleetPolicy)
	}
	if got, ok := c.CLI[0].Value.(string); !ok || got != "5099" {
		t.Errorf("cli value: %#v", c.CLI[0].Value)
	}
	if got, ok := c.Updates[0].Value.(int); !ok || got != 8131 || c.Updates[0].Op != "set" {
		t.Errorf("update 0: %#v", c.Updates[0])
	}
	if u := c.Updates[1]; u.Op != "unset" || u.Value != nil || u.Source != "remote-config" {
		t.Errorf("update 1: %#v", u)
	}
	wantKeys := []KeyEntry{{Key: "dogstatsd_port"}, {Key: "cmd_port", Getters: []string{"GetInt", "GetString"}}}
	if !reflect.DeepEqual(c.Keys, wantKeys) {
		t.Errorf("keys: %#v", c.Keys)
	}
}

func TestTypedValues(t *testing.T) {
	src := `name: typed
group: depth
cli:
  - {key: a, value: "s"}
  - {key: b, value: true}
  - {key: c, value: -9223372036854775808}
  - {key: d, value: 1.5}
  - {key: e, value: 1e3}
  - {key: f, value: null}
  - {key: g, value: [1, "x", [2.0]]}
  - {key: h, value: {k: 1, n: {m: false}}}
  - {key: i, value: 0x10}
  - {key: j, value: {"$float": "x"}}
keys: [a]
`
	c := mustParse(t, src)
	want := []interface{}{
		"s", true, int(-9223372036854775808), 1.5, 1000.0, nil,
		[]interface{}{1, "x", []interface{}{2.0}},
		map[string]interface{}{"k": 1, "n": map[string]interface{}{"m": false}},
		16,
		// A typed-value map is not a getter result: its "$float" key has no special meaning
		// (record.md §3.1), unlike a getter result's non-finite-float sentinel (getter-map.md §3).
		map[string]interface{}{"$float": "x"},
	}
	for i, o := range c.CLI {
		if !reflect.DeepEqual(o.Value, want[i]) {
			t.Errorf("cli %s: got %#v, want %#v", o.Key, o.Value, want[i])
		}
		// The JSON form in a case line must decode back to the same Go value.
		j, err := encodeTypedValue(o.Value)
		if err != nil {
			t.Fatalf("encode %s: %v", o.Key, err)
		}
		back, err := decodeTypedJSON(j)
		if err != nil {
			t.Fatalf("decode %s (%s): %v", o.Key, j, err)
		}
		if !reflect.DeepEqual(back, o.Value) {
			t.Errorf("round trip %s via %s: got %#v", o.Key, j, back)
		}
	}
}

func TestParseRejections(t *testing.T) {
	cases := []struct {
		name string
		src  string
		want error
	}{
		{"unknown field", "name: a\ngroup: depth\nkeys: [x]\nextra: 1\n", ErrCaseSyntax},
		{"unknown key-entry field", "name: a\ngroup: depth\nkeys: [{key: x, getter: [Get]}]\n", ErrCaseSyntax},
		{"unknown update field", "name: a\ngroup: depth\nkeys: [x]\nupdates: [{key: x, value: 1, source: cli, when: 1}]\n", ErrCaseSyntax},
		{"missing name", "group: depth\nkeys: [x]\n", ErrCaseMissingField},
		{"missing keys", "name: a\ngroup: depth\n", ErrCaseNoKeys},
		{"empty file", "", ErrCaseMissingField},
		{"two documents", "name: a\ngroup: depth\nkeys: [x]\n---\nname: b\n", ErrCaseMultipleDocs},
		{"name not a string", "name: 5\ngroup: depth\nkeys: [x]\n", ErrCaseFieldType},
		{"bad name", "name: A_b\ngroup: depth\nkeys: [x]\n", ErrCaseName},
		{"bad group", "name: a\ngroup: other\nkeys: [x]\n", ErrCaseGroup},
		{"behavior without why", "name: a\ngroup: behavior\nkeys: [x]\n", ErrCaseWhyRequired},
		{"uppercase key", "name: a\ngroup: depth\nkeys: [Logs_enabled]\n", ErrCaseKeyUppercase},
		{"duplicate key", "name: a\ngroup: depth\nkeys: [x, {key: x}]\n", ErrCaseKeyDuplicate},
		{"empty getters", "name: a\ngroup: depth\nkeys: [{key: x, getters: []}]\n", ErrCaseGettersEmpty},
		{"unknown getter", "name: a\ngroup: depth\nkeys: [{key: x, getters: [GetSource]}]\n", ErrCaseGetterUnknown},
		{"nan typed value", "name: a\ngroup: depth\nkeys: [x]\ncli: [{key: x, value: .nan}]\n", ErrTypedNonFinite},
		{"inf typed value", "name: a\ngroup: depth\nkeys: [x]\nupdates: [{key: x, value: -.inf, source: cli}]\n", ErrTypedNonFinite},
		{"int above int64", "name: a\ngroup: depth\nkeys: [x]\ncli: [{key: x, value: 9223372036854775808}]\n", ErrTypedIntRange},
		{"int above uint64", "name: a\ngroup: depth\nkeys: [x]\ncli: [{key: x, value: 99999999999999999999}]\n", ErrTypedIntRange},
		{"non-string map key", "name: a\ngroup: depth\nkeys: [x]\ncli: [{key: x, value: {1: a}}]\n", ErrTypedMapKey},
		{"binary tag", "name: a\ngroup: depth\nkeys: [x]\ncli: [{key: x, value: !!binary aGk=}]\n", ErrTypedTag},
		{"timestamp tag", "name: a\ngroup: depth\nkeys: [x]\ncli: [{key: x, value: 2001-12-14}]\n", ErrTypedTag},
		{"duplicate cli key", "name: a\ngroup: depth\nkeys: [x]\ncli: [{key: x, value: 1}, {key: x, value: 2}]\n", ErrCaseCLIDuplicate},
		{"cli without value", "name: a\ngroup: depth\nkeys: [x]\ncli: [{key: x}]\n", ErrCaseMissingField},
		{"bad source", "name: a\ngroup: depth\nkeys: [x]\nupdates: [{key: x, value: 1, source: default}]\n", ErrCaseUpdateSource},
		{"missing source", "name: a\ngroup: depth\nkeys: [x]\nupdates: [{key: x, value: 1}]\n", ErrCaseMissingField},
		{"bad op", "name: a\ngroup: depth\nkeys: [x]\nupdates: [{op: del, key: x, source: cli}]\n", ErrCaseUpdateOp},
		{"value on unset", "name: a\ngroup: depth\nkeys: [x]\nupdates: [{op: unset, key: x, value: 1, source: cli}]\n", ErrCaseUpdateValue},
		{"null value on unset", "name: a\ngroup: depth\nkeys: [x]\nupdates: [{op: unset, key: x, value: null, source: cli}]\n", ErrCaseUpdateValue},
		{"set without value", "name: a\ngroup: depth\nkeys: [x]\nupdates: [{key: x, source: cli}]\n", ErrCaseUpdateValue},
		{"secrets present", "name: a\ngroup: depth\nkeys: [x]\nsecrets: {h: p}\n", ErrCaseSecrets},
		{"non-string env value", "name: a\ngroup: depth\nkeys: [x]\nenv: {DD_X: 1}\n", ErrCaseEnvValue},
		{"bool env value", "name: a\ngroup: depth\nkeys: [x]\nenv: {DD_X: true}\n", ErrCaseEnvValue},
		{"bad env name", "name: a\ngroup: depth\nkeys: [x]\nenv: {DD-X: \"1\"}\n", ErrCaseEnvName},
		{"non-string env name", "name: a\ngroup: depth\nkeys: [x]\nenv: {true: \"1\"}\n", ErrCaseEnvName},
		{"yaml not a string", "name: a\ngroup: depth\nkeys: [x]\nyaml: 5\n", ErrCaseFieldType},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			_, err := ParseCase([]byte(tc.src))
			if !errors.Is(err, tc.want) {
				t.Fatalf("got %v, want %v", err, tc.want)
			}
		})
	}
}

func TestParseAccepts(t *testing.T) {
	c := mustParse(t, "name: a\ngroup: depth\nkeys: [x]\nenv: {DD_EMPTY: \"\"}\nupdates: [{key: x, value: null, source: environment-variable}]\n")
	if v, ok := c.Env["DD_EMPTY"]; !ok || v != "" {
		t.Errorf("empty env value dropped: %#v", c.Env)
	}
	if len(c.Why) != 0 || c.Why == nil {
		t.Errorf("why must default to []: %#v", c.Why)
	}
	if u := c.Updates[0]; u.Op != "set" || u.Value != nil {
		t.Errorf("set to null: %#v", u)
	}
}

func TestParseCaseFileStem(t *testing.T) {
	dir := t.TempDir()
	good := filepath.Join(dir, "additional-endpoints-env.yaml")
	if err := os.WriteFile(good, []byte(exampleEnvCase), 0o644); err != nil {
		t.Fatal(err)
	}
	if _, err := ParseCaseFile(good); err != nil {
		t.Errorf("good file: %v", err)
	}
	bad := filepath.Join(dir, "other.yaml")
	if err := os.WriteFile(bad, []byte(exampleEnvCase), 0o644); err != nil {
		t.Fatal(err)
	}
	if _, err := ParseCaseFile(bad); !errors.Is(err, ErrCaseNameStem) || !strings.Contains(err.Error(), "other") {
		t.Errorf("stem mismatch: %v", err)
	}
}
