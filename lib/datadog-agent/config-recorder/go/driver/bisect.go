// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package driver

import (
	"errors"
	"fmt"
	"slices"
	"strings"

	"github.com/DataDog/datadog-agent/pkg/config/model"
	"gopkg.in/yaml.v3"

	"github.com/DataDog/datadog-agent/cmd/config-recorder/record"
)

// ErrUnassignable marks a case input that sets none of the case's keys, which splitting cannot
// give to a part (case.md §3.1).
var ErrUnassignable = errors.New("case input sets none of the case's keys")

// ExpectedSources gives, for each of the case's keys that one of its inputs sets, the source the
// key's snapshot should stream: the highest-priority source (model.Source.IsGreaterThan) among
// `environment-variable` for env, `file` for yaml, `fleet-policies` for fleet_policy and `cli`
// for cli (case.md §3.1). envKeys maps each bound env var name to the keys it binds.
func ExpectedSources(c *record.Case, envKeys map[string][]string) (map[string]model.Source, error) {
	keys := caseKeySet(c)
	out := map[string]model.Source{}
	set := func(k string, s model.Source) {
		if !keys[k] {
			return
		}
		if cur, ok := out[k]; !ok || s.IsGreaterThan(cur) {
			out[k] = s
		}
	}
	for name := range c.Env {
		for _, k := range envKeys[name] {
			set(k, model.SourceEnvVar)
		}
	}
	for _, in := range []struct {
		text   *string
		source model.Source
	}{{c.YAML, model.SourceFile}, {c.FleetPolicy, model.SourceFleetPolicies}} {
		if in.text == nil {
			continue
		}
		root, err := parseYAMLRoot(*in.text)
		if err != nil {
			return nil, err
		}
		for _, entry := range c.Keys {
			if yamlSets(root, entry.Key) {
				set(entry.Key, in.source)
			}
		}
	}
	for _, o := range c.CLI {
		set(o.Key, model.SourceCLI)
	}
	return out, nil
}

// IsClean says whether a case's run needs no splitting (case.md §3.1): it started, and no key is
// dirty (DirtyKeys).
func IsClean(r *record.RunResult, expected map[string]model.Source, base map[string]record.Setting) bool {
	return started(r) && len(DirtyKeys(r, expected, base)) == 0
}

// DirtyKeys are the keys of a started run that are not clean (case.md §3.1), in the run's key
// order. A key the case sets from no input is expected to stream its baseline source; a key absent
// from the case's snapshot is clean only when it is also absent from the baseline's.
func DirtyKeys(r *record.RunResult, expected map[string]model.Source, base map[string]record.Setting) []string {
	var out []string
	for _, kl := range r.Run.Keys {
		b, inBase := base[kl.Key]
		if kl.Snapshot == nil {
			if inBase {
				out = append(out, kl.Key)
			}
			continue
		}
		want, ok := expected[kl.Key]
		if !ok {
			if !inBase {
				out = append(out, kl.Key)
				continue
			}
			want = model.Source(b.Source)
		}
		if kl.Snapshot.Source != string(want) {
			out = append(out, kl.Key)
		}
	}
	return out
}

// started says whether a run constructed the config and received its first snapshot.
func started(r *record.RunResult) bool {
	return r.StartupError == nil && r.Run != nil
}

// splitter projects a case onto subsets of its keys (case.md §3.1). newSplitter checks, once,
// that every input of the case can be given to a key: it is built only when the case must be
// split, since only then are these harness failures.
type splitter struct {
	c         *record.Case
	envKeys   map[string][]string
	yamlRoot  *yaml.Node
	fleetRoot *yaml.Node
}

// newSplitter checks that c can be split: its YAML inputs parse and use no anchors, aliases or
// merge keys, and every input sets one of its keys. envKeys maps each bound env var name to the
// keys it binds.
func newSplitter(c *record.Case, envKeys map[string][]string) (*splitter, error) {
	sp := &splitter{c: c, envKeys: envKeys}
	all := caseKeySet(c)
	for _, in := range []struct {
		field string
		text  *string
		root  **yaml.Node
	}{{"yaml", c.YAML, &sp.yamlRoot}, {"fleet_policy", c.FleetPolicy, &sp.fleetRoot}} {
		if in.text == nil {
			continue
		}
		root, err := parseYAMLRoot(*in.text)
		if err != nil {
			return nil, fmt.Errorf("case %q: %s: %w", c.Name, in.field, err)
		}
		if err := checkNoAliases(root); err != nil {
			return nil, fmt.Errorf("case %q: %s: %w", c.Name, in.field, err)
		}
		if err := checkYAMLAssigned(root, "", all); err != nil {
			return nil, fmt.Errorf("case %q: %s: %w", c.Name, in.field, err)
		}
		*in.root = root
	}
	for name := range c.Env {
		if !slices.ContainsFunc(envKeys[name], func(k string) bool { return all[k] }) {
			return nil, fmt.Errorf("case %q: %w: env %s", c.Name, ErrUnassignable, name)
		}
	}
	for _, o := range c.CLI {
		if !all[o.Key] {
			return nil, fmt.Errorf("case %q: %w: cli %q", c.Name, ErrUnassignable, o.Key)
		}
	}
	for _, u := range c.Updates {
		if !all[u.Key] {
			return nil, fmt.Errorf("case %q: %w: update of %q", c.Name, ErrUnassignable, u.Key)
		}
	}
	return sp, nil
}

// project returns the case cut down to keys, named name: each input goes with the keys it sets,
// keeping its relative order, and the case's group and why are kept (case.md §3.1). keys are in
// the case's key order.
func (sp *splitter) project(keys []record.KeyEntry, name string) (*record.Case, error) {
	c := sp.c
	p := &record.Case{Name: name, Group: c.Group, Why: c.Why, Keys: slices.Clone(keys)}
	in := caseKeySet(p)
	for n, v := range c.Env {
		if slices.ContainsFunc(sp.envKeys[n], func(k string) bool { return in[k] }) {
			if p.Env == nil {
				p.Env = map[string]string{}
			}
			p.Env[n] = v
		}
	}
	var err error
	if p.YAML, err = pruneYAML(sp.yamlRoot, in); err != nil {
		return nil, fmt.Errorf("case %q: yaml: %w", c.Name, err)
	}
	if p.FleetPolicy, err = pruneYAML(sp.fleetRoot, in); err != nil {
		return nil, fmt.Errorf("case %q: fleet_policy: %w", c.Name, err)
	}
	for _, o := range c.CLI {
		if in[o.Key] {
			p.CLI = append(p.CLI, o)
		}
	}
	for _, u := range c.Updates {
		if in[u.Key] {
			p.Updates = append(p.Updates, u)
		}
	}
	return p, nil
}

// ErrYAMLAlias marks YAML that uses an anchor, an alias or a merge key in a case that must be
// split: pruning its node tree could drop an anchor that a kept alias refers to (case.md §3.1).
var ErrYAMLAlias = errors.New("YAML uses anchors, aliases or merge keys, so the case cannot be split")

// checkNoAliases fails on the first anchor, alias or merge key in the tree, in document order.
func checkNoAliases(n *yaml.Node) error {
	if n == nil {
		return nil
	}
	switch {
	case n.Kind == yaml.AliasNode:
		return fmt.Errorf("%w: alias *%s", ErrYAMLAlias, n.Value)
	case n.Anchor != "":
		return fmt.Errorf("%w: anchor &%s", ErrYAMLAlias, n.Anchor)
	case n.Kind == yaml.ScalarNode && n.Tag == "!!merge":
		return fmt.Errorf("%w: merge key %s", ErrYAMLAlias, n.Value)
	}
	for _, sub := range n.Content {
		if err := checkNoAliases(sub); err != nil {
			return err
		}
	}
	return nil
}

func caseKeySet(c *record.Case) map[string]bool {
	keys := map[string]bool{}
	for _, e := range c.Keys {
		keys[e.Key] = true
	}
	return keys
}

// parseYAMLRoot parses YAML text into its top mapping node, or nil for an empty document.
func parseYAMLRoot(text string) (*yaml.Node, error) {
	var doc yaml.Node
	if err := yaml.Unmarshal([]byte(text), &doc); err != nil {
		return nil, err
	}
	if doc.Kind == 0 || len(doc.Content) == 0 {
		return nil, nil
	}
	root := doc.Content[0]
	if root.Kind == yaml.ScalarNode && root.Tag == "!!null" {
		return nil, nil
	}
	if root.Kind != yaml.MappingNode {
		return nil, fmt.Errorf("document is not a mapping")
	}
	return root, nil
}

func joinPath(prefix, name string) string {
	name = strings.ToLower(name)
	if prefix == "" {
		return name
	}
	return prefix + "." + name
}

func hasKeyUnder(keys map[string]bool, path string) bool {
	for k := range keys {
		if strings.HasPrefix(k, path+".") {
			return true
		}
	}
	return false
}

// yamlSets says whether the YAML tree holds a node at the key's path, matching names lowercased.
func yamlSets(root *yaml.Node, key string) bool {
	n := root
	for _, part := range strings.Split(key, ".") {
		if n == nil || n.Kind != yaml.MappingNode {
			return false
		}
		var next *yaml.Node
		for i := 0; i+1 < len(n.Content); i += 2 {
			if strings.ToLower(n.Content[i].Value) == part {
				next = n.Content[i+1]
			}
		}
		if next == nil {
			return false
		}
		n = next
	}
	return true
}

// checkYAMLAssigned checks that every path the YAML tree sets is one of keys or leads to one.
func checkYAMLAssigned(n *yaml.Node, prefix string, keys map[string]bool) error {
	if n == nil {
		return nil
	}
	for i := 0; i+1 < len(n.Content); i += 2 {
		path := joinPath(prefix, n.Content[i].Value)
		v := n.Content[i+1]
		switch {
		case keys[path]:
		case v.Kind == yaml.MappingNode && hasKeyUnder(keys, path):
			if err := checkYAMLAssigned(v, path, keys); err != nil {
				return err
			}
		default:
			return fmt.Errorf("%w: yaml path %q", ErrUnassignable, path)
		}
	}
	return nil
}

// pruneYAML returns the YAML tree cut down to the paths of keys, re-encoded, or nil when no path
// remains. Kept nodes are the original nodes, so a kept scalar keeps its style and tag.
func pruneYAML(root *yaml.Node, keys map[string]bool) (*string, error) {
	if root == nil {
		return nil, nil
	}
	pruned := pruneMapping(root, "", keys)
	if pruned == nil {
		return nil, nil
	}
	out, err := yaml.Marshal(&yaml.Node{Kind: yaml.DocumentNode, Content: []*yaml.Node{pruned}})
	if err != nil {
		return nil, err
	}
	s := string(out)
	return &s, nil
}

func pruneMapping(n *yaml.Node, prefix string, keys map[string]bool) *yaml.Node {
	var content []*yaml.Node
	for i := 0; i+1 < len(n.Content); i += 2 {
		path := joinPath(prefix, n.Content[i].Value)
		v := n.Content[i+1]
		if keys[path] {
			content = append(content, n.Content[i], v)
		} else if v.Kind == yaml.MappingNode && hasKeyUnder(keys, path) {
			if sub := pruneMapping(v, path, keys); sub != nil {
				content = append(content, n.Content[i], sub)
			}
		}
	}
	if len(content) == 0 {
		return nil
	}
	m := *n
	m.Content = content
	return &m
}

// SideEffects are the settings of a case's first snapshot that differ from the baseline's
// (record.md §3.2): a different streamed source, unset_source or value bytes, or a setting only
// on one side; a baseline setting absent from the case is Absent. Keys the case lists are left
// out. The result is sorted by key.
func SideEffects(base, snap map[string]record.Setting, keys []record.KeyEntry) []record.SideEffect {
	listed := map[string]bool{}
	for _, e := range keys {
		listed[e.Key] = true
	}
	var out []record.SideEffect
	for k, s := range snap {
		if listed[k] {
			continue
		}
		if b, ok := base[k]; !ok || !sameSetting(b, s) {
			out = append(out, record.SideEffect{Key: k, Setting: s})
		}
	}
	for k := range base {
		if _, ok := snap[k]; !ok && !listed[k] {
			out = append(out, record.SideEffect{Key: k, Absent: true})
		}
	}
	slices.SortFunc(out, func(a, b record.SideEffect) int { return strings.Compare(a.Key, b.Key) })
	return out
}

func sameSetting(a, b record.Setting) bool {
	return a.Source == b.Source && a.UnsetSource == b.UnsetSource && (a.Value == nil) == (b.Value == nil) &&
		string(a.Value) == string(b.Value)
}
