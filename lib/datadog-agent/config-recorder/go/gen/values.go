// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package gen

import (
	"encoding/json"
	"fmt"
	"math"
	"reflect"
	"sort"
	"strconv"
	"strings"
)

// Values is the pair of generated values for one key: Input is written to YAML (and rendered for
// env), Set is the breadth case's `set` update.
type Values struct {
	Input interface{}
	Set   interface{}
}

// ValuesFor chooses a key's values by rule from its schema type, default and format:
//
//   - boolean: Input is the negated default, Set is the default (a bool has only two values, so
//     Set cannot differ from both).
//   - integer: Input is default+1, Set is default+2 (a missing default counts as 0).
//   - number: Input is floor(default)+1.5, Set is floor(default)+2.5, so both always have a
//     fraction and are recorded as floats, whatever fraction the default has.
//   - string with `format: duration`: Input `61s`, Set `62s`; `71s` and `72s` if the default is
//     one of those.
//   - other string, or no type: Input `cr-a`, Set `cr-b`; `cr-c` and `cr-d` if the default is one
//     of those.
//   - array: two items for Input and one other item for Set, of the `items` type: strings `cr-a`,
//     `cr-b` / `cr-c`; integers 1, 2 / 3; numbers 1.5, 2.5 / 3.5; objects {cr_a: cr-a} / {cr_b:
//     cr-b}; no or another type as strings.
//   - object: one entry of the `additionalProperties` type: {cr_a: cr-a} / {cr_b: cr-b}; lists
//     {cr_a: [cr-a]} / {cr_b: [cr-b]}; integers {cr_a: 1} / {cr_b: 2}; numbers {cr_a: 1.5} /
//     {cr_b: 2.5}; no or another type as strings. With `env_parser: traces_span` the entry keys
//     are `service|operation` pairs and the values rates, the form that parser reads.
//
// A value equal to the default where the rule says it must differ is an error.
func ValuesFor(s *Setting) (Values, error) {
	var v Values
	switch s.Type {
	case "boolean":
		d, _ := s.Default.(bool)
		v = Values{Input: !d, Set: d}
	case "integer":
		d, err := intDefault(s)
		if err != nil {
			return v, err
		}
		v = Values{Input: d + 1, Set: d + 2}
	case "number":
		d, err := floatDefault(s)
		if err != nil {
			return v, err
		}
		f := math.Floor(d)
		v = Values{Input: f + 1.5, Set: f + 2.5}
	case "array":
		v = arrayValues(s.ItemType)
	case "object":
		v = objectValues(s.ItemType, s.ItemItemType, s.EnvParser == "traces_span")
	default:
		pairs := [][2]string{{"cr-a", "cr-b"}, {"cr-c", "cr-d"}}
		if s.Type == "string" && s.Format == "duration" {
			pairs = [][2]string{{"61s", "62s"}, {"71s", "72s"}}
		}
		d, _ := s.Default.(string)
		p := pairs[0]
		if d == p[0] || d == p[1] {
			p = pairs[1]
		}
		v = Values{Input: p[0], Set: p[1]}
	}
	if sameJSON(v.Input, s.Default) {
		return v, fmt.Errorf("key %q: generated value %v equals the default", s.Key, v.Input)
	}
	if sameJSON(v.Input, v.Set) || (s.Type != "boolean" && sameJSON(v.Set, s.Default)) {
		return v, fmt.Errorf("key %q: generated set value %v is not distinct", s.Key, v.Set)
	}
	return v, nil
}

func intDefault(s *Setting) (int, error) {
	switch d := s.Default.(type) {
	case nil:
		return 0, nil
	case int:
		return d, nil
	}
	return 0, fmt.Errorf("key %q: integer default %v is not an integer", s.Key, s.Default)
}

func floatDefault(s *Setting) (float64, error) {
	switch d := s.Default.(type) {
	case nil:
		return 0, nil
	case int:
		return float64(d), nil
	case float64:
		return d, nil
	}
	return 0, fmt.Errorf("key %q: number default %v is not a number", s.Key, s.Default)
}

func arrayValues(item string) Values {
	switch item {
	case "integer":
		return Values{Input: []interface{}{1, 2}, Set: []interface{}{3}}
	case "number":
		return Values{Input: []interface{}{1.5, 2.5}, Set: []interface{}{3.5}}
	case "object":
		return Values{
			Input: []interface{}{map[string]interface{}{"cr_a": "cr-a"}},
			Set:   []interface{}{map[string]interface{}{"cr_b": "cr-b"}},
		}
	}
	return Values{Input: []interface{}{"cr-a", "cr-b"}, Set: []interface{}{"cr-c"}}
}

func objectValues(item, itemItem string, tracesSpan bool) Values {
	if tracesSpan {
		return Values{Input: map[string]interface{}{"cr-a|cr-b": 0.5}, Set: map[string]interface{}{"cr-c|cr-d": 0.25}}
	}
	var a, b interface{} = "cr-a", "cr-b"
	switch {
	case item == "array" && (itemItem == "string" || itemItem == ""):
		a, b = []interface{}{"cr-a"}, []interface{}{"cr-b"}
	case item == "integer":
		a, b = 1, 2
	case item == "number":
		a, b = 1.5, 2.5
	}
	return Values{Input: map[string]interface{}{"cr_a": a}, Set: map[string]interface{}{"cr_b": b}}
}

func sameJSON(a, b interface{}) bool {
	ja, err1 := json.Marshal(a)
	jb, err2 := json.Marshal(b)
	if err1 != nil || err2 != nil {
		return reflect.DeepEqual(a, b)
	}
	return string(ja) == string(jb)
}

// commaParsers are the env parsers that split a list on commas; every other parser, and none,
// is covered by EnvText's other rules.
var commaParsers = map[string]bool{
	"comma_separated":              true,
	"csv_comma_separated":          true,
	"json_list_or_comma_separated": true,
	"comma_then_space_separated":   true,
}

// EnvText renders v as the text the Agent's env path expects for the key:
//
//   - scalars: their text (`true`, `42`, `1.5`, `61s`), which the typed getters parse.
//   - lists: with `env_parser: json`, JSON text; with a comma parser (`comma_separated`,
//     `csv_comma_separated`, `json_list_or_comma_separated`, `comma_then_space_separated`), items
//     joined by `,`; otherwise (no parser, `space_separated`, `comma_and_space_separated`,
//     `json_list_or_space_separated`) items joined by a space, which is also how the Agent splits
//     a list read from an unparsed env string. A list of objects has no split form and is JSON.
//   - maps: JSON text, except `env_parser: traces_span`, which reads `service|operation=rate`
//     entries joined by `,`.
func EnvText(s *Setting, v interface{}) (string, error) {
	switch x := v.(type) {
	case bool:
		return strconv.FormatBool(x), nil
	case int:
		return strconv.Itoa(x), nil
	case float64:
		return strconv.FormatFloat(x, 'g', -1, 64), nil
	case string:
		return x, nil
	case []interface{}:
		objects := false
		items := make([]string, len(x))
		for i, e := range x {
			switch e.(type) {
			case map[string]interface{}:
				objects = true
			default:
				t, err := EnvText(s, e)
				if err != nil {
					return "", err
				}
				items[i] = t
			}
		}
		if s.EnvParser == "json" || objects {
			return jsonText(x)
		}
		if commaParsers[s.EnvParser] {
			return strings.Join(items, ","), nil
		}
		return strings.Join(items, " "), nil
	case map[string]interface{}:
		if s.EnvParser == "traces_span" {
			keys := make([]string, 0, len(x))
			for k := range x {
				keys = append(keys, k)
			}
			sort.Strings(keys)
			parts := make([]string, len(keys))
			for i, k := range keys {
				t, err := EnvText(s, x[k])
				if err != nil {
					return "", err
				}
				parts[i] = k + "=" + t
			}
			return strings.Join(parts, ","), nil
		}
		return jsonText(x)
	}
	return "", fmt.Errorf("key %q: no env text for %T", s.Key, v)
}

func jsonText(v interface{}) (string, error) {
	b, err := json.Marshal(v)
	return string(b), err
}
