// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

// Package schema reads the Agent's merged core schema: every setting and section, with what the
// config recorder's getter selection and case generator need about each setting.
package schema

import (
	"fmt"
	"os"
	"slices"
	"sort"
	"strings"

	"gopkg.in/yaml.v3"
)

// Kind says how the schema knows a key.
type Kind int

const (
	// Unknown is a key the schema does not have.
	Unknown Kind = iota
	// Leaf is a schema setting.
	Leaf
	// Section is a schema section, an inner node.
	Section
)

// Key is one schema key. Every field but Path and Kind is set only for a leaf.
type Key struct {
	// Path is the dotted path as the schema writes it.
	Path string
	Kind Kind
	// Type is the declared schema `type`.
	Type string
	// Format is the schema `format`, e.g. "duration".
	Format string
	// Tags are the schema `tags`, as written.
	Tags []string
	// ItemType is the `items` type of an array, or the `additionalProperties` type of an object.
	ItemType string
	// ItemItemType is the `items` type of an array-typed ItemType (a map of lists).
	ItemItemType string
	// Default is the schema `default`, or the `linux` entry of `platform_default` when that is
	// given.
	Default interface{}
	// EnvVars are the schema `env_vars`, in schema order.
	EnvVars   []string
	EnvParser string
	// NoEnv is true when the tags include `no-env`.
	NoEnv bool
	// RenamedFrom are the keys of `renamed_from`, lowercased and sorted.
	RenamedFrom []string
}

// GolangType is the value of the key's `golang_type:` tag, or "" when it has none.
func (k *Key) GolangType() string {
	for _, t := range k.Tags {
		if v, ok := strings.CutPrefix(t, "golang_type:"); ok {
			return v
		}
	}
	return ""
}

// ElementType is ItemType, except that an array of strings is written `array_of_string`.
func (k *Key) ElementType() string {
	if k.ItemType == "array" && k.ItemItemType == "string" {
		return "array_of_string"
	}
	return k.ItemType
}

// Schema is every schema key by dotted path as the schema writes it.
type Schema map[string]*Key

type rawNode struct {
	NodeType             string                 `yaml:"node_type"`
	Type                 string                 `yaml:"type"`
	Format               string                 `yaml:"format"`
	Default              interface{}            `yaml:"default"`
	PlatformDefault      map[string]interface{} `yaml:"platform_default"`
	EnvVars              []string               `yaml:"env_vars"`
	EnvParser            string                 `yaml:"env_parser"`
	Tags                 []string               `yaml:"tags"`
	RenamedFrom          map[string]interface{} `yaml:"renamed_from"`
	Items                *rawNode               `yaml:"items"`
	AdditionalProperties *rawNode               `yaml:"additionalProperties"`
	Properties           map[string]*rawNode    `yaml:"properties"`
}

// Load reads the Agent's merged core schema (`$ref`s inlined, every annotation kept) from path, as
// the Agent's `//pkg/config/schema:merged_core_schema` step (tasks/schema/merge_schema.py) writes
// it. The schema the Agent embeds in its own binary is not used: its build step drops `tags`,
// which carry `golang_type`.
func Load(path string) (Schema, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	return Parse(data)
}

// Parse reads a merged core schema from data.
func Parse(data []byte) (Schema, error) {
	var root rawNode
	if err := yaml.Unmarshal(data, &root); err != nil {
		return nil, fmt.Errorf("schema: %w", err)
	}
	s := Schema{}
	s.walk("", &root)
	if len(s) == 0 {
		return nil, fmt.Errorf("schema has no keys")
	}
	return s, nil
}

func (s Schema) walk(prefix string, n *rawNode) {
	for name, c := range n.Properties {
		if c == nil {
			continue
		}
		path := name
		if prefix != "" {
			path = prefix + "." + name
		}
		switch c.NodeType {
		case "section":
			s[path] = &Key{Path: path, Kind: Section}
			s.walk(path, c)
		case "setting":
			s[path] = leaf(path, c)
		}
	}
}

func leaf(path string, c *rawNode) *Key {
	k := &Key{Path: path, Kind: Leaf, Type: c.Type, Format: c.Format, Tags: c.Tags, Default: c.Default,
		EnvVars: c.EnvVars, EnvParser: c.EnvParser}
	if c.PlatformDefault != nil {
		// The recorder runs on Linux; the Agent's getPlatformDefault picks `linux` there.
		k.Default = c.PlatformDefault["linux"]
	}
	var elem *rawNode
	switch c.Type {
	case "array":
		elem = c.Items
	case "object":
		elem = c.AdditionalProperties
	}
	if elem != nil {
		k.ItemType = elem.Type
		if elem.Items != nil {
			k.ItemItemType = elem.Items.Type
		}
	}
	for _, t := range c.Tags {
		if t == "no-env" {
			k.NoEnv = true
		}
	}
	for old := range c.RenamedFrom {
		k.RenamedFrom = append(k.RenamedFrom, strings.ToLower(old))
	}
	sort.Strings(k.RenamedFrom)
	return k
}

// Key returns the schema key at path; a path not in the schema is a Key of Kind Unknown.
func (s Schema) Key(path string) *Key {
	if k, ok := s[path]; ok {
		return k
	}
	return &Key{Path: path, Kind: Unknown}
}

// Leaves returns the schema's leaf paths, sorted.
func (s Schema) Leaves() []string {
	var out []string
	for p, k := range s {
		if k.Kind == Leaf {
			out = append(out, p)
		}
	}
	sort.Strings(out)
	return out
}

// LowercasedLeaves returns the schema's leaves by lowercased path. Two leaves whose paths
// lowercase alike are an error.
func (s Schema) LowercasedLeaves() (map[string]*Key, error) {
	out := map[string]*Key{}
	for _, p := range s.Leaves() {
		lower := strings.ToLower(p)
		if _, dup := out[lower]; dup {
			return nil, fmt.Errorf("schema: two settings lowercase to %q", lower)
		}
		out[lower] = s[p]
	}
	if len(out) == 0 {
		return nil, fmt.Errorf("schema has no settings")
	}
	return out, nil
}

// EnvBindings returns, for every leaf by lowercased path, the set of env var names the Agent binds
// for it, sorted, as the Agent's bindEnv and BindEnvAndSetDefaultWithDeprecation do
// (pkg/config/nodetreemodel/config.go): the schema's `env_vars` when it lists any; otherwise the
// derived name (DerivedEnvName) and, for a key with `renamed_from`, the derived names of its former
// names. A `no-env` key binds none and is left out. The result is a set: it does not follow the
// Agent's own order of a key's names (deprecated names first, the new name last), so a caller that
// needs one name must choose it by its own rule.
func (s Schema) EnvBindings() map[string][]string {
	out := map[string][]string{}
	for _, p := range s.Leaves() {
		k := s[p]
		if k.NoEnv {
			continue
		}
		var names []string
		if len(k.EnvVars) > 0 {
			names = slices.Clone(k.EnvVars)
		} else {
			names = []string{DerivedEnvName(p)}
			for _, old := range k.RenamedFrom {
				names = append(names, DerivedEnvName(old))
			}
		}
		slices.Sort(names)
		out[strings.ToLower(p)] = slices.Compact(names)
	}
	return out
}

// EnvKeys inverts bindings: every bound env var name to the keys it binds, sorted.
func EnvKeys(bindings map[string][]string) map[string][]string {
	out := map[string][]string{}
	for k, names := range bindings {
		for _, n := range names {
			if !slices.Contains(out[n], k) {
				out[n] = append(out[n], k)
			}
		}
	}
	for n := range out {
		sort.Strings(out[n])
	}
	return out
}

// DerivedEnvName is the env var name the Agent derives for a key with no `env_vars`: `DD_` and the
// key uppercased, with `.` as `_`.
func DerivedEnvName(key string) string {
	return "DD_" + strings.ToUpper(strings.ReplaceAll(key, ".", "_"))
}
