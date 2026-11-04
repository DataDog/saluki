// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package schema

import (
	"strings"
	"testing"
)

func TestKeyMatchesMixedCasePathIgnoringCase(t *testing.T) {
	gui := &Key{Path: "GUI_host", Kind: Leaf}
	s := Schema{"GUI_host": gui, "api_key": {Path: "api_key", Kind: Leaf}}

	if got := s.Key("gui_host"); got != gui {
		t.Fatalf("Key(%q) = %+v, want the GUI_host leaf", "gui_host", got)
	}
	if got := s.Key("api_key"); got.Path != "api_key" || got.Kind != Leaf {
		t.Fatalf("Key(%q) = %+v, want the api_key leaf", "api_key", got)
	}
	if got := s.Key("not_in_schema"); got.Kind != Unknown || got.Path != "not_in_schema" {
		t.Fatalf("Key(%q) = %+v, want Kind Unknown", "not_in_schema", got)
	}
}

func TestParseRejectsPathsThatDifferOnlyInCase(t *testing.T) {
	data := []byte(`properties:
  GUI_host:
    node_type: setting
  gui_host:
    node_type: setting
`)
	_, err := Parse(data)
	if err == nil || !strings.Contains(err.Error(), "differ only in case") {
		t.Fatalf("Parse error = %v, want a differ-only-in-case error", err)
	}
}
