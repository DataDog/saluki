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
	// The synthetic schema's representatives, pinned the way depth_reps.go pins the real one.
	g.depthReps, err = g.deriveDepthReps(modeled)
	if err != nil {
		t.Fatal(err)
	}
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
	// The pin follows the schema change, the way depth_reps.go is re-reviewed after one; the
	// classes below then hold the re-pinned (here: byte-first) representatives.
	pin, err := g.deriveDepthReps(modeled)
	if err != nil {
		t.Fatal(err)
	}
	g.depthReps = pin
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
	// The pin is not derived here: the missing default-layer type must surface from the class
	// scan itself, before any representative is chosen.
	s, _ := schema.Parse([]byte(testSchema))
	leaves, _ := s.LowercasedLeaves()
	g = &generator{leaves: leaves, facts: f, envBindings: s.EnvBindings(),
		depthReps: map[depthRepKey]string{}, names: map[string]bool{}}
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

// pristinePin derives the synthetic schema's pin as it stands before the change under test —
// the role pinnedDepthReps plays across a schema bump.
func pristinePin(t *testing.T) map[depthRepKey]string {
	t.Helper()
	g, _ := testGenerator(t, testFacts())
	return g.depthReps
}

// generateBump generates a changed synthetic schema or overlay against the pristine pin, with
// facts extended for any keys the change adds. It fails the test only on setup errors: the
// generation error is the assertion.
func generateBump(t *testing.T, facts *AgentFacts, schemaText, overlayText string) (*Result, error) {
	t.Helper()
	s, err := schema.Parse([]byte(schemaText))
	if err != nil {
		t.Fatal(err)
	}
	o, err := ParseOverlay([]byte(overlayText))
	if err != nil {
		t.Fatal(err)
	}
	return generate(s, o, facts, pristinePin(t))
}

// TestDepthRepsPinned checks that the pin, not byte order, decides a class's representative: a
// key added ahead of the pinned one moves nothing, while an unmodeled or retyped
// representative, a new class and a stale pin entry all fail naming what to do.
func TestDepthRepsPinned(t *testing.T) {
	// An unrelated string key sorting before "name": without the pin it would take over the
	// string class, and its variants' rows would silently switch settings.
	withKey := strings.Replace(testSchema, "  flag:",
		"  aaa: {node_type: setting, type: string, default: zzz}\n  flag:", 1)
	withSupport := strings.Replace(testOverlay, "  flag: {support: full}",
		"  aaa: {support: full}\n  flag: {support: full}", 1)
	f := testFacts()
	f.DefaultType["aaa"] = "string"
	f.Getters["aaa"] = []string{"GetString"}
	f.EnvVars["DD_AAA"] = true
	r, err := generateBump(t, f, withKey, withSupport)
	if err != nil {
		t.Fatalf("an unrelated earlier-sorting key must not fail generation: %v", err)
	}
	for _, c := range r.Cases {
		if c.Group != record.GroupDepth {
			continue
		}
		for _, k := range c.Keys {
			if k.Key == "aaa" {
				t.Errorf("case %s: %q became a depth representative; the pin keeps the class on \"name\"", c.Name, k.Key)
			}
		}
	}
	var quoted []string
	for _, k := range byName(r)["depth-yaml-quoted"].Keys {
		quoted = append(quoted, k.Key)
	}
	if !contains(quoted, "name") {
		t.Errorf("depth-yaml-quoted keys: got %v, want the pinned \"name\" among them", quoted)
	}

	// An unmodeled representative: the pinned key leaves the modeled set.
	withoutName := strings.Replace(testOverlay, "  name: {support: full}\n", "", 1)
	_, err = generateBump(t, testFacts(), testSchema, withoutName)
	if err == nil || !strings.Contains(err.Error(), `pinned representative "name"`) ||
		!strings.Contains(err.Error(), "depth_reps.go") {
		t.Errorf("unmodeled rep: got %v, want a failure naming the pinned key and the pin file", err)
	}

	// A retyped representative: the pinned key moves to another class.
	retyped := testFacts()
	retyped.DefaultType["name"] = "int"
	_, err = generateBump(t, retyped, testSchema, testOverlay)
	if err == nil || !strings.Contains(err.Error(), `pinned representative "name" no longer belongs`) {
		t.Errorf("retyped rep: got %v, want a failure naming the pinned key as no longer in its class", err)
	}

	// A new class: no pin entry exists for its type.
	withInt32 := strings.Replace(testSchema, "  flag:",
		"  zzz: {node_type: setting, type: integer, default: 7}\n  flag:", 1)
	withInt32Support := strings.Replace(testOverlay, "  flag: {support: full}",
		"  zzz: {support: full}\n  flag: {support: full}", 1)
	f2 := testFacts()
	f2.DefaultType["zzz"] = "int32"
	f2.Getters["zzz"] = []string{"GetInt32"}
	f2.EnvVars["DD_ZZZ"] = true
	_, err = generateBump(t, f2, withInt32, withInt32Support)
	if err == nil || !strings.Contains(err.Error(), "no pinned representative") ||
		!strings.Contains(err.Error(), "int32") || !strings.Contains(err.Error(), `"zzz"`) {
		t.Errorf("new class: got %v, want a failure naming the class and its byte-first key", err)
	}

	// A stale pin entry: the class's only key is unmodeled, so the class no longer exists.
	withoutEnds := strings.Replace(testOverlay, "  ends: {support: full}\n", "", 1)
	_, err = generateBump(t, testFacts(), testSchema, withoutEnds)
	if err == nil || !strings.Contains(err.Error(), "map[string][]string") ||
		!strings.Contains(err.Error(), "remove the entry") {
		t.Errorf("stale pin: got %v, want a failure naming the class and asking for removal", err)
	}
}

func contains(xs []string, want string) bool {
	for _, x := range xs {
		if x == want {
			return true
		}
	}
	return false
}

func containsUpdate(us []record.Update, want record.Update) bool {
	for _, u := range us {
		if reflect.DeepEqual(u, want) {
			return true
		}
	}
	return false
}
