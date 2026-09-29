// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package record

import (
	"bytes"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"regexp"
	"strings"

	"gopkg.in/yaml.v3"
)

// Case is one parsed and validated case file.
type Case struct {
	Name  string
	Group string
	Why   []string
	// Env is nil when the file has no `env`; an empty map when it has `env: {}`.
	Env         map[string]string
	YAML        *string
	FleetPolicy *string
	CLI         []CLIOverride
	Updates     []Update
	Keys        []KeyEntry
}

// CLIOverride is one startup CLI override with its decoded typed value.
type CLIOverride struct {
	Key   string
	Value interface{}
}

// Update is one runtime update. Value is meaningful only when Op is "set".
type Update struct {
	Op     string
	Key    string
	Value  interface{}
	Source string
}

// KeyEntry is one key to record. Getters is nil unless the case overrides the default list.
type KeyEntry struct {
	Key     string
	Getters []string
}

// Errors for each case-file validation rule, so callers and tests can tell them apart.
var (
	ErrCaseSyntax         = errors.New("case file is not valid YAML for a case")
	ErrCaseMultipleDocs   = errors.New("case file holds more than one YAML document")
	ErrCaseMissingField   = errors.New("case file is missing a required field")
	ErrCaseFieldType      = errors.New("case field has the wrong type")
	ErrCaseName           = errors.New("case name must match ^[a-z0-9][a-z0-9-]*$")
	ErrCaseNameStem       = errors.New("case name must equal the file stem")
	ErrCaseGroup          = errors.New("case group is not a known group")
	ErrCaseWhyRequired    = errors.New("behavior case must have a non-empty why")
	ErrCaseEnvName        = errors.New("env name must match ^[A-Za-z_][A-Za-z0-9_]*$")
	ErrCaseEnvValue       = errors.New("env value must be a YAML string; quote it")
	ErrCaseCLIDuplicate   = errors.New("cli keys must be unique")
	ErrCaseSecrets        = errors.New("secrets is not supported; remove the secrets field")
	ErrCaseUpdateOp       = errors.New("update op must be set or unset")
	ErrCaseUpdateValue    = errors.New("update value must be given for set and absent for unset")
	ErrCaseUpdateSource   = errors.New("update source is not an allowed model.Source")
	ErrCaseNoKeys         = errors.New("case must list at least one key")
	ErrCaseKeyEmpty       = errors.New("key must not be empty")
	ErrCaseKeyUppercase   = errors.New("key must not contain an uppercase letter")
	ErrCaseKeyDuplicate   = errors.New("keys must be unique within a case")
	ErrCaseGettersEmpty   = errors.New("getters override must be non-empty")
	ErrCaseGetterUnknown  = errors.New("getter name is not an allowed getter")
	ErrCaseGetterRepeated = errors.New("getter listed twice for one key")
)

var (
	caseNameRe = regexp.MustCompile(`^[a-z0-9][a-z0-9-]*$`)
	envNameRe  = regexp.MustCompile(`^[A-Za-z_][A-Za-z0-9_]*$`)
)

var caseGroups = map[string]bool{
	"baseline": true, "breadth": true, "depth": true, "unsupported": true,
	"excluded": true, "unknown": true, "behavior": true,
}

// updateSources are the sources an update may write or clear. `default`, `schema`, `unknown` and
// `provided` are left out because they are not layers a writer can target.
var updateSources = map[string]bool{
	"infra-mode": true, "file": true, "environment-variable": true, "fleet-policies": true,
	"config-post-init": true, "secret": true, "local-config-process": true, "agent-runtime": true,
	"remote-config": true, "cli": true,
}

// yamlString is a string field that only accepts a `!!str` node. yaml.v3 would otherwise decode
// `1` or `true` into a string field silently.
type yamlString string

func (s *yamlString) UnmarshalYAML(n *yaml.Node) error {
	if n.Kind != yaml.ScalarNode || n.ShortTag() != "!!str" {
		return fmt.Errorf("%w: line %d: want a string, got %s", ErrCaseFieldType, n.Line, describeNode(n))
	}
	*s = yamlString(n.Value)
	return nil
}

// A zero yaml.Node (Kind 0) marks an absent field; a pointer would lose `value: null`.
type rawCase struct {
	Name        *yamlString  `yaml:"name"`
	Group       *yamlString  `yaml:"group"`
	Why         []yamlString `yaml:"why"`
	Env         yaml.Node    `yaml:"env"`
	YAML        *yamlString  `yaml:"yaml"`
	FleetPolicy *yamlString  `yaml:"fleet_policy"`
	CLI         []rawCLI     `yaml:"cli"`
	Secrets     yaml.Node    `yaml:"secrets"`
	Updates     []rawUpdate  `yaml:"updates"`
	Keys        []rawKey     `yaml:"keys"`
}

type rawCLI struct {
	Key   *yamlString `yaml:"key"`
	Value yaml.Node   `yaml:"value"`
}

type rawUpdate struct {
	Op     *yamlString `yaml:"op"`
	Key    *yamlString `yaml:"key"`
	Value  yaml.Node   `yaml:"value"`
	Source *yamlString `yaml:"source"`
}

// rawKey accepts either a bare key string or a `{key, getters}` mapping.
type rawKey struct {
	Key     *yamlString
	Getters []yamlString
	HasGets bool
}

func (k *rawKey) UnmarshalYAML(n *yaml.Node) error {
	if n.Kind == yaml.ScalarNode {
		var s yamlString
		if err := s.UnmarshalYAML(n); err != nil {
			return err
		}
		k.Key = &s
		return nil
	}
	if n.Kind != yaml.MappingNode {
		return fmt.Errorf("%w: line %d: key entry must be a string or a mapping", ErrCaseFieldType, n.Line)
	}
	// Node.Decode does not inherit KnownFields, so check the members here.
	for i := 0; i+1 < len(n.Content); i += 2 {
		name, val := n.Content[i], n.Content[i+1]
		switch name.Value {
		case "key":
			if k.Key != nil {
				return fmt.Errorf("%w: line %d: key given twice", ErrCaseSyntax, name.Line)
			}
			var s yamlString
			if err := s.UnmarshalYAML(val); err != nil {
				return err
			}
			k.Key = &s
		case "getters":
			if k.HasGets {
				return fmt.Errorf("%w: line %d: getters given twice", ErrCaseSyntax, name.Line)
			}
			if val.Kind != yaml.SequenceNode {
				return fmt.Errorf("%w: line %d: getters must be a list", ErrCaseFieldType, val.Line)
			}
			k.HasGets = true
			k.Getters = []yamlString{}
			for _, g := range val.Content {
				var s yamlString
				if err := s.UnmarshalYAML(g); err != nil {
					return err
				}
				k.Getters = append(k.Getters, s)
			}
		default:
			return fmt.Errorf("%w: line %d: field %s not found in key entry", ErrCaseSyntax, name.Line, name.Value)
		}
	}
	return nil
}

// ParseCaseFile reads and validates a case file, and checks that its name equals the file stem.
func ParseCaseFile(path string) (*Case, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	c, err := ParseCase(data)
	if err != nil {
		return nil, fmt.Errorf("%s: %w", path, err)
	}
	if stem := strings.TrimSuffix(filepath.Base(path), ".yaml"); stem != c.Name {
		return nil, fmt.Errorf("%s: %w: name %q, stem %q", path, ErrCaseNameStem, c.Name, stem)
	}
	return c, nil
}

// ParseCase decodes and validates the content of one case file.
func ParseCase(data []byte) (*Case, error) {
	dec := yaml.NewDecoder(bytes.NewReader(data))
	dec.KnownFields(true)
	var raw rawCase
	if err := dec.Decode(&raw); err != nil {
		if errors.Is(err, io.EOF) {
			return nil, fmt.Errorf("%w: name", ErrCaseMissingField)
		}
		// Errors from yamlString and rawKey already carry a sentinel.
		for _, sentinel := range []error{ErrCaseFieldType, ErrCaseSyntax} {
			if errors.Is(err, sentinel) {
				return nil, err
			}
		}
		return nil, fmt.Errorf("%w: %v", ErrCaseSyntax, err)
	}
	var extra yaml.Node
	if err := dec.Decode(&extra); !errors.Is(err, io.EOF) {
		return nil, ErrCaseMultipleDocs
	}
	return raw.validate()
}

func (raw *rawCase) validate() (*Case, error) {
	c := &Case{Why: []string{}}

	if raw.Name == nil {
		return nil, fmt.Errorf("%w: name", ErrCaseMissingField)
	}
	c.Name = string(*raw.Name)
	if !caseNameRe.MatchString(c.Name) {
		return nil, fmt.Errorf("%w: %q", ErrCaseName, c.Name)
	}

	if raw.Group == nil {
		return nil, fmt.Errorf("%w: group", ErrCaseMissingField)
	}
	c.Group = string(*raw.Group)
	if !caseGroups[c.Group] {
		return nil, fmt.Errorf("%w: %q", ErrCaseGroup, c.Group)
	}
	for _, w := range raw.Why {
		c.Why = append(c.Why, string(w))
	}
	if c.Group == "behavior" && len(c.Why) == 0 {
		return nil, ErrCaseWhyRequired
	}

	env, err := decodeEnv(&raw.Env)
	if err != nil {
		return nil, err
	}
	c.Env = env

	if raw.YAML != nil {
		s := string(*raw.YAML)
		c.YAML = &s
	}
	if raw.FleetPolicy != nil {
		s := string(*raw.FleetPolicy)
		c.FleetPolicy = &s
	}

	seenCLI := map[string]bool{}
	for i, rc := range raw.CLI {
		if rc.Key == nil {
			return nil, fmt.Errorf("%w: cli[%d].key", ErrCaseMissingField, i)
		}
		if rc.Value.Kind == 0 {
			return nil, fmt.Errorf("%w: cli[%d].value", ErrCaseMissingField, i)
		}
		key := string(*rc.Key)
		if seenCLI[key] {
			return nil, fmt.Errorf("%w: %q", ErrCaseCLIDuplicate, key)
		}
		seenCLI[key] = true
		v, err := decodeTypedValue(&rc.Value)
		if err != nil {
			return nil, fmt.Errorf("cli[%d].value: %w", i, err)
		}
		c.CLI = append(c.CLI, CLIOverride{Key: key, Value: v})
	}

	if raw.Secrets.Kind != 0 {
		secrets, err := decodeStringMap(&raw.Secrets, "secrets")
		if err != nil {
			return nil, err
		}
		if len(secrets) > 0 {
			return nil, ErrCaseSecrets
		}
	}

	for i, ru := range raw.Updates {
		u, err := ru.validate()
		if err != nil {
			return nil, fmt.Errorf("updates[%d]: %w", i, err)
		}
		c.Updates = append(c.Updates, u)
	}

	if len(raw.Keys) == 0 {
		return nil, ErrCaseNoKeys
	}
	seenKeys := map[string]bool{}
	for i, rk := range raw.Keys {
		k, err := rk.validate()
		if err != nil {
			return nil, fmt.Errorf("keys[%d]: %w", i, err)
		}
		if seenKeys[k.Key] {
			return nil, fmt.Errorf("%w: %q", ErrCaseKeyDuplicate, k.Key)
		}
		seenKeys[k.Key] = true
		c.Keys = append(c.Keys, k)
	}
	return c, nil
}

func (ru *rawUpdate) validate() (Update, error) {
	u := Update{Op: "set"}
	if ru.Op != nil {
		u.Op = string(*ru.Op)
	}
	if u.Op != "set" && u.Op != "unset" {
		return u, fmt.Errorf("%w: %q", ErrCaseUpdateOp, u.Op)
	}
	if ru.Key == nil {
		return u, fmt.Errorf("%w: key", ErrCaseMissingField)
	}
	u.Key = string(*ru.Key)
	if ru.Source == nil {
		return u, fmt.Errorf("%w: source", ErrCaseMissingField)
	}
	u.Source = string(*ru.Source)
	if !updateSources[u.Source] {
		return u, fmt.Errorf("%w: %q", ErrCaseUpdateSource, u.Source)
	}
	hasValue := ru.Value.Kind != 0
	if hasValue != (u.Op == "set") {
		return u, fmt.Errorf("%w: op %s", ErrCaseUpdateValue, u.Op)
	}
	if hasValue {
		v, err := decodeTypedValue(&ru.Value)
		if err != nil {
			return u, fmt.Errorf("value: %w", err)
		}
		u.Value = v
	}
	return u, nil
}

func (rk *rawKey) validate() (KeyEntry, error) {
	if rk.Key == nil {
		return KeyEntry{}, fmt.Errorf("%w: key", ErrCaseMissingField)
	}
	k := KeyEntry{Key: string(*rk.Key)}
	if k.Key == "" {
		return k, ErrCaseKeyEmpty
	}
	if strings.ToLower(k.Key) != k.Key {
		return k, fmt.Errorf("%w: %q", ErrCaseKeyUppercase, k.Key)
	}
	if rk.HasGets {
		if len(rk.Getters) == 0 {
			return k, fmt.Errorf("%w: %q", ErrCaseGettersEmpty, k.Key)
		}
		k.Getters = []string{}
		seen := map[string]bool{}
		for _, g := range rk.Getters {
			name := string(g)
			if !IsGetterName(name) {
				return k, fmt.Errorf("%w: %q", ErrCaseGetterUnknown, name)
			}
			if seen[name] {
				return k, fmt.Errorf("%w: %q", ErrCaseGetterRepeated, name)
			}
			seen[name] = true
			k.Getters = append(k.Getters, name)
		}
	}
	return k, nil
}

func decodeEnv(n *yaml.Node) (map[string]string, error) {
	if n.Kind == 0 {
		return nil, nil
	}
	if n.Kind != yaml.MappingNode {
		return nil, fmt.Errorf("%w: env must be a mapping", ErrCaseFieldType)
	}
	env := map[string]string{}
	for i := 0; i+1 < len(n.Content); i += 2 {
		name, val := n.Content[i], n.Content[i+1]
		// The tag check matters: `true:` would otherwise pass the name pattern.
		if name.Kind != yaml.ScalarNode || name.ShortTag() != "!!str" || !envNameRe.MatchString(name.Value) {
			return nil, fmt.Errorf("%w: %q", ErrCaseEnvName, name.Value)
		}
		if _, dup := env[name.Value]; dup {
			return nil, fmt.Errorf("%w: env name %q given twice", ErrCaseSyntax, name.Value)
		}
		if val.Kind != yaml.ScalarNode || val.ShortTag() != "!!str" {
			return nil, fmt.Errorf("%w: %s is %s", ErrCaseEnvValue, name.Value, describeNode(val))
		}
		env[name.Value] = val.Value
	}
	return env, nil
}

func decodeStringMap(n *yaml.Node, field string) (map[string]string, error) {
	if n.Kind == yaml.ScalarNode && n.ShortTag() == "!!null" {
		return nil, nil
	}
	if n.Kind != yaml.MappingNode {
		return nil, fmt.Errorf("%w: %s must be a mapping", ErrCaseFieldType, field)
	}
	out := map[string]string{}
	for i := 0; i+1 < len(n.Content); i += 2 {
		k, v := n.Content[i], n.Content[i+1]
		if k.ShortTag() != "!!str" || v.Kind != yaml.ScalarNode || v.ShortTag() != "!!str" {
			return nil, fmt.Errorf("%w: %s must map strings to strings", ErrCaseFieldType, field)
		}
		out[k.Value] = v.Value
	}
	return out, nil
}

func describeNode(n *yaml.Node) string {
	switch n.Kind {
	case yaml.ScalarNode:
		return n.ShortTag()
	case yaml.SequenceNode:
		return "a sequence"
	case yaml.MappingNode:
		return "a mapping"
	case yaml.AliasNode:
		return "an alias"
	default:
		return "an unexpected node"
	}
}
