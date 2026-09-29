// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package corpus

import (
	"errors"
	"reflect"
	"testing"
)

func TestSelectGetters(t *testing.T) {
	tests := []struct {
		typ  string
		tags SchemaTags
		want []string
	}{
		{"bool", SchemaTags{}, []string{"GetBool"}},
		{"int", SchemaTags{GolangType: "duration"}, []string{"GetInt", "GetDuration"}},
		{"float64", SchemaTags{GolangType: "duration"}, []string{"GetFloat64", "GetDuration"}},
		{"string", SchemaTags{Format: "duration"}, []string{"GetString", "GetDuration"}},
		{"time.Duration", SchemaTags{Format: "duration", GolangType: "duration"}, []string{"GetDuration"}},
		{"string", SchemaTags{GolangType: "duration"}, []string{"GetString"}},
		{"int", SchemaTags{Format: "duration"}, []string{"GetInt"}},
		{"[]int", SchemaTags{}, []string{"Get", "GetStringSlice"}},
		{"[]map[string]string", SchemaTags{}, []string{"Get"}},
		{"[]map[string]interface {}", SchemaTags{}, []string{"Get"}},
		{"map[string]float64", SchemaTags{}, []string{"GetStringMap"}},
		{"<nil>", SchemaTags{}, []string{"Get"}},
	}
	for _, tc := range tests {
		got, err := selectGetters(tc.typ, tc.tags)
		if err != nil || !reflect.DeepEqual(got, tc.want) {
			t.Errorf("%s %+v: got %v, %v; want %v", tc.typ, tc.tags, got, err, tc.want)
		}
	}
	if _, err := selectGetters("[]bool", SchemaTags{}); !errors.Is(err, ErrGetterDefaultType) {
		t.Errorf("[]bool: %v", err)
	}
	for typ, getters := range primaryGetters {
		for _, g := range getters {
			if !isGetterName(g) {
				t.Errorf("%s maps to unknown getter %s", typ, g)
			}
		}
	}
}

func TestGettersForKey(t *testing.T) {
	override := KeyEntry{Key: "k", Getters: []string{"GetInt"}}
	if got, _ := GettersForKey(override, KeyLeaf, true, "string", "", "", SchemaTags{}); !reflect.DeepEqual(got, []string{"GetInt"}) {
		t.Errorf("override: %v", got)
	}
	for _, kind := range []KeyKind{KeyUnknown, KeySection} {
		if got, _ := GettersForKey(KeyEntry{Key: "k"}, kind, true, "", "", "", SchemaTags{}); !reflect.DeepEqual(got, []string{"Get"}) {
			t.Errorf("kind %d: %v", kind, got)
		}
	}
	// A leaf with no default falls back to the declared-type table (getter-map.md §1.1).
	if got, _ := GettersForKey(KeyEntry{Key: "k"}, KeyLeaf, false, "", "boolean", "", SchemaTags{}); !reflect.DeepEqual(got, []string{"Get", "GetBool"}) {
		t.Errorf("no default: %v", got)
	}
}

// TestSelectGettersNoDefault covers every row of getter-map.md §1.1's table.
func TestSelectGettersNoDefault(t *testing.T) {
	tests := []struct {
		typ, elem string
		tags      SchemaTags
		want      []string
	}{
		{"boolean", "", SchemaTags{}, []string{"Get", "GetBool"}},
		{"integer", "", SchemaTags{}, []string{"Get", "GetInt"}},
		{"number", "", SchemaTags{}, []string{"Get", "GetFloat64"}},
		{"string", "", SchemaTags{}, []string{"Get", "GetString"}},
		{"array", "string", SchemaTags{}, []string{"Get", "GetStringSlice"}},
		{"array", "number", SchemaTags{}, []string{"Get", "GetFloat64Slice"}},
		{"array", "", SchemaTags{}, []string{"Get"}},
		{"array", "object", SchemaTags{}, []string{"Get"}},
		{"object", "string", SchemaTags{}, []string{"Get", "GetStringMapString"}},
		{"object", "array_of_string", SchemaTags{}, []string{"Get", "GetStringMapStringSlice"}},
		{"object", "", SchemaTags{}, []string{"Get", "GetStringMap"}},
		{"object", "integer", SchemaTags{}, []string{"Get", "GetStringMap"}},
		{"", "", SchemaTags{}, []string{"Get"}},
		{"string", "", SchemaTags{Format: "duration"}, []string{"Get", "GetString", "GetDuration"}},
		{"integer", "", SchemaTags{GolangType: "duration"}, []string{"Get", "GetInt", "GetDuration"}},
		{"number", "", SchemaTags{GolangType: "duration"}, []string{"Get", "GetFloat64", "GetDuration"}},
	}
	for _, tc := range tests {
		got := selectGettersNoDefault(tc.typ, tc.elem, tc.tags)
		if !reflect.DeepEqual(got, tc.want) {
			t.Errorf("%s/%s %+v: got %v, want %v", tc.typ, tc.elem, tc.tags, got, tc.want)
		}
		for _, g := range got {
			if !isGetterName(g) {
				t.Errorf("%s/%s: unknown getter %s", tc.typ, tc.elem, g)
			}
		}
	}
}

func TestMentions(t *testing.T) {
	tests := []struct {
		s, name string
		want    bool
	}{
		{`key "logs_enabled": bad`, "logs_enabled", true},
		{"logs_enabled", "logs_enabled", true},
		{"logs_enabled_extra is set", "logs_enabled", false},
		{"logs.logs_enabled is set", "logs_enabled", false},
		{"xlogs_enabled", "logs_enabled", false},
		{"logs_enabled2 and logs_enabled.", "logs_enabled", false},
		{"logs_enabled2 and (logs_enabled)", "logs_enabled", true},
		{"DD_API_KEY=", "DD_API_KEY", true},
		{"anything", "", false},
	}
	for _, tc := range tests {
		if got := mentions(tc.s, tc.name); got != tc.want {
			t.Errorf("mentions(%q, %q) = %v", tc.s, tc.name, got)
		}
	}

	c := mustParse(t, exampleLayersCase)
	ws := []Warning{
		{"WARN", "dogstatsd_port overridden"},
		{"WARN", "unrelated"},
		{"ERROR", "bad cmd_port"},
		{"WARN", "cmd_ports"},
	}
	got := FilterWarnings(ws, CaseWarningNames(c))
	want := []Warning{ws[0], ws[2]}
	if !reflect.DeepEqual(got, want) {
		t.Errorf("filter: %v", got)
	}
}
