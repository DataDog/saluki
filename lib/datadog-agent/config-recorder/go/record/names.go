// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package record

import (
	"bytes"
	"strings"
)

// Group is a case's coverage group (case.md §3).
type Group string

// The coverage groups.
const (
	GroupBaseline    Group = "baseline"
	GroupBreadth     Group = "breadth"
	GroupDepth       Group = "depth"
	GroupUnsupported Group = "unsupported"
	GroupExcluded    Group = "excluded"
	GroupUnknown     Group = "unknown"
	GroupBehavior    Group = "behavior"
)

// Bisectable says whether the driver may split a case of this group into parts (case.md §3.1):
// every group but `baseline` and `behavior`.
func (g Group) Bisectable() bool {
	return g != GroupBaseline && g != GroupBehavior
}

// Sanitize maps s to the case-name alphabet: lowercase, and every character outside [a-z0-9]
// becomes `-`.
func Sanitize(s string) string {
	var b strings.Builder
	for _, r := range strings.ToLower(s) {
		if (r >= 'a' && r <= 'z') || (r >= '0' && r <= '9') {
			b.WriteRune(r)
		} else {
			b.WriteByte('-')
		}
	}
	return b.String()
}

// PartName is the name of the single-key part of the case root that holds key: `<root>--<k>`,
// with k the key sanitized (case.md §3.1).
func PartName(root, key string) string {
	return root + "--" + Sanitize(key)
}

// MarshalSideEffect returns the canonical JSON of one side_effects element (record.md §3.2): the
// streamed setting plus `key`, or `{"absent":true,"key":k}` for a setting absent from the case.
func MarshalSideEffect(se SideEffect) ([]byte, error) {
	var buf bytes.Buffer
	if err := writeCanonical(&buf, sideEffectObject(se)); err != nil {
		return nil, err
	}
	return buf.Bytes(), nil
}

func sideEffectObject(se SideEffect) jsonObject {
	if se.Absent {
		return jsonObject{"absent": true, "key": se.Key}
	}
	o := settingObject(&se.Setting)
	o["key"] = se.Key
	return o
}
