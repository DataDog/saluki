// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package gen

import (
	"fmt"
	"math"
	"slices"
	"sort"
	"strings"

	"gopkg.in/yaml.v3"

	"github.com/DataDog/datadog-agent/cmd/config-recorder/record"
	"github.com/DataDog/datadog-agent/cmd/config-recorder/schema"
)

// Kinds of a class's default-layer Go type (case.md §3.2.1).
const (
	kindList   = "list"
	kindMap    = "map"
	kindScalar = "scalar"
)

// mixedString is the depth text that mixes the space and comma list separators.
const mixedString = "cr-a cr-b,cr-c"

// depthClass is one class of modeled keys (case.md §3.2.1). For YAML and `set` variants a class is
// a default-layer Go type; for env variants it is that type and the schema env parser.
type depthClass struct {
	// Type is the default-layer `%T`, or `<nil>:<schema type>` for a key with no default.
	Type string
	// EnvParser is the schema's `env_parser`, "" when it has none or for a YAML and `set` class.
	EnvParser string
	// Rep is the class's byte-first modeled key; for an env class, its byte-first env-bound key.
	Rep string
}

// kind is the class type's kind: `list` for `[]…`, `map` for `map[…]…`, otherwise `scalar`.
func kind(typ string) string {
	switch {
	case strings.HasPrefix(typ, "[]"):
		return kindList
	case strings.HasPrefix(typ, "map["):
		return kindMap
	}
	return kindScalar
}

// depthSource is the one input a depth variant writes.
type depthSource int

const (
	sourceYAML depthSource = iota
	sourceEnv
	sourceSet
)

// String is the source as it appears in a depth case name.
func (s depthSource) String() string {
	switch s {
	case sourceYAML:
		return "yaml"
	case sourceEnv:
		return "env"
	case sourceSet:
		return "set"
	}
	return fmt.Sprintf("depthSource(%d)", int(s))
}

// depthVariant is one depth input shape, from the table of case.md §3.2.1. applies says whether it
// covers a class type. A YAML or `set` variant's value is its input for one key, from the key's
// schema entry, breadth values and class type; an env variant's envValue is its env text.
type depthVariant struct {
	Name     string
	Source   depthSource
	applies  func(typ string) bool
	value    func(s *schema.Key, v Values, typ string) (interface{}, error)
	envValue func(s *schema.Key, v Values) (string, error)
}

func yamlVariant(name string, applies func(string) bool, value func(*schema.Key, Values, string) (interface{}, error)) depthVariant {
	return depthVariant{Name: name, Source: sourceYAML, applies: applies, value: value}
}

func setVariant(name string, applies func(string) bool, value func(*schema.Key, Values, string) (interface{}, error)) depthVariant {
	return depthVariant{Name: name, Source: sourceSet, applies: applies, value: value}
}

func envVariant(name string, applies func(string) bool, value func(*schema.Key, Values) (string, error)) depthVariant {
	return depthVariant{Name: name, Source: sourceEnv, applies: applies, envValue: value}
}

func allKinds(string) bool { return true }

func kinds(ks ...string) func(string) bool {
	return func(typ string) bool { return slices.Contains(ks, kind(typ)) }
}

func types(ts ...string) func(string) bool {
	return func(typ string) bool { return slices.Contains(ts, typ) }
}

func fixed(x interface{}) func(*schema.Key, Values, string) (interface{}, error) {
	return func(*schema.Key, Values, string) (interface{}, error) { return x, nil }
}

func envFixed(text string) func(*schema.Key, Values) (string, error) {
	return func(*schema.Key, Values) (string, error) { return text, nil }
}

// nullNode is YAML `~`: the encoder writes a nil value as `null`, so the spelling is set here.
func nullNode() *yaml.Node { return &yaml.Node{Kind: yaml.ScalarNode, Tag: "!!null", Value: "~"} }

func numberOf(s *schema.Key, v interface{}) (float64, error) {
	switch x := v.(type) {
	case int:
		return float64(x), nil
	case float64:
		return x, nil
	}
	return 0, fmt.Errorf("key %q: breadth value %v is not a number", strings.ToLower(s.Path), v)
}

// depthVariants lists every depth variant except the secondary-name variant, in the priority order
// of the case.md §3.2.1 table. To meet the depth budget, remove variants from the bottom.
var depthVariants = []depthVariant{
	yamlVariant("yaml-empty-list", allKinds, fixed([]interface{}{})),
	yamlVariant("yaml-empty-map", allKinds, fixed(map[string]interface{}{})),
	yamlVariant("yaml-null", kinds(kindList, kindMap), func(*schema.Key, Values, string) (interface{}, error) {
		return nullNode(), nil
	}),
	envVariant("env-empty", kinds(kindList, kindMap), envFixed("")),
	setVariant("set-empty-list", kinds(kindList, kindMap), fixed([]interface{}{})),
	yamlVariant("yaml-mixed-string", kinds(kindList, kindMap), fixed(mixedString)),
	setVariant("set-mixed-string", kinds(kindList), fixed(mixedString)),
	envVariant("env-mixed-string", kinds(kindList), envFixed(mixedString)),
	envVariant("env-json-list", kinds(kindList), envFixed(`["cr-a","cr-b"]`)),
	envVariant("env-bool-words", types("bool"), envFixed("on")),
	setVariant("set-bool-words", types("bool"), fixed("on")),
	setVariant("set-empty-string", types("bool", "int", "float64"), fixed("")),
	envVariant("env-not-json", kinds(kindMap), envFixed("cr_a:cr-a")),
	setVariant("set-json-string", kinds(kindMap), func(_ *schema.Key, v Values, _ string) (interface{}, error) {
		return jsonText(v.Set)
	}),
	yamlVariant("yaml-map-int-values", kinds(kindMap), fixed(map[string]interface{}{"cr_a": 1})),
	yamlVariant("yaml-float", types("int", "int64"), func(s *schema.Key, v Values, _ string) (interface{}, error) {
		f, err := numberOf(s, v.Input)
		return f + 0.5, err
	}),
	envVariant("env-float", types("int", "int64"), func(s *schema.Key, v Values) (string, error) {
		f, err := numberOf(s, v.Input)
		if err != nil {
			return "", err
		}
		return EnvText(s, f+0.5)
	}),
	envVariant("env-hex", types("int", "int64"), envFixed("0x10")),
	yamlVariant("yaml-int", types("float64", "time.Duration"), func(s *schema.Key, v Values, typ string) (interface{}, error) {
		if typ == "time.Duration" {
			return 30, nil
		}
		f, err := numberOf(s, v.Input)
		return int(math.Floor(f)) + 1, err
	}),
	envVariant("env-exponent", types("float64"), envFixed("1e3")),
	envVariant("env-bool-digit", types("bool"), envFixed("1")),
	yamlVariant("yaml-quoted", kinds(kindScalar), func(_ *schema.Key, v Values, _ string) (interface{}, error) {
		return &yaml.Node{Kind: yaml.ScalarNode, Tag: "!!str", Style: yaml.DoubleQuotedStyle,
			Value: fmt.Sprintf("%v", v.Input)}, nil
	}),
	yamlVariant("yaml-one-item-list", kinds(kindScalar, kindMap), func(_ *schema.Key, v Values, typ string) (interface{}, error) {
		if kind(typ) == kindMap {
			return []interface{}{"cr-a"}, nil
		}
		return []interface{}{v.Input}, nil
	}),
	setVariant("set-string", kinds(kindScalar), func(_ *schema.Key, v Values, _ string) (interface{}, error) {
		return fmt.Sprintf("%v", v.Set), nil
	}),
}

// secondaryNameVariant is the case of keys set only by their last schema env name.
const secondaryNameVariant = "env-secondary-name"

// classType is the class type of a key whose default-layer `%T` is typ: typ itself, or
// `<nil>:<schema type>` for a key with no default.
func classType(s *schema.Key, typ string) string {
	if typ == "<nil>" {
		return "<nil>:" + s.Type
	}
	return typ
}

// depthClasses groups modeled keys into the classes of variants from source and picks each
// class's representative, sorted by class type then env parser. Env classes are split by env
// parser, and a class with no env-bound key is left out; YAML and `set` classes are by type only.
func (g *generator) depthClasses(modeled []string, source depthSource) ([]depthClass, error) {
	byClass := map[[2]string]*depthClass{}
	var order [][2]string
	sorted := append([]string(nil), modeled...)
	sort.Strings(sorted)
	for _, k := range sorted {
		s := g.leaves[k]
		typ, ok := g.facts.DefaultType[k]
		if !ok {
			return nil, fmt.Errorf("key %q: no default-layer type from the Agent", k)
		}
		id := [2]string{classType(s, typ), ""}
		if source == sourceEnv {
			_, bound, err := g.envName(s)
			if err != nil {
				return nil, err
			}
			if !bound {
				continue
			}
			id[1] = s.EnvParser
		}
		if _, seen := byClass[id]; !seen {
			byClass[id] = &depthClass{Type: id[0], EnvParser: id[1], Rep: k}
			order = append(order, id)
		}
	}
	sort.Slice(order, func(i, j int) bool {
		if order[i][0] != order[j][0] {
			return order[i][0] < order[j][0]
		}
		return order[i][1] < order[j][1]
	})
	out := make([]depthClass, len(order))
	for i, id := range order {
		out[i] = *byClass[id]
	}
	return out, nil
}

// depthEntry is a depth key entry: the key's default getters, then `Get` if missing (case.md
// §3.2.1).
func (g *generator) depthEntry(k string) (record.KeyEntry, error) {
	getters, ok := g.facts.Getters[k]
	if !ok || len(getters) == 0 {
		return record.KeyEntry{}, fmt.Errorf("key %q: no default getters from the Agent", k)
	}
	list := append([]string(nil), getters...)
	if !slices.Contains(list, "Get") {
		list = append(list, "Get")
	}
	return record.KeyEntry{Key: k, Getters: list}, nil
}

// depthCase builds one depth case from its keys and each YAML or `set` key's input value. An env
// case's caller sets its env.
func (g *generator) depthCase(name string, source depthSource, keys []string, values map[string]interface{}) (*record.Case, error) {
	sort.Strings(keys)
	c := &record.Case{Name: name, Group: record.GroupDepth, Why: []string{}}
	yamlValues := map[string]interface{}{}
	for _, k := range keys {
		e, err := g.depthEntry(k)
		if err != nil {
			return nil, err
		}
		c.Keys = append(c.Keys, e)
		switch source {
		case sourceEnv:
		case sourceYAML:
			yamlValues[k] = values[k]
		case sourceSet:
			c.Updates = append(c.Updates, record.Update{Op: "set", Key: k, Value: values[k], Source: setSource})
		}
	}
	if source == sourceYAML {
		text, err := yamlText(yamlValues)
		if err != nil {
			return nil, fmt.Errorf("case %q: %w", name, err)
		}
		c.YAML = &text
	}
	return c, nil
}

// depthGroup adds the `depth` cases (case.md §3.2.1): one case per variant, holding every
// class's representative the variant applies to, and the secondary env name case.
func (g *generator) depthGroup(modeled []string) error {
	classes := map[depthSource][]depthClass{}
	for _, source := range []depthSource{sourceYAML, sourceEnv, sourceSet} {
		cs, err := g.depthClasses(modeled, source)
		if err != nil {
			return err
		}
		classes[source] = cs
	}
	for _, v := range depthVariants {
		values := map[string]interface{}{}
		env := map[string]string{}
		var keys []string
		for _, cl := range classes[v.Source] {
			if !v.applies(cl.Type) {
				continue
			}
			k := cl.Rep
			s := g.leaves[k]
			bv, err := ValuesFor(s)
			if err != nil {
				return err
			}
			if v.Source == sourceEnv {
				text, err := v.envValue(s, bv)
				if err != nil {
					return err
				}
				name, _, err := g.envName(s)
				if err != nil {
					return err
				}
				if _, dup := env[name]; dup {
					return fmt.Errorf("depth variant %s: env var %s is bound by two keys", v.Name, name)
				}
				env[name] = text
			} else {
				x, err := v.value(s, bv, cl.Type)
				if err != nil {
					return err
				}
				values[k] = x
			}
			keys = append(keys, k)
		}
		if len(keys) == 0 {
			continue
		}
		c, err := g.depthCase("depth-"+v.Name, v.Source, keys, values)
		if err != nil {
			return err
		}
		if v.Source == sourceEnv {
			c.Env = env
		}
		if err := g.add(c); err != nil {
			return err
		}
	}
	return g.secondaryNames(modeled)
}

// secondaryNames adds `depth-env-secondary-name`: every modeled key whose schema lists more than
// one env name, set only by its last name to its breadth Input value.
func (g *generator) secondaryNames(modeled []string) error {
	env := map[string]string{}
	var keys []string
	for _, k := range modeled {
		s := g.leaves[k]
		if len(s.EnvVars) < 2 {
			continue
		}
		name := s.EnvVars[len(s.EnvVars)-1]
		if !slices.Contains(g.envBindings[k], name) || !g.facts.EnvVars[name] {
			return fmt.Errorf("key %q: env var %s is not in the Agent's GetEnvVars()", k, name)
		}
		if _, dup := env[name]; dup {
			return fmt.Errorf("depth variant %s: env var %s is bound by two keys", secondaryNameVariant, name)
		}
		bv, err := ValuesFor(s)
		if err != nil {
			return err
		}
		text, err := EnvText(s, bv.Input)
		if err != nil {
			return err
		}
		env[name] = text
		keys = append(keys, k)
	}
	if len(keys) == 0 {
		return nil
	}
	c, err := g.depthCase("depth-"+secondaryNameVariant, sourceEnv, keys, nil)
	if err != nil {
		return err
	}
	c.Env = env
	return g.add(c)
}
