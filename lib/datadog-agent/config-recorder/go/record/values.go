// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package record

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"math"
	"sort"
	"strconv"
	"strings"

	"gopkg.in/yaml.v3"
)

// Errors for typed values that have no Go form the Agent calls accept.
var (
	ErrTypedIntRange  = errors.New("typed value: integer outside the int64 range")
	ErrTypedNonFinite = errors.New("typed value: non-finite float")
	ErrTypedMapKey    = errors.New("typed value: mapping key is not a string")
	ErrTypedTag       = errors.New("typed value: unsupported YAML tag")
	ErrTypedDupKey    = errors.New("typed value: mapping key given twice")
	ErrTypedJSON      = errors.New("typed value: invalid JSON")
)

// decodeTypedValue turns a `cli[].value` or `updates[].value` node into the Go value passed to the
// Agent. The resolved YAML tag alone decides the Go type, so a case author controls it exactly.
func decodeTypedValue(n *yaml.Node) (interface{}, error) {
	if n.Kind == yaml.AliasNode {
		return decodeTypedValue(n.Alias)
	}
	if n.Kind == yaml.DocumentNode && len(n.Content) == 1 {
		return decodeTypedValue(n.Content[0])
	}
	switch n.Kind {
	case yaml.SequenceNode:
		out := make([]interface{}, 0, len(n.Content))
		for _, c := range n.Content {
			v, err := decodeTypedValue(c)
			if err != nil {
				return nil, err
			}
			out = append(out, v)
		}
		return out, nil
	case yaml.MappingNode:
		out := make(map[string]interface{}, len(n.Content)/2)
		for i := 0; i+1 < len(n.Content); i += 2 {
			k := n.Content[i]
			if k.Kind == yaml.AliasNode {
				k = k.Alias
			}
			if k.Kind != yaml.ScalarNode || k.ShortTag() != "!!str" {
				return nil, fmt.Errorf("%w: line %d", ErrTypedMapKey, k.Line)
			}
			if _, dup := out[k.Value]; dup {
				return nil, fmt.Errorf("%w: %q", ErrTypedDupKey, k.Value)
			}
			v, err := decodeTypedValue(n.Content[i+1])
			if err != nil {
				return nil, err
			}
			out[k.Value] = v
		}
		return out, nil
	case yaml.ScalarNode:
	default:
		return nil, fmt.Errorf("%w: line %d", ErrTypedTag, n.Line)
	}

	switch tag := n.ShortTag(); tag {
	case "!!str":
		return n.Value, nil
	case "!!null":
		return nil, nil
	case "!!bool":
		var b bool
		if err := n.Decode(&b); err != nil {
			return nil, fmt.Errorf("%w: %v", ErrTypedTag, err)
		}
		return b, nil
	case "!!int":
		var i int64
		if err := n.Decode(&i); err != nil {
			// yaml.v3 fails to decode an in-range `!!int` only on overflow.
			return nil, fmt.Errorf("%w: %s", ErrTypedIntRange, n.Value)
		}
		return int(i), nil
	case "!!float":
		// yaml.v3 resolves an integer literal beyond uint64 as a float; its JSON form would be an
		// integer, so treat it as the out-of-range integer it is.
		if n.Style&yaml.TaggedStyle == 0 && isIntegerLiteral(n.Value) {
			return nil, fmt.Errorf("%w: %s", ErrTypedIntRange, n.Value)
		}
		var f float64
		if err := n.Decode(&f); err != nil {
			return nil, fmt.Errorf("%w: %v", ErrTypedTag, err)
		}
		if math.IsNaN(f) || math.IsInf(f, 0) {
			return nil, fmt.Errorf("%w: %s", ErrTypedNonFinite, n.Value)
		}
		return f, nil
	default:
		return nil, fmt.Errorf("%w: %s on line %d", ErrTypedTag, tag, n.Line)
	}
}

func isIntegerLiteral(s string) bool {
	s = strings.TrimLeft(s, "+-")
	if s == "" {
		return false
	}
	for _, r := range s {
		if (r < '0' || r > '9') && r != '_' {
			return false
		}
	}
	return true
}

// decodeTypedJSON decodes a typed value from its JSON form in a case line's `inputs`, giving the
// same Go value decodeTypedValue gives for the YAML form, so a case line can be replayed.
func decodeTypedJSON(data []byte) (interface{}, error) {
	dec := json.NewDecoder(bytes.NewReader(data))
	dec.UseNumber()
	var v interface{}
	if err := dec.Decode(&v); err != nil {
		return nil, fmt.Errorf("%w: %v", ErrTypedJSON, err)
	}
	if _, err := dec.Token(); !errors.Is(err, io.EOF) {
		return nil, fmt.Errorf("%w: trailing data", ErrTypedJSON)
	}
	return typedFromJSON(v)
}

func typedFromJSON(v interface{}) (interface{}, error) {
	switch v := v.(type) {
	case json.Number:
		s := v.String()
		if !strings.ContainsAny(s, ".eE") {
			i, err := strconv.ParseInt(s, 10, 64)
			if err != nil {
				return nil, fmt.Errorf("%w: %s", ErrTypedIntRange, s)
			}
			return int(i), nil
		}
		f, err := strconv.ParseFloat(s, 64)
		if err != nil {
			return nil, fmt.Errorf("%w: %s", ErrTypedNonFinite, s)
		}
		return f, nil
	case []interface{}:
		for i := range v {
			e, err := typedFromJSON(v[i])
			if err != nil {
				return nil, err
			}
			v[i] = e
		}
		return v, nil
	case map[string]interface{}:
		for k := range v {
			e, err := typedFromJSON(v[k])
			if err != nil {
				return nil, err
			}
			v[k] = e
		}
		return v, nil
	default:
		// string, bool and nil already have their Go form.
		return v, nil
	}
}

// encodeTypedValue writes a decoded typed value in its JSON form (case.md §7, record.md §3.1). A
// typed value can hold only the Go types decodeTypedValue and typedFromJSON produce: it can never
// be a non-finite float, so it does not go through EncodeResult's getter-result encoder, whose
// "$float" collision guard exists only to keep getter-map.md §3's non-finite sentinel unambiguous.
// A typed value's own number rules are otherwise the same as a getter result's.
func encodeTypedValue(v interface{}) (json.RawMessage, error) {
	var buf bytes.Buffer
	if err := encodeTypedValueInto(&buf, v); err != nil {
		return nil, err
	}
	return buf.Bytes(), nil
}

func encodeTypedValueInto(buf *bytes.Buffer, v interface{}) error {
	switch v := v.(type) {
	case nil:
		buf.WriteString("null")
	case bool:
		buf.WriteString(strconv.FormatBool(v))
	case int:
		buf.WriteString(strconv.Itoa(v))
	case float64:
		return encodeFloat(buf, v, 64)
	case string:
		return encodeString(buf, v)
	case []interface{}:
		buf.WriteByte('[')
		for i, e := range v {
			if i > 0 {
				buf.WriteByte(',')
			}
			if err := encodeTypedValueInto(buf, e); err != nil {
				return err
			}
		}
		buf.WriteByte(']')
	case map[string]interface{}:
		keys := make([]string, 0, len(v))
		for k := range v {
			keys = append(keys, k)
		}
		sort.Strings(keys)
		buf.WriteByte('{')
		for i, k := range keys {
			if i > 0 {
				buf.WriteByte(',')
			}
			if err := encodeString(buf, k); err != nil {
				return err
			}
			buf.WriteByte(':')
			if err := encodeTypedValueInto(buf, v[k]); err != nil {
				return err
			}
		}
		buf.WriteByte('}')
	default:
		return fmt.Errorf("%w: unsupported type %T", ErrResultEncoding, v)
	}
	return nil
}
