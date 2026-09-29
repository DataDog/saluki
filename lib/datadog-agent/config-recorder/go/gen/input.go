// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

// Package gen writes the config recorder's generated cases: it reads the Agent's merged schema
// and saluki's overlay, chooses the keys of each generated group, and gives them values by rule.
// It needs no Agent config: the facts only a built config knows (default-layer Go types and the
// bound env var names) come in as AgentFacts.
package gen

import (
	"fmt"
	"sort"
	"strings"

	"gopkg.in/yaml.v3"
)

// Setting is what the generator reads from one schema leaf.
type Setting struct {
	// Key is the dotted path, lowercased.
	Key       string
	Type      string
	Format    string
	EnvParser string
	// ItemType is the `items` type of an array, or the `additionalProperties` type of an object.
	ItemType string
	// ItemItemType is the `items` type of an array-typed ItemType (a map of lists).
	ItemItemType string
	// Default is the schema `default`, or the `linux` entry of `platform_default` when given.
	Default     interface{}
	EnvVars     []string
	NoEnv       bool
	RenamedFrom []string
}

// Schema is every schema leaf by lowercased dotted path.
type Schema map[string]*Setting

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

// ParseSchema reads the leaves (`node_type: setting`) of the Agent's merged core schema.
func ParseSchema(data []byte) (Schema, error) {
	var root rawNode
	if err := yaml.Unmarshal(data, &root); err != nil {
		return nil, fmt.Errorf("schema: %w", err)
	}
	s := Schema{}
	if err := s.walk("", &root); err != nil {
		return nil, err
	}
	if len(s) == 0 {
		return nil, fmt.Errorf("schema has no settings")
	}
	return s, nil
}

func (s Schema) walk(prefix string, n *rawNode) error {
	for name, c := range n.Properties {
		if c == nil {
			continue
		}
		key := strings.ToLower(name)
		if prefix != "" {
			key = prefix + "." + key
		}
		switch c.NodeType {
		case "section":
			if err := s.walk(key, c); err != nil {
				return err
			}
		case "setting":
			if _, dup := s[key]; dup {
				return fmt.Errorf("schema: two settings lowercase to %q", key)
			}
			st := &Setting{Key: key, Type: c.Type, Format: c.Format, EnvParser: c.EnvParser, Default: c.Default,
				EnvVars: c.EnvVars}
			if c.PlatformDefault != nil {
				// The recorder runs on Linux; the Agent's getPlatformDefault picks `linux` there.
				st.Default = c.PlatformDefault["linux"]
			}
			var elem *rawNode
			switch c.Type {
			case "array":
				elem = c.Items
			case "object":
				elem = c.AdditionalProperties
			}
			if elem != nil {
				st.ItemType = elem.Type
				if elem.Items != nil {
					st.ItemItemType = elem.Items.Type
				}
			}
			for _, t := range c.Tags {
				if t == "no-env" {
					st.NoEnv = true
				}
			}
			for old := range c.RenamedFrom {
				st.RenamedFrom = append(st.RenamedFrom, strings.ToLower(old))
			}
			sort.Strings(st.RenamedFrom)
			s[key] = st
		}
	}
	return nil
}

// Keys returns the schema's keys in byte order.
func (s Schema) Keys() []string {
	out := make([]string, 0, len(s))
	for k := range s {
		out = append(out, k)
	}
	sort.Strings(out)
	return out
}

// Overlay is the part of saluki's schema overlay the generator reads: each inventory key's
// `support`, and the excluded key names. Keys are lowercased.
type Overlay struct {
	Support  map[string]string
	Excluded map[string]bool
}

// ParseOverlay reads `inventory.<key>.support` and the keys of `excluded`, and ignores the rest.
func ParseOverlay(data []byte) (*Overlay, error) {
	var raw struct {
		Inventory map[string]struct {
			Support string `yaml:"support"`
		} `yaml:"inventory"`
		Excluded map[string]interface{} `yaml:"excluded"`
	}
	if err := yaml.Unmarshal(data, &raw); err != nil {
		return nil, fmt.Errorf("overlay: %w", err)
	}
	o := &Overlay{Support: map[string]string{}, Excluded: map[string]bool{}}
	for k, v := range raw.Inventory {
		switch v.Support {
		case "full", "partial", "none", "unknown":
		default:
			return nil, fmt.Errorf("overlay: inventory key %q has support %q", k, v.Support)
		}
		o.Support[strings.ToLower(k)] = v.Support
	}
	for k := range raw.Excluded {
		o.Excluded[strings.ToLower(k)] = true
	}
	return o, nil
}

// AgentFacts holds what only the Agent's built config knows.
type AgentFacts struct {
	// DefaultType is the `%T` of each schema key's default-layer value, `<nil>` when it has none.
	DefaultType map[string]string
	// EnvVars is the Agent's GetEnvVars().
	EnvVars map[string]bool
}
