// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package agentcfg

import (
	"errors"
	"reflect"
	"testing"
	"time"

	"github.com/DataDog/datadog-agent/pkg/config/model"

	"github.com/DataDog/datadog-agent/cmd/config-recorder/record"
	"github.com/DataDog/datadog-agent/cmd/config-recorder/schema"
)

// stubReader is a model.Reader that answers every method with its zero value. It exists only so
// tests can call CallGetter without building a real Agent configuration.
type stubReader struct{}

func (stubReader) Get(string) interface{}                                     { return nil }
func (stubReader) GetString(string) string                                    { return "" }
func (stubReader) GetBool(string) bool                                        { return false }
func (stubReader) GetInt(string) int                                          { return 0 }
func (stubReader) GetInt32(string) int32                                      { return 0 }
func (stubReader) GetInt64(string) int64                                      { return 0 }
func (stubReader) GetFloat64(string) float64                                  { return 0 }
func (stubReader) GetDuration(string) time.Duration                           { return 0 }
func (stubReader) GetStringSlice(string) []string                             { return nil }
func (stubReader) GetFloat64Slice(string) []float64                           { return nil }
func (stubReader) GetStringMap(string) map[string]interface{}                 { return nil }
func (stubReader) GetStringMapString(string) map[string]string                { return nil }
func (stubReader) GetStringMapStringSlice(string) map[string][]string         { return nil }
func (stubReader) GetSizeInBytes(string) uint                                 { return 0 }
func (stubReader) GetProxies() *model.Proxy                                   { return nil }
func (stubReader) GetSequenceID() uint64                                      { return 0 }
func (stubReader) GetSource(string) model.Source                              { return "" }
func (stubReader) GetAllSources(string) []model.ValueWithSource               { return nil }
func (stubReader) ConfigFileUsed() string                                     { return "" }
func (stubReader) ExtraConfigFilesUsed() []string                             { return nil }
func (stubReader) AllSettings() map[string]interface{}                        { return nil }
func (stubReader) AllSettingsWithoutDefault() map[string]interface{}          { return nil }
func (stubReader) AllSettingsWithoutSecrets() map[string]interface{}          { return nil }
func (stubReader) AllSettingsWithoutDefaultOrSecrets() map[string]interface{} { return nil }
func (stubReader) AllSettingsBySource() map[model.Source]interface{}          { return nil }
func (stubReader) AllKeysLowercased() []string                                { return nil }
func (stubReader) AllFlattenedSettingsWithSequenceID() (map[string]interface{}, uint64) {
	return nil, 0
}
func (stubReader) SetTestOnlyDynamicSchema(bool)                           {}
func (stubReader) IsConfigured(string) bool                                { return false }
func (stubReader) HasSection(string) bool                                  { return false }
func (stubReader) IsKnown(string) bool                                     { return false }
func (stubReader) IsSetting(string) bool                                   { return false }
func (stubReader) GetEnvVars() []string                                    { return nil }
func (stubReader) Warnings() []string                                      { return nil }
func (stubReader) StartTime() time.Time                                    { return time.Time{} }
func (r stubReader) Object() model.Reader                                  { return r }
func (stubReader) OnUpdate(model.NotificationReceiver)                     {}
func (stubReader) Stringify(model.Source, ...model.StringifyOption) string { return "" }

func TestSelectGetters(t *testing.T) {
	tests := []struct {
		typ  string
		tags Tags
		want []string
	}{
		{"bool", Tags{}, []string{"GetBool"}},
		{"int", Tags{GolangType: "duration"}, []string{"GetInt", "GetDuration"}},
		{"float64", Tags{GolangType: "duration"}, []string{"GetFloat64", "GetDuration"}},
		{"string", Tags{Format: "duration"}, []string{"GetString", "GetDuration"}},
		{"time.Duration", Tags{Format: "duration", GolangType: "duration"}, []string{"GetDuration"}},
		{"string", Tags{GolangType: "duration"}, []string{"GetString"}},
		{"int", Tags{Format: "duration"}, []string{"GetInt"}},
		{"[]int", Tags{}, []string{"Get", "GetStringSlice"}},
		{"[]map[string]string", Tags{}, []string{"Get"}},
		{"[]map[string]interface {}", Tags{}, []string{"Get"}},
		{"map[string]float64", Tags{}, []string{"GetStringMap"}},
		{"<nil>", Tags{}, []string{"Get"}},
	}
	for _, tc := range tests {
		got, err := selectGetters(tc.typ, tc.tags)
		if err != nil || !reflect.DeepEqual(got, tc.want) {
			t.Errorf("%s %+v: got %v, %v; want %v", tc.typ, tc.tags, got, err, tc.want)
		}
	}
	if _, err := selectGetters("[]bool", Tags{}); !errors.Is(err, ErrGetterDefaultType) {
		t.Errorf("[]bool: %v", err)
	}
	for typ, getters := range primaryGetters {
		for _, g := range getters {
			if !record.IsGetterName(g) {
				t.Errorf("%s maps to unknown getter %s", typ, g)
			}
		}
	}
}

// TestCallGetterHandlesGetterNames checks that CallGetter dispatches every name in
// record.GetterNames (and only those): an unlisted name, including another Reader method that
// CallGetter does not wrap, must come back as ErrGetterName.
func TestCallGetterHandlesGetterNames(t *testing.T) {
	cfg := stubReader{}
	for _, name := range record.GetterNames {
		if _, err := CallGetter(cfg, name, "some.key"); errors.Is(err, ErrGetterName) {
			t.Errorf("CallGetter does not handle listed getter %s", name)
		}
	}
	for _, name := range []string{"GetProxies", "GetSequenceID", "GetSource", "GetAllSources", "getstring", "Bogus", ""} {
		if _, err := CallGetter(cfg, name, "some.key"); !errors.Is(err, ErrGetterName) {
			t.Errorf("CallGetter handled unlisted name %s: %v", name, err)
		}
	}
}

func TestGettersForKey(t *testing.T) {
	override := record.KeyEntry{Key: "k", Getters: []string{"GetInt"}}
	if got, _ := GettersForKey(override, &schema.Key{Kind: schema.Leaf}, true, "string"); !reflect.DeepEqual(got, []string{"GetInt"}) {
		t.Errorf("override: %v", got)
	}
	for _, kind := range []schema.Kind{schema.Unknown, schema.Section} {
		if got, _ := GettersForKey(record.KeyEntry{Key: "k"}, &schema.Key{Kind: kind}, true, ""); !reflect.DeepEqual(got, []string{"Get"}) {
			t.Errorf("kind %d: %v", kind, got)
		}
	}
	// A leaf with no default falls back to the declared-type table (getter-map.md §1.1).
	if got, _ := GettersForKey(record.KeyEntry{Key: "k"}, &schema.Key{Kind: schema.Leaf, Type: "boolean"}, false, ""); !reflect.DeepEqual(got, []string{"Get", "GetBool"}) {
		t.Errorf("no default: %v", got)
	}
}

// TestSelectGettersNoDefault covers every row of getter-map.md §1.1's table.
func TestSelectGettersNoDefault(t *testing.T) {
	tests := []struct {
		typ, elem string
		tags      Tags
		want      []string
	}{
		{"boolean", "", Tags{}, []string{"Get", "GetBool"}},
		{"integer", "", Tags{}, []string{"Get", "GetInt"}},
		{"number", "", Tags{}, []string{"Get", "GetFloat64"}},
		{"string", "", Tags{}, []string{"Get", "GetString"}},
		{"array", "string", Tags{}, []string{"Get", "GetStringSlice"}},
		{"array", "number", Tags{}, []string{"Get", "GetFloat64Slice"}},
		{"array", "", Tags{}, []string{"Get"}},
		{"array", "object", Tags{}, []string{"Get"}},
		{"object", "string", Tags{}, []string{"Get", "GetStringMapString"}},
		{"object", "array_of_string", Tags{}, []string{"Get", "GetStringMapStringSlice"}},
		{"object", "", Tags{}, []string{"Get", "GetStringMap"}},
		{"object", "integer", Tags{}, []string{"Get", "GetStringMap"}},
		{"", "", Tags{}, []string{"Get"}},
		{"string", "", Tags{Format: "duration"}, []string{"Get", "GetString", "GetDuration"}},
		{"integer", "", Tags{GolangType: "duration"}, []string{"Get", "GetInt", "GetDuration"}},
		{"number", "", Tags{GolangType: "duration"}, []string{"Get", "GetFloat64", "GetDuration"}},
	}
	for _, tc := range tests {
		got := selectGettersNoDefault(tc.typ, tc.elem, tc.tags)
		if !reflect.DeepEqual(got, tc.want) {
			t.Errorf("%s/%s %+v: got %v, want %v", tc.typ, tc.elem, tc.tags, got, tc.want)
		}
		for _, g := range got {
			if !record.IsGetterName(g) {
				t.Errorf("%s/%s: unknown getter %s", tc.typ, tc.elem, g)
			}
		}
	}
}
