// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package record

// GetterNames are the getters a list may name, in the record format's order. GetSource is
// recorded separately and is not one of them. ReadConfigSection and IsConfigured (getter-map.md
// §2.1) are explicit-only reads and are only ever named by a case's explicit getters list: default
// selection never picks them.
var GetterNames = []string{
	"Get", "GetString", "GetBool", "GetInt", "GetInt32", "GetInt64", "GetFloat64",
	"GetFloat64Slice", "GetDuration", "GetStringSlice", "GetStringMap", "GetStringMapString",
	"GetStringMapStringSlice", "GetSizeInBytes", "ReadConfigSection", "IsConfigured",
}

// IsGetterName reports whether name is one of GetterNames.
func IsGetterName(name string) bool {
	for _, n := range GetterNames {
		if n == name {
			return true
		}
	}
	return false
}
