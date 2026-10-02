// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

// Package agentcfg is the config recorder's code that reads the Agent's config: getter selection
// and calls, stream proto conversion, and the check that the schema's key set is the Agent's.
package agentcfg

import (
	"errors"
	"fmt"
	"strings"

	"github.com/DataDog/datadog-agent/comp/otelcol/otlp/configcheck"
	"github.com/DataDog/datadog-agent/pkg/config/model"

	"github.com/DataDog/datadog-agent/cmd/config-recorder/record"
	"github.com/DataDog/datadog-agent/cmd/config-recorder/schema"
)

// Errors for getter selection and dispatch.
var (
	ErrGetterDefaultType = errors.New("no getter rule for the default-layer Go type")
	ErrGetterName        = errors.New("unknown getter name")
	ErrSectionRead       = errors.New("section read did not return a map")
)

// primaryGetters maps the `%T` of a key's default-layer value to its default getter list.
var primaryGetters = map[string][]string{
	"bool":                    {"GetBool"},
	"int":                     {"GetInt"},
	"int64":                   {"GetInt64"},
	"float64":                 {"GetFloat64"},
	"string":                  {"GetString"},
	"time.Duration":           {"GetDuration"},
	"[]string":                {"GetStringSlice"},
	"[]int":                   {"Get", "GetStringSlice"},
	"[]float64":               {"GetFloat64Slice"},
	"[]interface {}":          {"Get"},
	"map[string]interface {}": {"GetStringMap"},
	"map[string]string":       {"GetStringMapString"},
	"map[string][]string":     {"GetStringMapStringSlice"},
	"map[string]float64":      {"GetStringMap"},
	"map[string]int":          {"GetStringMap"},
}

// Tags are the schema annotations that may add secondary getters to a key.
type Tags struct {
	// Format is the schema's `format`, e.g. "duration".
	Format string
	// GolangType is the schema's `golang_type`, e.g. "duration".
	GolangType string
}

// selectGetters returns the default getter list for a schema leaf, from the `%T` of its
// default-layer value and its schema tags. The default value's type, not the schema type, decides
// the primary getter, because that is the type the Agent's getters convert from.
func selectGetters(defaultType string, tags Tags) ([]string, error) {
	primary, ok := primaryGetters[defaultType]
	if !ok && strings.HasPrefix(defaultType, "[]map[string]") {
		primary, ok = []string{"Get"}, true
	}
	if !ok {
		return nil, fmt.Errorf("%w: %s", ErrGetterDefaultType, defaultType)
	}
	getters := append([]string{}, primary...)
	switch {
	case tags.Format == "duration" && defaultType == "string",
		tags.GolangType == "duration" && (defaultType == "int" || defaultType == "float64"):
		getters = append(getters, "GetDuration")
	}
	return getters, nil
}

// selectGettersNoDefault returns the getter list for a schema leaf with no default (getter-map.md
// §1.1), whose declaredType is the schema's `type` and elementType is its `items` type (for
// `array`) or `additionalProperties` type (for `object`), or "" when the schema has none. A missing
// or nil default is valid, so this never errors: an unmatched declared type or element type falls
// back to `Get` alone, exactly as the table's "any other or no ..." rows do.
func selectGettersNoDefault(declaredType, elementType string, tags Tags) []string {
	var primary []string
	switch declaredType {
	case "boolean":
		primary = []string{"Get", "GetBool"}
	case "integer":
		primary = []string{"Get", "GetInt"}
	case "number":
		primary = []string{"Get", "GetFloat64"}
	case "string":
		primary = []string{"Get", "GetString"}
	case "array":
		switch elementType {
		case "string":
			primary = []string{"Get", "GetStringSlice"}
		case "number":
			primary = []string{"Get", "GetFloat64Slice"}
		default:
			primary = []string{"Get"}
		}
	case "object":
		switch elementType {
		case "string":
			primary = []string{"Get", "GetStringMapString"}
		case "array_of_string":
			primary = []string{"Get", "GetStringMapStringSlice"}
		default:
			primary = []string{"Get", "GetStringMap"}
		}
	default:
		primary = []string{"Get"}
	}
	getters := append([]string{}, primary...)
	switch {
	case tags.Format == "duration" && declaredType == "string",
		tags.GolangType == "duration" && (declaredType == "integer" || declaredType == "number"):
		getters = append(getters, "GetDuration")
	}
	return getters
}

// GettersForKey returns the getter list for one key entry. An override from the case wins;
// otherwise unknown keys and sections use [Get]. A leaf with a default (hasDefault) uses
// selectGetters from defaultType; a leaf with no default uses selectGettersNoDefault from its
// declared schema type and element type instead (getter-map.md §1.1).
//
// Both the probe and case runs select through SelectGetters, which calls this.
func GettersForKey(entry record.KeyEntry, sk *schema.Key, hasDefault bool, defaultType string) ([]string, error) {
	if entry.Getters != nil {
		return append([]string{}, entry.Getters...), nil
	}
	if sk.Kind != schema.Leaf {
		return []string{"Get"}, nil
	}
	tags := Tags{Format: sk.Format, GolangType: sk.GolangType()}
	if !hasDefault {
		return selectGettersNoDefault(sk.Type, sk.ElementType(), tags), nil
	}
	return selectGetters(defaultType, tags)
}

// DefaultLayerType returns the `%T` of a schema leaf's default-layer value, and whether it has
// one. The key has no default (getter-map.md §1.1) when GetAllSources' first element is missing,
// is not the default layer, or holds nil; that is valid, so the caller falls back
// to the key's declared schema type. Only call it on schema keys after the first snapshot:
// reading an unknown key adds it to the key set.
func DefaultLayerType(r model.Reader, key string) (typ string, hasDefault bool) {
	sources := r.GetAllSources(key)
	if len(sources) == 0 || sources[0].Source != model.SourceDefault || sources[0].Value == nil {
		return "", false
	}
	return fmt.Sprintf("%T", sources[0].Value), true
}

// CallGetter calls the named getter on the Agent's config reader. ReadConfigSection is not a
// method of the config: it is the Agent's OTLP pipeline's section read, which keeps only the
// section's configured leaves and nil-declared sections. It exists only under the `otlp` build
// tag, so the config recorder is built with that tag. IsConfigured is a config method, but is
// explicit-only (getter-map.md §2.1): it reports whether the user set a key, not the key's value.
func CallGetter(r model.Reader, name, key string) (interface{}, error) {
	switch name {
	case "Get":
		return r.Get(key), nil
	case "GetString":
		return r.GetString(key), nil
	case "GetBool":
		return r.GetBool(key), nil
	case "GetInt":
		return r.GetInt(key), nil
	case "GetInt32":
		return r.GetInt32(key), nil
	case "GetInt64":
		return r.GetInt64(key), nil
	case "GetFloat64":
		return r.GetFloat64(key), nil
	case "GetFloat64Slice":
		return r.GetFloat64Slice(key), nil
	case "GetDuration":
		return r.GetDuration(key), nil
	case "GetStringSlice":
		return r.GetStringSlice(key), nil
	case "GetStringMap":
		return r.GetStringMap(key), nil
	case "GetStringMapString":
		return r.GetStringMapString(key), nil
	case "GetStringMapStringSlice":
		return r.GetStringMapStringSlice(key), nil
	case "GetSizeInBytes":
		return r.GetSizeInBytes(key), nil
	case "ReadConfigSection":
		return readConfigSection(r, key)
	case "IsConfigured":
		return r.IsConfigured(key), nil
	default:
		return nil, fmt.Errorf("%w: %q", ErrGetterName, name)
	}
}

// SelectGetters returns the getter list for one key entry of a case: the override if given,
// otherwise the default list from the schema and, for a schema leaf, its default-layer type.
// GetAllSources is called only on schema leaves without an override; the caller discards its
// warnings. It also returns the default-layer type it used and whether the key has a default.
func SelectGetters(r model.Reader, entry record.KeyEntry, s schema.Schema) (list []string, defaultType string, hasDefault bool, err error) {
	sk := s.Key(entry.Key)
	if entry.Getters == nil && sk.Kind == schema.Leaf {
		defaultType, hasDefault = DefaultLayerType(r, entry.Key)
	}
	list, err = GettersForKey(entry, sk, hasDefault, defaultType)
	return list, defaultType, hasDefault, err
}

// readConfigSection reads a section the way the Agent's OTLP pipeline does, in the nested form
// of confmap's ToStringMap. A non-map result is an error from the recorder, not a getter result.
func readConfigSection(r model.Reader, key string) (interface{}, error) {
	var v interface{} = configcheck.ReadConfigSection(r, key).ToStringMap()
	m, ok := v.(map[string]interface{})
	if !ok || m == nil {
		return nil, fmt.Errorf("%w: ReadConfigSection of %q returned %T, not a map", ErrSectionRead, key, v)
	}
	return m, nil
}
