// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package gen

import (
	"fmt"
	"math"
	"reflect"
	"strings"
	"testing"

	"github.com/DataDog/datadog-agent/cmd/config-recorder/record"
	"github.com/DataDog/datadog-agent/cmd/config-recorder/schema"
)

const testSchema = `
properties:
  flag: {node_type: setting, type: boolean, default: true}
  port: {node_type: setting, type: integer, default: 8125}
  rate: {node_type: setting, type: number, default: 1}
  name: {node_type: setting, type: string, default: cr-a}
  wait: {node_type: setting, type: string, format: duration, default: 10s}
  sock: {node_type: setting, type: string, platform_default: {linux: /x.sock, other: ""}}
  quiet: {node_type: setting, type: string, default: "", tags: [no-env]}
  tags: {node_type: setting, type: array, items: {type: string}, default: [], env_parser: comma_separated}
  ends: {node_type: setting, type: object, additionalProperties: {type: array, items: {type: string}}, default: {}}
  gone: {node_type: setting, type: integer, default: 1}
  Other_Key: {node_type: setting, type: boolean, default: false}
  apm_config:
    node_type: section
    properties:
      a: {node_type: setting, type: integer, default: 0, env_vars: [DD_APM_A, DD_A]}
      b: {node_type: setting, type: integer, default: 0}
      c: {node_type: setting, type: integer, default: 0}
`

const testOverlay = `
inventory:
  flag: {support: full}
  port: {support: partial}
  rate: {support: full}
  name: {support: full}
  wait: {support: full}
  sock: {support: full}
  quiet: {support: full}
  tags: {support: full}
  ends: {support: full}
  apm_config.a: {support: full}
  apm_config.b: {support: full}
  apm_config.c: {support: full}
  gone: {support: none}
  otlp_config.x.y: {support: unknown, doc: ignored}
excluded:
  other_key: reason
`

func testFacts() *AgentFacts {
	f := &AgentFacts{DefaultType: map[string]string{"other_key": "bool"}, EnvVars: map[string]bool{}}
	for _, v := range []string{"DD_FLAG", "DD_PORT", "DD_RATE", "DD_NAME", "DD_WAIT", "DD_SOCK", "DD_TAGS", "DD_ENDS",
		"DD_APM_A", "DD_A", "DD_APM_CONFIG_B", "DD_APM_CONFIG_C", "DD_GONE", "DD_OTHER_KEY"} {
		f.EnvVars[v] = true
	}
	return f
}

func mustGenerate(t *testing.T, facts *AgentFacts) *Result {
	t.Helper()
	s, err := schema.Parse([]byte(testSchema))
	if err != nil {
		t.Fatal(err)
	}
	o, err := ParseOverlay([]byte(testOverlay))
	if err != nil {
		t.Fatal(err)
	}
	r, err := Generate(s, o, facts)
	if err != nil {
		t.Fatal(err)
	}
	return r
}

func byName(r *Result) map[string]*record.Case {
	m := map[string]*record.Case{}
	for _, c := range r.Cases {
		m[c.Name] = c
	}
	return m
}

func TestValueRules(t *testing.T) {
	s, err := schema.Parse([]byte(testSchema))
	if err != nil {
		t.Fatal(err)
	}
	want := map[string]Values{
		"flag":         {false, true},
		"port":         {8126, 8127},
		"rate":         {2.5, 3.5},
		"name":         {"cr-c", "cr-d"},
		"wait":         {"61s", "62s"},
		"sock":         {"cr-a", "cr-b"},
		"tags":         {[]interface{}{"cr-a", "cr-b"}, []interface{}{"cr-c"}},
		"ends":         {map[string]interface{}{"cr_a": []interface{}{"cr-a"}}, map[string]interface{}{"cr_b": []interface{}{"cr-b"}}},
		"apm_config.a": {1, 2},
	}
	for k, w := range want {
		got, err := ValuesFor(s[k])
		if err != nil {
			t.Fatalf("%s: %v", k, err)
		}
		if !reflect.DeepEqual(got, w) {
			t.Errorf("%s: got %#v, want %#v", k, got, w)
		}
	}
	if s["sock"].Default != "/x.sock" {
		t.Errorf("platform_default: got %v, want the linux value", s["sock"].Default)
	}
	for _, c := range []struct {
		s    schema.Key
		want Values
	}{
		{schema.Key{Type: "array", ItemType: "integer"}, Values{[]interface{}{1, 2}, []interface{}{3}}},
		{schema.Key{Type: "array", ItemType: "number"}, Values{[]interface{}{1.5, 2.5}, []interface{}{3.5}}},
		{schema.Key{Type: "array", ItemType: "object"}, Values{[]interface{}{map[string]interface{}{"cr_a": "cr-a"}},
			[]interface{}{map[string]interface{}{"cr_b": "cr-b"}}}},
		{schema.Key{Type: "object", ItemType: "string"}, Values{map[string]interface{}{"cr_a": "cr-a"},
			map[string]interface{}{"cr_b": "cr-b"}}},
		{schema.Key{Type: "object", ItemType: "number"}, Values{map[string]interface{}{"cr_a": 1.5},
			map[string]interface{}{"cr_b": 2.5}}},
		{schema.Key{Type: "object"}, Values{map[string]interface{}{"cr_a": "cr-a"}, map[string]interface{}{"cr_b": "cr-b"}}},
		{schema.Key{Type: "string", Format: "duration", Default: "62s"}, Values{"71s", "72s"}},
		{schema.Key{Type: "boolean"}, Values{true, false}},
		{schema.Key{Type: "number", Default: 0.25}, Values{1.5, 2.5}},
	} {
		got, err := ValuesFor(&c.s)
		if err != nil {
			t.Fatal(err)
		}
		if !reflect.DeepEqual(got, c.want) {
			t.Errorf("%+v: got %#v, want %#v", c.s, got, c.want)
		}
	}
	if _, err := ValuesFor(&schema.Key{Path: "k", Type: "array", Default: []interface{}{"cr-a", "cr-b"}}); err == nil {
		t.Error("a value equal to the default must fail")
	}
}

func TestEnvText(t *testing.T) {
	list := []interface{}{"cr-a", "cr-b"}
	for _, c := range []struct {
		parser string
		v      interface{}
		want   string
	}{
		{"", true, "true"},
		{"", 42, "42"},
		{"", 2.5, "2.5"},
		{"", "61s", "61s"},
		{"", list, "cr-a cr-b"},
		{"space_separated", list, "cr-a cr-b"},
		{"comma_and_space_separated", list, "cr-a cr-b"},
		{"json_list_or_space_separated", list, "cr-a cr-b"},
		{"comma_separated", list, "cr-a,cr-b"},
		{"csv_comma_separated", list, "cr-a,cr-b"},
		{"json_list_or_comma_separated", list, "cr-a,cr-b"},
		{"comma_then_space_separated", list, "cr-a,cr-b"},
		{"json", list, `["cr-a","cr-b"]`},
		{"", []interface{}{1, 2}, "1 2"},
		{"", []interface{}{map[string]interface{}{"cr_a": "cr-a"}}, `[{"cr_a":"cr-a"}]`},
		{"", map[string]interface{}{"cr_a": []interface{}{"cr-a"}}, `{"cr_a":["cr-a"]}`},
		{"traces_span", map[string]interface{}{"cr-a|cr-b": 0.5}, "cr-a|cr-b=0.5"},
	} {
		got, err := EnvText(&schema.Key{Path: "k", EnvParser: c.parser}, c.v)
		if err != nil {
			t.Fatal(err)
		}
		if got != c.want {
			t.Errorf("%q %v: got %q, want %q", c.parser, c.v, got, c.want)
		}
	}
}

func TestGroupsAndNames(t *testing.T) {
	r := mustGenerate(t, testFacts())
	cases := byName(r)
	var names []string
	for _, c := range r.Cases {
		names = append(names, c.Name)
	}
	want := []string{
		"baseline-default",
		"breadth-env-apm-config", "breadth-env-top-e", "breadth-env-top-f",
		"breadth-env-top-n", "breadth-env-top-p", "breadth-env-top-r", "breadth-env-top-s", "breadth-env-top-t",
		"breadth-env-top-w",
		"breadth-yaml-apm-config", "breadth-yaml-top-e", "breadth-yaml-top-f",
		"breadth-yaml-top-n", "breadth-yaml-top-p", "breadth-yaml-top-q", "breadth-yaml-top-r", "breadth-yaml-top-s",
		"breadth-yaml-top-t", "breadth-yaml-top-w",
		"excluded-env-top-o", "excluded-yaml-top-o",
		"unknown-env-top-c",
		"unknown-yaml-apm-config", "unknown-yaml-top-c",
		"unsupported-env-top-g", "unsupported-yaml-top-g",
	}
	if !reflect.DeepEqual(names, want) {
		t.Fatalf("names:\n got %v\nwant %v", names, want)
	}
	if got := cases["baseline-default"].Keys; len(got) != 12 || got[0].Key != "apm_config.a" {
		t.Errorf("batch keys: %v", got)
	}
	env := cases["breadth-env-apm-config"].Env
	if !reflect.DeepEqual(env, map[string]string{"DD_APM_A": "1", "DD_APM_CONFIG_B": "1", "DD_APM_CONFIG_C": "1"}) {
		t.Errorf("env: %v", env)
	}
	if got := cases["breadth-env-top-t"].Env["DD_TAGS"]; got != "cr-a,cr-b" {
		t.Errorf("env list: %q", got)
	}
	y := cases["breadth-yaml-apm-config"]
	if y.YAML == nil || *y.YAML != "apm_config:\n    a: 1\n    b: 1\n    c: 1\n" {
		t.Errorf("yaml: %v", y.YAML)
	}
	if !reflect.DeepEqual(y.Updates, []record.Update{{Op: "set", Key: "apm_config.a", Value: 2, Source: "agent-runtime"}, {Op: "set", Key: "apm_config.b", Value: 2, Source: "agent-runtime"},
		{Op: "set", Key: "apm_config.c", Value: 2, Source: "agent-runtime"}}) {
		t.Errorf("updates: %v", y.Updates)
	}
	if len(cases["unsupported-yaml-top-g"].Updates) != 0 {
		t.Error("unsupported cases have no set updates")
	}
	for _, c := range r.Cases {
		for _, k := range c.Keys {
			if k.Key == "otlp_config.x.y" {
				t.Errorf("case %s lists an overlay-only key", c.Name)
			}
		}
	}
	if got := cases["unknown-env-top-c"].Env; !reflect.DeepEqual(got,
		map[string]string{"DD_CONFIG_RECORDER_UNKNOWN": "cr-a"}) {
		t.Errorf("unknown env: %v", got)
	}
	if !reflect.DeepEqual(r.Skipped, []Skip{{"breadth", "env", "quiet", "no env binding (no-env)"}}) {
		t.Errorf("skipped: %v", r.Skipped)
	}
}

func TestDeterministic(t *testing.T) {
	a, b := mustGenerate(t, testFacts()), mustGenerate(t, testFacts())
	for i := range a.Cases {
		x, err := record.MarshalCase(a.Cases[i])
		if err != nil {
			t.Fatal(err)
		}
		y, _ := record.MarshalCase(b.Cases[i])
		if string(x) != string(y) {
			t.Fatalf("case %s differs between runs", a.Cases[i].Name)
		}
	}
}

func TestEnvNameMustBeBound(t *testing.T) {
	f := testFacts()
	delete(f.EnvVars, "DD_APM_A")
	s, _ := schema.Parse([]byte(testSchema))
	o, _ := ParseOverlay([]byte(testOverlay))
	if _, err := Generate(s, o, f); err == nil || !strings.Contains(err.Error(), "DD_APM_A") {
		t.Fatalf("got %v, want an unbound env var failure", err)
	}
}

func TestExcludedPerType(t *testing.T) {
	s, _ := schema.Parse([]byte(testSchema))
	o := &Overlay{Support: map[string]string{}, Excluded: map[string]bool{"port": true, "gone": true, "flag": true}}
	f := testFacts()
	f.DefaultType = map[string]string{"port": "int", "gone": "int", "flag": "bool"}
	r, err := Generate(s, o, f)
	if err != nil {
		t.Fatal(err)
	}
	cases := byName(r)
	if cases["excluded-yaml-top-g"] == nil || cases["excluded-yaml-top-f"] == nil || cases["excluded-yaml-top-p"] != nil {
		t.Fatalf("excluded keys: %v", r.Cases)
	}
}

// TestSameRawPrefixStillBatches checks that keys sharing the same raw first component (or raw
// first two components, for a sub-section batch) still land in one batch: sanitizing the same
// raw prefix twice must not look like a collision.
func TestSameRawPrefixStillBatches(t *testing.T) {
	names, batches, err := batch([]string{"apm_config.a", "apm_config.b"})
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(names, []string{"apm-config"}) || !reflect.DeepEqual(batches, [][]string{{"apm_config.a", "apm_config.b"}}) {
		t.Fatalf("names=%v batches=%v", names, batches)
	}

	var keys []string
	for i := 0; i < 45; i++ {
		keys = append(keys, fmt.Sprintf("big.leaf%02d", i))
	}
	names, batches, err = batch(keys)
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(names, []string{"big"}) || len(batches[0]) != 45 {
		t.Fatalf("names=%v sizes=%v", names, len(batches[0]))
	}
}

// TestSectionNameCollisionFails checks that two different raw sections that sanitize alike (case.md
// §3.2) are a harness failure, not a silent merge.
func TestSectionNameCollisionFails(t *testing.T) {
	_, _, err := batch([]string{"apm_config.a", "apm-config.b"})
	if err == nil {
		t.Fatal("different raw sections sanitizing alike must fail")
	}
	if !strings.Contains(err.Error(), "apm_config") || !strings.Contains(err.Error(), "apm-config") {
		t.Fatalf("error must name both raw prefixes: %v", err)
	}
}

// TestSubSectionNameCollisionFails checks that a sub-section split name that equals another
// section's own name is a harness failure, not a silent merge (case.md §3.2).
func TestSubSectionNameCollisionFails(t *testing.T) {
	var keys []string
	for i := 0; i < 45; i++ {
		keys = append(keys, fmt.Sprintf("big.leaf%02d", i), fmt.Sprintf("big.collector.k%02d", i))
	}
	keys = append(keys, "big_collector.x")
	_, _, err := batch(keys)
	if err == nil {
		t.Fatal("a sub-section split name equal to another section's name must fail")
	}
	if !strings.Contains(err.Error(), "big.collector") || !strings.Contains(err.Error(), "big_collector") {
		t.Fatalf("error must name both raw prefixes: %v", err)
	}
}

func TestTopLevelLeafSection(t *testing.T) {
	// A top-level leaf (no dot) batches by its first character, sanitized, as `top-<c>`
	// (case.md §3.2); a key with a dot still batches by its first path component.
	for _, c := range []struct{ key, want string }{
		{"flag", "top-f"},
		{"gone", "top-g"},
		{"9lives", "top-9"},
		{"_hidden", "top--"},
		{"apm_config.a", "apm-config"},
	} {
		if got := section(c.key); got != c.want {
			t.Errorf("section(%q) = %q, want %q", c.key, got, c.want)
		}
	}
}

// TestTopLevelLeavesBatchIndependently checks that two top-level leaves with different first
// characters never share a batch.
func TestTopLevelLeavesBatchIndependently(t *testing.T) {
	names, batches, err := batch([]string{"apple", "banana", "avocado"})
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(names, []string{"top-a", "top-b"}) {
		t.Fatalf("names: %v", names)
	}
	if !reflect.DeepEqual(batches, [][]string{{"apple", "avocado"}, {"banana"}}) {
		t.Fatalf("batches: %v", batches)
	}
}

// TestLargeSectionSplitsBySubSection checks that a section of more than 40 keys splits once by its
// second path component, keeping direct leaves in `<section>`, and that `top-<c>` never splits.
func TestLargeSectionSplitsBySubSection(t *testing.T) {
	var keys []string
	for i := 0; i < 20; i++ {
		keys = append(keys, fmt.Sprintf("big.leaf%02d", i), fmt.Sprintf("big.sub_a.k%02d", i))
	}
	keys = append(keys, "big.sub_b.x.y")
	for i := 0; i < 45; i++ {
		keys = append(keys, fmt.Sprintf("t%02d", i))
	}
	keys = append(keys, "small.a.b")
	names, batches, err := batch(keys)
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(names, []string{"big", "big-sub-a", "big-sub-b", "small", "top-t"}) {
		t.Fatalf("names: %v", names)
	}
	if len(batches[0]) != 20 || len(batches[1]) != 20 || len(batches[2]) != 1 || len(batches[4]) != 45 {
		t.Fatalf("batch sizes: %d %d %d %d", len(batches[0]), len(batches[1]), len(batches[2]), len(batches[4]))
	}
}

func TestInventoriedAndExcludedFails(t *testing.T) {
	s, _ := schema.Parse([]byte(testSchema))
	o, _ := ParseOverlay([]byte(testOverlay))
	o.Excluded["port"] = true
	if _, err := Generate(s, o, testFacts()); err == nil || !strings.Contains(err.Error(), "port") {
		t.Fatalf("got %v, want an inventoried-and-excluded failure naming port", err)
	}
}

func TestNumberValuesKeepFraction(t *testing.T) {
	for _, d := range []interface{}{nil, 0, 1, 0.5, 2.25, -0.5} {
		v, err := ValuesFor(&schema.Key{Path: "n", Type: "number", Default: d})
		if err != nil {
			t.Fatal(err)
		}
		for _, x := range []interface{}{v.Input, v.Set} {
			f := x.(float64)
			if f == math.Trunc(f) {
				t.Errorf("default %v: value %v has no fraction", d, f)
			}
		}
	}
}

func TestSanitizeLowercases(t *testing.T) {
	if got := record.Sanitize("Apm_Config.X"); got != "apm-config-x" {
		t.Fatalf("sanitize: %q", got)
	}
}
