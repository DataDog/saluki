// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

// Package gen writes the config recorder's generated cases: it reads the Agent's merged schema
// (package schema) and saluki's overlay, chooses the keys of each generated group, and gives them values by rule. It writes each case as a record.Case.
// It needs no Agent config: the facts only a built config knows (default-layer Go types and the
// bound env var names) come in as AgentFacts.
package gen

import (
	"fmt"
	"strings"

	"gopkg.in/yaml.v3"
)

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
