// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package agentcfg

import (
	"bytes"
	"errors"
	"reflect"
	"testing"
	"time"

	"github.com/DataDog/datadog-agent/pkg/config/model"
	"github.com/DataDog/datadog-agent/pkg/config/nodetreemodel"

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

// sectionReader is a stubReader holding one section's stored value, with the given leaves
// configured and the given sections declared, as the Agent's IsConfigured and HasSection report.
type sectionReader struct {
	stubReader
	section    string
	value      map[string]interface{}
	configured map[string]bool
	declared   map[string]bool
}

func (r sectionReader) Get(key string) interface{} {
	if key == r.section {
		return r.value
	}
	return nil
}
func (r sectionReader) IsConfigured(key string) bool { return r.configured[key] }
func (r sectionReader) HasSection(key string) bool   { return r.declared[key] }

// TestCallGetterReadConfigSection checks that the section read keeps configured leaves and
// declared sections, drops defaults, and returns the nested map.
func TestCallGetterReadConfigSection(t *testing.T) {
	cfg := sectionReader{
		section: "otlp_config.receiver",
		value: map[string]interface{}{
			"protocols": map[string]interface{}{
				"grpc": map[string]interface{}{"endpoint": "0.0.0.0:4417", "transport": "tcp"},
				"http": map[string]interface{}{"endpoint": "0.0.0.0:4318"},
			},
		},
		configured: map[string]bool{"otlp_config.receiver.protocols.grpc.endpoint": true},
		declared:   map[string]bool{"otlp_config.receiver.protocols.http": true},
	}
	got, err := CallGetter(cfg, "ReadConfigSection", "otlp_config.receiver")
	if err != nil {
		t.Fatal(err)
	}
	want := map[string]interface{}{
		"protocols": map[string]interface{}{
			"grpc": map[string]interface{}{"endpoint": "0.0.0.0:4417"},
			"http": nil,
		},
	}
	if !reflect.DeepEqual(got, want) {
		t.Errorf("got %#v; want %#v", got, want)
	}
	if _, err := record.EncodeResult(got); err != nil {
		t.Errorf("encode: %v", err)
	}
}

// TestCallGetterIsConfigured checks IsConfigured against a real Agent config (not a stub), since
// getter-map.md §2.1 records the Agent's own answer rather than a rule saluki re-derives: a key
// set by the user to its default value, a key the user declared with a YAML null value, a section
// with one configured child, and an untouched key.
//
// null_value must be set by reading real YAML with an empty scalar (as TestCompareEmptyLeafSetting
// in nodetreemodel/compatibility_test.go does), not with Set(key, nil, ...): the Agent converts a
// nil passed to Set into the default's zero value (here ""), which is non-nil, so IsConfigured
// would report true instead of the false this test needs.
func TestCallGetterIsConfigured(t *testing.T) {
	cfg := nodetreemodel.NewNodeTreeConfig("test-is-configured", "TEST_IS_CONFIGURED", nil)
	cfg.SetDefault("default_value", "x")
	cfg.SetDefault("null_value", "x")
	cfg.SetDefault("section.configured_child", "x")
	cfg.SetDefault("section.other_child", "x")
	cfg.SetDefault("untouched", "x")
	cfg.BuildSchema()

	cfg.SetConfigType("yaml")
	yamlInput := "default_value: x\nnull_value:\nsection:\n  configured_child: y\n"
	if err := cfg.ReadConfig(bytes.NewBufferString(yamlInput)); err != nil {
		t.Fatal(err)
	}

	want := map[string]bool{
		"default_value": true,
		"null_value":    false,
		"section":       true,
		"untouched":     false,
	}
	for key, w := range want {
		got, err := CallGetter(cfg, "IsConfigured", key)
		if err != nil {
			t.Fatalf("IsConfigured(%q): %v", key, err)
		}
		if got != w {
			t.Errorf("IsConfigured(%q) = %v, want %v", key, got, w)
		}
	}
}

// TestCallGetterReadConfigSectionEmpty checks that a section with no configured leaves and no
// declared child sections reads back as {}, and that the recorder's non-map check accepts it
// (an empty map is still a map).
func TestCallGetterReadConfigSectionEmpty(t *testing.T) {
	cfg := sectionReader{
		section:    "otlp_config.receiver",
		value:      map[string]interface{}{"protocols": map[string]interface{}{}},
		configured: map[string]bool{},
		declared:   map[string]bool{},
	}
	got, err := CallGetter(cfg, "ReadConfigSection", "otlp_config.receiver")
	if err != nil {
		t.Fatal(err)
	}
	if want := map[string]interface{}{}; !reflect.DeepEqual(got, want) {
		t.Errorf("got %#v; want %#v", got, want)
	}
	if _, err := record.EncodeResult(got); err != nil {
		t.Errorf("encode: %v", err)
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
