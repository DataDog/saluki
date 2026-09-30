// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package gen

import (
	"reflect"
	"sort"
	"strings"
	"testing"

	"github.com/DataDog/datadog-agent/cmd/config-recorder/record"
	"github.com/DataDog/datadog-agent/cmd/config-recorder/schema"
)

func testGenerator(t *testing.T, facts *AgentFacts) (*generator, []string) {
	t.Helper()
	s, err := schema.Parse([]byte(testSchema))
	if err != nil {
		t.Fatal(err)
	}
	leaves, err := s.LowercasedLeaves()
	if err != nil {
		t.Fatal(err)
	}
	g := &generator{leaves: leaves, facts: facts, envBindings: s.EnvBindings(), names: map[string]bool{}}
	modeled := []string{"apm_config.a", "apm_config.b", "apm_config.c", "ends", "flag", "name", "port", "quiet",
		"rate", "sock", "tags", "wait"}
	return g, modeled
}

func TestDepthClasses(t *testing.T) {
	g, modeled := testGenerator(t, testFacts())
	byType := []depthClass{
		{Type: "<nil>:string", Rep: "quiet"},
		{Type: "[]string", Rep: "tags"},
		{Type: "bool", Rep: "flag"},
		{Type: "float64", Rep: "rate"},
		{Type: "int", Rep: "apm_config.a"},
		{Type: "map[string][]string", Rep: "ends"},
		{Type: "string", Rep: "name"},
		{Type: "time.Duration", Rep: "wait"},
	}
	// quiet has no env binding, so its class has no env representative.
	byEnv := []depthClass{
		{Type: "[]string", EnvParser: "comma_separated", Rep: "tags"},
		{Type: "bool", Rep: "flag"},
		{Type: "float64", Rep: "rate"},
		{Type: "int", Rep: "apm_config.a"},
		{Type: "map[string][]string", Rep: "ends"},
		{Type: "string", Rep: "name"},
		{Type: "time.Duration", Rep: "wait"},
	}
	for source, want := range map[depthSource][]depthClass{sourceYAML: byType, sourceSet: byType, sourceEnv: byEnv} {
		classes, err := g.depthClasses(modeled, source)
		if err != nil {
			t.Fatal(err)
		}
		if !reflect.DeepEqual(classes, want) {
			t.Errorf("%s classes:\n got %+v\nwant %+v", source, classes, want)
		}
	}

	// The env parser splits env classes only: with its own parser, sock gets an env class of its
	// own, while the YAML string class keeps one representative.
	g.leaves["sock"].EnvParser = "space_separated"
	yamlClasses, err := g.depthClasses(modeled, sourceYAML)
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(yamlClasses, byType) {
		t.Errorf("yaml classes with a second string parser:\n got %+v\nwant %+v", yamlClasses, byType)
	}
	envClasses, err := g.depthClasses(modeled, sourceEnv)
	if err != nil {
		t.Fatal(err)
	}
	var strs []depthClass
	for _, c := range envClasses {
		if c.Type == "string" {
			strs = append(strs, c)
		}
	}
	wantStrs := []depthClass{{Type: "string", Rep: "name"}, {Type: "string", EnvParser: "space_separated", Rep: "sock"}}
	if !reflect.DeepEqual(strs, wantStrs) {
		t.Errorf("env string classes: got %+v, want %+v", strs, wantStrs)
	}

	// The env representative is the byte-first key of the class with an env binding, which need not
	// be the YAML representative.
	f := testFacts()
	f.DefaultType["name"] = "<nil>"
	f.DefaultType["quiet"] = "string"
	g, _ = testGenerator(t, f)
	for source, want := range map[depthSource]string{sourceYAML: "quiet", sourceEnv: "sock"} {
		classes, err := g.depthClasses(modeled, source)
		if err != nil {
			t.Fatal(err)
		}
		for _, c := range classes {
			if c.Type == "string" && c.Rep != want {
				t.Errorf("%s string class: got %+v, want rep %s", source, c, want)
			}
		}
	}

	delete(f.DefaultType, "rate")
	g, _ = testGenerator(t, f)
	if _, err := g.depthClasses(modeled, sourceYAML); err == nil || !strings.Contains(err.Error(), "rate") {
		t.Errorf("got %v, want a missing default-layer type failure naming rate", err)
	}
}

// TestDepthVariantKinds checks that each variant's case holds exactly the representatives of the
// classes it applies to, and that a class with no env binding gets no env variants.
func TestDepthVariantKinds(t *testing.T) {
	facts := testFacts()
	r := mustGenerate(t, facts)
	cases := byName(r)
	g, modeled := testGenerator(t, facts)
	for _, v := range depthVariants {
		classes, err := g.depthClasses(modeled, v.Source)
		if err != nil {
			t.Fatal(err)
		}
		var want []string
		for _, cl := range classes {
			if v.applies(cl.Type) {
				want = append(want, cl.Rep)
			}
		}
		c := cases["depth-"+v.Name]
		if len(want) == 0 {
			if c != nil {
				t.Errorf("%s: want no case, got one", v.Name)
			}
			continue
		}
		if c == nil {
			t.Errorf("%s: no case", v.Name)
			continue
		}
		var got []string
		for _, k := range c.Keys {
			got = append(got, k.Key)
		}
		sort.Strings(want)
		if !reflect.DeepEqual(got, want) {
			t.Errorf("%s keys: got %v, want %v", v.Name, got, want)
		}
		if c.Group != record.GroupDepth || !strings.HasPrefix(c.Name, "depth-"+v.Source.String()+"-") {
			t.Errorf("%s: group %s, name %s", v.Name, c.Group, c.Name)
		}
		hasEnv, hasYAML, hasSet := len(c.Env) > 0, c.YAML != nil, len(c.Updates) > 0
		if hasEnv != (v.Source == sourceEnv) || hasYAML != (v.Source == sourceYAML) || hasSet != (v.Source == sourceSet) {
			t.Errorf("%s: inputs env=%v yaml=%v set=%v", v.Name, hasEnv, hasYAML, hasSet)
		}
	}

	for name, want := range map[string]map[string]string{
		"depth-env-hex":            {"DD_APM_A": "0x10"},
		"depth-env-float":          {"DD_APM_A": "1.5"},
		"depth-env-bool-words":     {"DD_FLAG": "on"},
		"depth-env-exponent":       {"DD_RATE": "1e3"},
		"depth-env-not-json":       {"DD_ENDS": "cr_a:cr-a"},
		"depth-env-secondary-name": {"DD_A": "1"},
	} {
		if got := cases[name].Env; !reflect.DeepEqual(got, want) {
			t.Errorf("%s env: got %v, want %v", name, got, want)
		}
	}
	for name, want := range map[string]string{
		"depth-yaml-float":          "apm_config:\n    a: 1.5\n",
		"depth-yaml-int":            "rate: 3\nwait: 30\n",
		"depth-yaml-map-int-values": "ends:\n    cr_a: 1\n",
		"depth-yaml-null":           "~",
		"depth-yaml-quoted":         `name: "cr-c"`,
		"depth-yaml-one-item-list":  "ends:\n    - cr-a\n",
	} {
		if y := cases[name].YAML; y == nil || !strings.Contains(*y, want) {
			t.Errorf("%s yaml: got %v, want it to contain %q", name, y, want)
		}
	}
	for name, want := range map[string]record.Update{
		"depth-set-json-string": {Op: "set", Key: "ends", Value: `{"cr_b":["cr-b"]}`, Source: "agent-runtime"},
		"depth-set-string":      {Op: "set", Key: "apm_config.a", Value: "2", Source: "agent-runtime"},
		"depth-set-bool-words":  {Op: "set", Key: "flag", Value: "on", Source: "agent-runtime"},
	} {
		if !containsUpdate(cases[name].Updates, want) {
			t.Errorf("%s updates: got %v, want %v among them", name, cases[name].Updates, want)
		}
	}

	getters := map[string][]string{}
	for _, k := range cases["depth-yaml-empty-list"].Keys {
		getters[k.Key] = k.Getters
	}
	for k, want := range map[string][]string{
		"tags":  {"GetStringSlice", "Get"},
		"ends":  {"GetStringMapStringSlice", "Get"},
		"quiet": {"Get", "GetString"},
		"flag":  {"GetBool", "Get"},
	} {
		if !reflect.DeepEqual(getters[k], want) {
			t.Errorf("%s getters: got %v, want %v", k, getters[k], want)
		}
	}
}

func TestDepthDeterministicNames(t *testing.T) {
	names := func() []string {
		var out []string
		for _, c := range mustGenerate(t, testFacts()).Cases {
			if c.Group == record.GroupDepth {
				out = append(out, c.Name)
			}
		}
		return out
	}
	a, b := names(), names()
	if !reflect.DeepEqual(a, b) || len(a) == 0 {
		t.Fatalf("depth names differ between runs or are empty:\n%v\n%v", a, b)
	}
}

// TestDepthVariantTable checks the variant table's order and the kinds its collection-only rows
// cover: a YAML null and an empty env var are ignored for every known key, so they cover only lists
// and maps.
func TestDepthVariantTable(t *testing.T) {
	var names []string
	byName := map[string]depthVariant{}
	for _, v := range depthVariants {
		names = append(names, v.Name)
		byName[v.Name] = v
		if !strings.HasPrefix(v.Name, v.Source.String()+"-") {
			t.Errorf("%s: source %s", v.Name, v.Source)
		}
		if (v.Source == sourceEnv) != (v.envValue != nil) || (v.Source == sourceEnv) == (v.value != nil) {
			t.Errorf("%s: a %s variant needs exactly its own value function", v.Name, v.Source)
		}
	}
	if len(names) != 24 || names[0] != "yaml-empty-list" || names[14] != "yaml-map-int-values" ||
		names[23] != "set-string" {
		t.Errorf("variant table: %v", names)
	}
	for _, name := range []string{"yaml-null", "env-empty", "set-empty-list"} {
		v := byName[name]
		if !v.applies("[]string") || !v.applies("map[string]string") || v.applies("int") || v.applies("string") {
			t.Errorf("%s must apply to the list and map kinds only", name)
		}
	}
	if v := byName["yaml-int"]; !v.applies("float64") || !v.applies("time.Duration") || v.applies("int") {
		t.Error("yaml-int must apply to float64 and time.Duration only")
	}
}

func containsUpdate(us []record.Update, want record.Update) bool {
	for _, u := range us {
		if reflect.DeepEqual(u, want) {
			return true
		}
	}
	return false
}
