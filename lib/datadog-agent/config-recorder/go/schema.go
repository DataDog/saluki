// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package main

import (
	"fmt"
	"os"
	"sort"
	"strings"

	"gopkg.in/yaml.v3"

	"github.com/DataDog/datadog-agent/pkg/config/model"

	"github.com/DataDog/datadog-agent/cmd/config-recorder/corpus"
)

// schemaNode is the part of one schema node the recorder reads.
type schemaNode struct {
	NodeType             string                 `yaml:"node_type"`
	Type                 string                 `yaml:"type"`
	Format               string                 `yaml:"format"`
	Tags                 []string               `yaml:"tags"`
	Items                *schemaNode            `yaml:"items"`
	AdditionalProperties *schemaNode            `yaml:"additionalProperties"`
	Properties           map[string]*schemaNode `yaml:"properties"`
}

// schemaKey is what getter selection needs about one schema key.
type schemaKey struct {
	Kind         corpus.KeyKind
	DeclaredType string
	ElementType  string
	Tags         corpus.SchemaTags
}

// agentSchema is every schema key by dotted path.
type agentSchema map[string]schemaKey

// loadSchema reads the Agent's merged core schema (`$ref`s inlined, every annotation kept) from
// path, as the Agent's `//pkg/config/schema:merged_core_schema` step (tasks/schema/merge_schema.py)
// writes it. The schema the Agent embeds in its own binary is not used: its build step drops
// `tags`, which carry `golang_type`.
func loadSchema(path string) (agentSchema, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	var root schemaNode
	if err := yaml.Unmarshal(data, &root); err != nil {
		return nil, fmt.Errorf("schema: %w", err)
	}
	s := agentSchema{}
	s.walk("", &root)
	if len(s) == 0 {
		return nil, fmt.Errorf("schema has no keys")
	}
	return s, nil
}

func (s agentSchema) walk(prefix string, n *schemaNode) {
	for name, child := range n.Properties {
		if child == nil {
			continue
		}
		key := name
		if prefix != "" {
			key = prefix + "." + name
		}
		switch child.NodeType {
		case "section":
			s[key] = schemaKey{Kind: corpus.KeySection}
			s.walk(key, child)
		case "setting":
			s[key] = schemaKey{
				Kind:         corpus.KeyLeaf,
				DeclaredType: child.Type,
				ElementType:  elementType(child),
				Tags:         corpus.SchemaTags{Format: child.Format, GolangType: golangType(child.Tags)},
			}
		}
	}
}

// elementType is the `items` type of an array, or the `additionalProperties` type of an object,
// with an array of strings written `array_of_string`.
func elementType(n *schemaNode) string {
	var e *schemaNode
	switch n.Type {
	case "array":
		e = n.Items
	case "object":
		e = n.AdditionalProperties
	}
	if e == nil {
		return ""
	}
	if e.Type == "array" && e.Items != nil && e.Items.Type == "string" {
		return "array_of_string"
	}
	return e.Type
}

func golangType(tags []string) string {
	for _, t := range tags {
		if v, ok := strings.CutPrefix(t, "golang_type:"); ok {
			return v
		}
	}
	return ""
}

func (s agentSchema) leaves() []string {
	var out []string
	for k, v := range s {
		if v.Kind == corpus.KeyLeaf {
			out = append(out, k)
		}
	}
	sort.Strings(out)
	return out
}

// checkSchemaKeys checks that the schema's leaves are exactly the Agent's own key set for the
// config cfg (getter-map.md §1). Call it right after the first snapshot and before any getter
// runs (record.md §4.1): AllKeysLowercased is not itself a getter and must not join unknown keys
// to the key set, but a getter called first would, making a later mismatch ambiguous.
//
// AllKeysLowercased lowercases every key (e.g. a schema key named with an acronym like `GUI_host`
// comes back as `gui_host`), so the schema side is lowercased the same way before comparing.
func checkSchemaKeys(schema agentSchema, cfg model.Reader) error {
	leaves := schema.leaves()
	lowered := make([]string, len(leaves))
	for i, k := range leaves {
		lowered[i] = strings.ToLower(k)
	}
	diff := corpus.DiffSchemaKeys(lowered, cfg.AllKeysLowercased())
	if !diff.Empty() {
		return diff
	}
	return nil
}
