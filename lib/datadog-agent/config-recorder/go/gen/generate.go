// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package gen

import (
	"bytes"
	"fmt"
	"regexp"
	"slices"
	"sort"
	"strings"
	"unicode/utf8"

	"gopkg.in/yaml.v3"

	"github.com/DataDog/datadog-agent/cmd/config-recorder/record"
	"github.com/DataDog/datadog-agent/cmd/config-recorder/schema"
)

// Fixed names for the unknown group's unknown keys.
const (
	unknownName   = "config_recorder_unknown"
	unknownEnvVar = "DD_CONFIG_RECORDER_UNKNOWN"
	// setSource is the layer a breadth case's `set` updates write.
	setSource = "agent-runtime"
)

// Skip is a key left out of one source of a group, with the reason.
type Skip struct {
	Group, Source, Key, Reason string
}

// Result is every generated case, sorted by name, and the keys skipped.
type Result struct {
	Cases   []*record.Case
	Skipped []Skip
}

var namePattern = regexp.MustCompile(`^[a-z0-9][a-z0-9-]*$`)

// splitThreshold is the most keys a top-level section's batch holds before it is split by its
// second path component (case.md §3.2).
const splitThreshold = 40

// section is the key's top-level batch: its first path component, sanitized, for a key with a
// dot; for a top-level leaf (a key with no dot), `top-<c>` where `<c>` is the key's first
// character, sanitized (case.md §3.2).
func section(key string) string {
	first, _, found := strings.Cut(key, ".")
	if !found {
		r, _ := utf8.DecodeRuneInString(first)
		return "top-" + record.Sanitize(string(r))
	}
	return record.Sanitize(first)
}

// subSection is the batch of a key in a split section: `<section>` for the section's direct
// leaves, `<section>-<sub>` for a key under the sub-section `<sub>`.
func subSection(key string) string {
	parts := strings.SplitN(key, ".", 3)
	if len(parts) < 3 {
		return section(key)
	}
	return section(key) + "-" + record.Sanitize(parts[1])
}

// rawSection is the unsanitized path prefix that section(key) derives its batch name from: the
// raw first path component for a key with a dot, or the raw first character for a top-level
// leaf.
func rawSection(key string) string {
	first, _, found := strings.Cut(key, ".")
	if !found {
		r, _ := utf8.DecodeRuneInString(first)
		return string(r)
	}
	return first
}

// rawSubSection is the unsanitized path prefix that subSection(key) derives its batch name from:
// the raw first two path components, joined by ".", for a key under a sub-section; rawSection's
// prefix for a section's direct leaf.
func rawSubSection(key string) string {
	parts := strings.SplitN(key, ".", 3)
	if len(parts) < 3 {
		return rawSection(key)
	}
	return parts[0] + "." + parts[1]
}

// batch splits keys by top-level section; a section with more than splitThreshold keys, other
// than a `top-<c>` batch, is split once more by subSection. A batch name depends only on the key
// paths. It returns each batch's name suffix and keys, both in byte order.
//
// Batch names are keyed only by their sanitized form, so two different raw prefixes that sanitize
// alike (case.md §3.2), or a sub-section split name that equals another section's name, would
// otherwise merge silently. batch tracks the raw prefix behind each name and fails if a name is
// reached by two different raw prefixes.
func batch(keys []string) ([]string, [][]string, error) {
	sorted := append([]string(nil), keys...)
	sort.Strings(sorted)
	bySection := map[string][]string{}
	sectionRaw := map[string]string{}
	for _, k := range sorted {
		name := section(k)
		raw := rawSection(k)
		if prev, ok := sectionRaw[name]; ok && prev != raw {
			return nil, nil, fmt.Errorf("batch name %q is reached by two different raw prefixes: %q and %q", name, prev, raw)
		}
		sectionRaw[name] = raw
		bySection[name] = append(bySection[name], k)
	}
	byName := map[string][]string{}
	nameRaw := map[string]string{}
	for s, ks := range bySection {
		if len(ks) <= splitThreshold || strings.HasPrefix(s, "top-") {
			if prev, ok := nameRaw[s]; ok && prev != sectionRaw[s] {
				return nil, nil, fmt.Errorf("batch name %q is reached by two different raw prefixes: %q and %q", s, prev, sectionRaw[s])
			}
			nameRaw[s] = sectionRaw[s]
			byName[s] = append(byName[s], ks...)
			continue
		}
		for _, k := range ks {
			subName := subSection(k)
			raw := rawSubSection(k)
			if prev, ok := nameRaw[subName]; ok && prev != raw {
				return nil, nil, fmt.Errorf("batch name %q is reached by two different raw prefixes: %q and %q", subName, prev, raw)
			}
			nameRaw[subName] = raw
			byName[subName] = append(byName[subName], k)
		}
	}
	names := make([]string, 0, len(byName))
	for n := range byName {
		names = append(names, n)
	}
	sort.Strings(names)
	out := make([][]string, len(names))
	for i, n := range names {
		out[i] = byName[n]
		sort.Strings(out[i])
	}
	return names, out, nil
}

// newCase is a case with no inputs, in the form record.ParseCase gives it.
func newCase(name, group string, keys []string) *record.Case {
	c := &record.Case{Name: name, Group: record.Group(group), Why: []string{}}
	for _, k := range keys {
		c.Keys = append(c.Keys, record.KeyEntry{Key: k})
	}
	return c
}

func sortedKeys[V any](m map[string]V) []string {
	out := make([]string, 0, len(m))
	for k := range m {
		out = append(out, k)
	}
	sort.Strings(out)
	return out
}

type generator struct {
	// leaves is every schema leaf by lowercased path.
	leaves map[string]*schema.Key
	facts  *AgentFacts
	// envBindings is schema.EnvBindings: every leaf's bound env names, by lowercased path.
	envBindings map[string][]string
	res         Result
	names       map[string]bool
}

func (g *generator) add(c *record.Case) error {
	if !namePattern.MatchString(c.Name) {
		return fmt.Errorf("generated case name %q does not match %s", c.Name, namePattern)
	}
	if g.names[c.Name] {
		return fmt.Errorf("generated case name %q is used twice", c.Name)
	}
	g.names[c.Name] = true
	g.res.Cases = append(g.res.Cases, c)
	return nil
}

// envName is the key's env var, or none for a `no-env` key: the first schema `env_vars` entry,
// else the derived name (schema.DerivedEnvName). The name must be one the Agent bound; if not, the
// generator's reading of the schema disagrees with the Agent, a harness failure.
func (g *generator) envName(s *schema.Key) (string, bool, error) {
	if s.NoEnv {
		return "", false, nil
	}
	name := schema.DerivedEnvName(s.Path)
	if len(s.EnvVars) > 0 {
		name = s.EnvVars[0]
	}
	if !slices.Contains(g.envBindings[strings.ToLower(s.Path)], name) || !g.facts.EnvVars[name] {
		return "", false, fmt.Errorf("key %q: env var %s is not in the Agent's GetEnvVars()", strings.ToLower(s.Path), name)
	}
	return name, true, nil
}

// yamlText writes the dotted keys' values as nested YAML mappings.
func yamlText(values map[string]interface{}) (string, error) {
	root := map[string]interface{}{}
	keys := make([]string, 0, len(values))
	for k := range values {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	for _, k := range keys {
		parts := strings.Split(k, ".")
		m := root
		for _, p := range parts[:len(parts)-1] {
			next, ok := m[p].(map[string]interface{})
			if !ok {
				if _, taken := m[p]; taken {
					return "", fmt.Errorf("yaml: %q is both a value and a mapping", p)
				}
				next = map[string]interface{}{}
				m[p] = next
			}
			m = next
		}
		if _, taken := m[parts[len(parts)-1]]; taken {
			return "", fmt.Errorf("yaml: key %q is written twice", k)
		}
		m[parts[len(parts)-1]] = values[k]
	}
	var buf bytes.Buffer
	enc := yaml.NewEncoder(&buf)
	if err := enc.Encode(root); err != nil {
		return "", err
	}
	if err := enc.Close(); err != nil {
		return "", err
	}
	return buf.String(), nil
}

// envAndYAML adds a group's env and YAML cases for schema keys; withSet adds the breadth case's
// `set` updates to the YAML cases.
func (g *generator) envAndYAML(group string, keys []string, withSet bool) error {
	var envKeys []string
	for _, k := range keys {
		_, ok, err := g.envName(g.leaves[k])
		if err != nil {
			return err
		}
		if ok {
			envKeys = append(envKeys, k)
		} else {
			g.res.Skipped = append(g.res.Skipped, Skip{group, "env", k, "no env binding (no-env)"})
		}
	}
	names, batches, err := batch(envKeys)
	if err != nil {
		return err
	}
	for i, ks := range batches {
		c := newCase(group+"-env-"+names[i], group, ks)
		c.Env = map[string]string{}
		for _, k := range ks {
			s := g.leaves[k]
			v, err := ValuesFor(s)
			if err != nil {
				return err
			}
			name, _, _ := g.envName(s)
			text, err := EnvText(s, v.Input)
			if err != nil {
				return err
			}
			if _, dup := c.Env[name]; dup {
				return fmt.Errorf("case %q: env var %s is bound by two keys", c.Name, name)
			}
			c.Env[name] = text
		}
		if err := g.add(c); err != nil {
			return err
		}
	}
	names, batches, err = batch(keys)
	if err != nil {
		return err
	}
	for i, ks := range batches {
		c := newCase(group+"-yaml-"+names[i], group, ks)
		values := map[string]interface{}{}
		for _, k := range ks {
			v, err := ValuesFor(g.leaves[k])
			if err != nil {
				return err
			}
			values[k] = v.Input
			if withSet {
				c.Updates = append(c.Updates, record.Update{Op: "set", Key: k, Value: v.Set, Source: setSource})
			}
		}
		text, err := yamlText(values)
		if err != nil {
			return fmt.Errorf("case %q: %w", c.Name, err)
		}
		c.YAML = &text
		if err := g.add(c); err != nil {
			return err
		}
	}
	return nil
}

// Generate writes the generated groups `baseline`, `breadth`, `unsupported`, `excluded` and
// `unknown`, batched by section (case.md §3.2). Overlay keys that are not schema keys are in no
// generated group; a key the overlay both inventories and excludes is an error.
func Generate(s schema.Schema, overlay *Overlay, facts *AgentFacts) (*Result, error) {
	leaves, err := s.LowercasedLeaves()
	if err != nil {
		return nil, err
	}
	g := &generator{leaves: leaves, facts: facts, envBindings: s.EnvBindings(), names: map[string]bool{}}
	var both []string
	for k := range overlay.Support {
		if overlay.Excluded[k] {
			both = append(both, k)
		}
	}
	if len(both) > 0 {
		sort.Strings(both)
		return nil, fmt.Errorf("overlay keys are both inventoried and excluded; each must be one or the other: %s",
			strings.Join(both, ", "))
	}
	var modeled, unsupported []string
	for k, sup := range overlay.Support {
		if _, ok := leaves[k]; !ok {
			continue
		}
		switch sup {
		case "full", "partial":
			modeled = append(modeled, k)
		default:
			unsupported = append(unsupported, k)
		}
	}
	sort.Strings(modeled)
	sort.Strings(unsupported)

	// baseline: every modeled key in one case, with no inputs.
	if err := g.add(newCase("baseline-default", "baseline", modeled)); err != nil {
		return nil, err
	}
	if err := g.envAndYAML("breadth", modeled, true); err != nil {
		return nil, err
	}
	if err := g.envAndYAML("unsupported", unsupported, false); err != nil {
		return nil, err
	}

	// excluded: per default-layer Go type, the byte-first excluded schema key.
	firstByType := map[string]string{}
	for _, k := range sortedKeys(leaves) {
		if !overlay.Excluded[k] {
			continue
		}
		t, ok := facts.DefaultType[k]
		if !ok {
			return nil, fmt.Errorf("key %q: no default-layer type from the Agent", k)
		}
		if _, seen := firstByType[t]; !seen {
			firstByType[t] = k
		}
	}
	var excluded []string
	for _, k := range firstByType {
		excluded = append(excluded, k)
	}
	sort.Strings(excluded)
	if err := g.envAndYAML("excluded", excluded, false); err != nil {
		return nil, err
	}

	if err := g.unknownGroup(modeled); err != nil {
		return nil, err
	}
	sort.Slice(g.res.Cases, func(i, j int) bool { return g.res.Cases[i].Name < g.res.Cases[j].Name })
	return &g.res, nil
}

// unknownGroup adds the `unknown` cases (case.md §3.3): in YAML, an unknown top-level key, an unknown key under the byte-first modeled section, and the byte-first
// deprecated name of a modeled key; by env, an unknown DD_* variable and that deprecated name.
// Unknown keys take the string rule's value, since the schema gives them no type.
func (g *generator) unknownGroup(modeled []string) error {
	yamlValues := map[string]interface{}{}
	yamlValues[unknownName] = "cr-a"
	for _, k := range modeled {
		if strings.Contains(k, ".") {
			first, _, _ := strings.Cut(k, ".")
			yamlValues[first+"."+unknownName] = "cr-a"
			break
		}
	}
	envKeys := map[string]string{unknownName: unknownEnvVar}
	envValues := map[string]string{unknownName: "cr-a"}

	var deprecated, owner string
	for _, k := range modeled {
		for _, old := range g.leaves[k].RenamedFrom {
			if deprecated == "" || old < deprecated {
				deprecated, owner = old, k
			}
		}
	}
	if deprecated != "" {
		s := g.leaves[owner]
		v, err := ValuesFor(s)
		if err != nil {
			return err
		}
		yamlValues[deprecated] = v.Input
		name := "DD_" + strings.ToUpper(strings.ReplaceAll(deprecated, ".", "_"))
		if !g.facts.EnvVars[name] {
			return fmt.Errorf("deprecated name %q of %q: env var %s is not in the Agent's GetEnvVars()", deprecated, owner, name)
		}
		text, err := EnvText(s, v.Input)
		if err != nil {
			return err
		}
		envKeys[deprecated] = name
		envValues[deprecated] = text
	}

	keys := make([]string, 0, len(yamlValues))
	for k := range yamlValues {
		keys = append(keys, k)
	}
	names, batches, err := batch(keys)
	if err != nil {
		return err
	}
	for i, ks := range batches {
		sub := map[string]interface{}{}
		for _, k := range ks {
			sub[k] = yamlValues[k]
		}
		text, err := yamlText(sub)
		if err != nil {
			return err
		}
		c := newCase("unknown-yaml-"+names[i], "unknown", ks)
		c.YAML = &text
		if err := g.add(c); err != nil {
			return err
		}
	}
	keys = keys[:0]
	for k := range envKeys {
		keys = append(keys, k)
	}
	names, batches, err = batch(keys)
	if err != nil {
		return err
	}
	for i, ks := range batches {
		c := newCase("unknown-env-"+names[i], "unknown", ks)
		c.Env = map[string]string{}
		for _, k := range ks {
			c.Env[envKeys[k]] = envValues[k]
		}
		if err := g.add(c); err != nil {
			return err
		}
	}
	return nil
}
