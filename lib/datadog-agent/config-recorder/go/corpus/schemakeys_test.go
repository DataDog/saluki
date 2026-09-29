// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package corpus

import (
	"strings"
	"testing"
)

func TestDiffSchemaKeysEqual(t *testing.T) {
	d := DiffSchemaKeys([]string{"b", "a", "a"}, []string{"a", "b"})
	if !d.Empty() {
		t.Errorf("equal key sets (in different order, with a duplicate): got diff %+v", d)
	}
}

func TestDiffSchemaKeysMismatch(t *testing.T) {
	d := DiffSchemaKeys([]string{"only_schema", "shared"}, []string{"shared", "only_agent"})
	if d.Empty() {
		t.Fatal("expected a non-empty diff")
	}
	if got, want := d.OnlySchema, []string{"only_schema"}; !equalStrings(got, want) {
		t.Errorf("OnlySchema: got %v, want %v", got, want)
	}
	if got, want := d.OnlyAgent, []string{"only_agent"}; !equalStrings(got, want) {
		t.Errorf("OnlyAgent: got %v, want %v", got, want)
	}
	msg := d.Error()
	for _, want := range []string{"1 only in the schema", "1 only in the Agent", "only_schema", "only_agent"} {
		if !strings.Contains(msg, want) {
			t.Errorf("Error() = %q, missing %q", msg, want)
		}
	}
}

func TestDiffSchemaKeysSamplesCapped(t *testing.T) {
	var schemaOnly []string
	for i := 0; i < 25; i++ {
		schemaOnly = append(schemaOnly, string(rune('a'+i)))
	}
	d := DiffSchemaKeys(schemaOnly, nil)
	if len(d.OnlySchema) != 25 {
		t.Fatalf("diff itself must keep every key: got %d", len(d.OnlySchema))
	}
	msg := d.Error()
	if strings.Count(msg, ",") > maxSchemaKeyDiffSample {
		t.Errorf("Error() shows more than %d keys: %q", maxSchemaKeyDiffSample, msg)
	}
	if !strings.Contains(msg, "and 5 more") {
		t.Errorf("Error() = %q, want the overflow count", msg)
	}
}

func equalStrings(a, b []string) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i] != b[i] {
			return false
		}
	}
	return true
}
