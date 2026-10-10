// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package record

import (
	"bytes"
	"errors"
	"fmt"
	"math"
	"os"
	"path/filepath"
	"reflect"
	"sort"
	"strconv"
	"strings"

	"gopkg.in/yaml.v3"
)

// ErrCaseWrite marks a case that cannot be written as a case file that reads back to it.
var ErrCaseWrite = errors.New("case cannot be written as a case file")

// WriteCaseFile writes c to path as a case file in one canonical form: the fields in the order of
// case.md §2, each omitted when it has no value, and every string written in a style that keeps
// its bytes (a multi-line `yaml` or `fleet_policy` keeps its trailing newline, or lack of one).
// The file stem must be the case name.
//
// It checks that the written file parses back to a Case equal to c, so a case that ParseCase would
// reject or read differently (a nil Why, an invalid name, an unsupported typed value) is an error
// and no file is written.
func WriteCaseFile(path string, c *Case) error {
	data, err := MarshalCase(c)
	if err != nil {
		return err
	}
	if stem := filepath.Base(path); stem != c.Name+".yaml" {
		return fmt.Errorf("%w: file %s is not named %s.yaml", ErrCaseWrite, path, c.Name)
	}
	return os.WriteFile(path, data, 0o644)
}

// MarshalCase renders c in WriteCaseFile's canonical form and checks that it parses back to c.
func MarshalCase(c *Case) ([]byte, error) {
	doc, err := caseNode(c)
	if err != nil {
		return nil, err
	}
	var buf bytes.Buffer
	enc := yaml.NewEncoder(&buf)
	enc.SetIndent(2)
	if err := enc.Encode(doc); err != nil {
		return nil, fmt.Errorf("%w: %v", ErrCaseWrite, err)
	}
	if err := enc.Close(); err != nil {
		return nil, fmt.Errorf("%w: %v", ErrCaseWrite, err)
	}
	back, err := ParseCase(buf.Bytes())
	if err != nil {
		return nil, fmt.Errorf("%w: case %q does not parse back: %v", ErrCaseWrite, c.Name, err)
	}
	if !reflect.DeepEqual(back, c) {
		return nil, fmt.Errorf("%w: case %q parses back to a different case", ErrCaseWrite, c.Name)
	}
	if caseSignMismatch(back, c) {
		return nil, fmt.Errorf("%w: case %q parses back with a different float sign (-0.0 vs 0.0)", ErrCaseWrite, c.Name)
	}
	return buf.Bytes(), nil
}

// caseSignMismatch reports whether back and c, already known equal by reflect.DeepEqual, differ
// in a float64's sign somewhere in a CLI or update typed value. DeepEqual treats -0.0 and 0.0 as
// equal, so the round-trip self-check calls this too.
func caseSignMismatch(back, c *Case) bool {
	for i := range c.CLI {
		if signMismatch(back.CLI[i].Value, c.CLI[i].Value) {
			return true
		}
	}
	for i := range c.Updates {
		if signMismatch(back.Updates[i].Value, c.Updates[i].Value) {
			return true
		}
	}
	return false
}

// signMismatch reports whether a and b, a decoded typed value (case.md §7) already known equal by
// reflect.DeepEqual, differ in a float64's sign, recursing through []interface{} and
// map[string]interface{} the way typed values nest.
func signMismatch(a, b interface{}) bool {
	switch av := a.(type) {
	case float64:
		bv, _ := b.(float64)
		return math.Signbit(av) != math.Signbit(bv)
	case []interface{}:
		bv, _ := b.([]interface{})
		for i := range av {
			if signMismatch(av[i], bv[i]) {
				return true
			}
		}
	case map[string]interface{}:
		bv, _ := b.(map[string]interface{})
		for k, v := range av {
			if signMismatch(v, bv[k]) {
				return true
			}
		}
	}
	return false
}

func caseNode(c *Case) (*yaml.Node, error) {
	m := mapping()
	add := func(key string, v *yaml.Node) { m.Content = append(m.Content, str(key), v) }
	add("name", str(c.Name))
	add("group", str(string(c.Group)))
	if len(c.Why) > 0 {
		add("why", strs(c.Why))
	}
	if c.Env != nil {
		env := mapping()
		for _, k := range sortedKeys(c.Env) {
			env.Content = append(env.Content, str(k), str(c.Env[k]))
		}
		add("env", env)
	}
	if c.YAML != nil {
		add("yaml", str(*c.YAML))
	}
	if c.FleetPolicy != nil {
		add("fleet_policy", str(*c.FleetPolicy))
	}
	if len(c.CLI) > 0 {
		cli := &yaml.Node{Kind: yaml.SequenceNode}
		for _, o := range c.CLI {
			v, err := typedNode(o.Value)
			if err != nil {
				return nil, fmt.Errorf("cli %q: %w", o.Key, err)
			}
			e := mapping()
			e.Content = append(e.Content, str("key"), str(o.Key), str("value"), v)
			cli.Content = append(cli.Content, e)
		}
		add("cli", cli)
	}
	if len(c.Updates) > 0 {
		updates := &yaml.Node{Kind: yaml.SequenceNode}
		for i, u := range c.Updates {
			e := mapping()
			if u.Op != "set" {
				e.Content = append(e.Content, str("op"), str(u.Op))
			}
			e.Content = append(e.Content, str("key"), str(u.Key))
			if u.Op == "set" {
				v, err := typedNode(u.Value)
				if err != nil {
					return nil, fmt.Errorf("updates[%d]: %w", i, err)
				}
				e.Content = append(e.Content, str("value"), v)
			}
			e.Content = append(e.Content, str("source"), str(u.Source))
			updates.Content = append(updates.Content, e)
		}
		add("updates", updates)
	}
	keys := &yaml.Node{Kind: yaml.SequenceNode}
	for _, k := range c.Keys {
		if k.Getters == nil {
			keys.Content = append(keys.Content, str(k.Key))
			continue
		}
		e := mapping()
		e.Content = append(e.Content, str("key"), str(k.Key), str("getters"), strs(k.Getters))
		e.Style = yaml.FlowStyle
		keys.Content = append(keys.Content, e)
	}
	add("keys", keys)
	return &yaml.Node{Kind: yaml.DocumentNode, Content: []*yaml.Node{m}}, nil
}

func mapping() *yaml.Node { return &yaml.Node{Kind: yaml.MappingNode} }

// str is a string node. The encoder quotes it when its text would read back as another type, and
// writes text with a newline as a block scalar. Some text a block scalar cannot hold exactly
// (leading blank lines, a carriage return, a Unicode line break), and yaml.v3 does not always
// notice, so text whose block form does not read back to it is double-quoted instead.
func str(s string) *yaml.Node {
	n := &yaml.Node{Kind: yaml.ScalarNode, Tag: "!!str", Value: s}
	if strings.ContainsAny(s, "\r\u0085\u2028\u2029") || (strings.Contains(s, "\n") && !blockReadsBack(s)) {
		n.Style = yaml.DoubleQuotedStyle
	}
	return n
}

// blockReadsBack reports whether s, written by the encoder as a mapping value, reads back to s.
func blockReadsBack(s string) bool {
	data, err := yaml.Marshal(map[string]string{"v": s})
	if err != nil {
		return false
	}
	var back map[string]string
	return yaml.Unmarshal(data, &back) == nil && back["v"] == s
}

func strs(ss []string) *yaml.Node {
	n := &yaml.Node{Kind: yaml.SequenceNode, Style: yaml.FlowStyle}
	for _, s := range ss {
		n.Content = append(n.Content, str(s))
	}
	return n
}

func sortedKeys[V any](m map[string]V) []string {
	keys := make([]string, 0, len(m))
	for k := range m {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	return keys
}

// typedNode writes a decoded typed value (case.md §7) as the node decodeTypedValue reads back to
// it. Floats use the JSON form of record.md §3.1, so a whole float keeps its `.0`.
func typedNode(v interface{}) (*yaml.Node, error) {
	scalar := func(tag, value string) *yaml.Node {
		return &yaml.Node{Kind: yaml.ScalarNode, Tag: tag, Value: value}
	}
	switch v := v.(type) {
	case nil:
		return scalar("!!null", "null"), nil
	case bool:
		return scalar("!!bool", strconv.FormatBool(v)), nil
	case int:
		return scalar("!!int", strconv.Itoa(v)), nil
	case float64:
		if math.IsNaN(v) || math.IsInf(v, 0) {
			return nil, fmt.Errorf("%w: non-finite float", ErrCaseWrite)
		}
		var buf bytes.Buffer
		if err := encodeFloat(&buf, v, 64); err != nil {
			return nil, err
		}
		return scalar("!!float", buf.String()), nil
	case string:
		return str(v), nil
	case []interface{}:
		n := &yaml.Node{Kind: yaml.SequenceNode}
		for _, e := range v {
			en, err := typedNode(e)
			if err != nil {
				return nil, err
			}
			n.Content = append(n.Content, en)
		}
		return n, nil
	case map[string]interface{}:
		n := mapping()
		for _, k := range sortedKeys(v) {
			en, err := typedNode(v[k])
			if err != nil {
				return nil, err
			}
			n.Content = append(n.Content, str(k), en)
		}
		return n, nil
	}
	return nil, fmt.Errorf("%w: unsupported typed value type %T", ErrCaseWrite, v)
}
