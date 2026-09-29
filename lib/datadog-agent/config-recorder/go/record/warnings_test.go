// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package record

import (
	"errors"
	"log/slog"
	"reflect"
	"testing"
	"time"
)

func TestWarningFromRecord(t *testing.T) {
	tests := []struct {
		level    slog.Level
		want     bool
		levelStr string
	}{
		{slog.LevelDebug, false, ""},
		{slog.LevelInfo, false, ""},
		{slog.LevelWarn, true, "WARN"},
		{slog.LevelWarn + 1, true, "WARN"},
		{slog.LevelError, true, "ERROR"},
		{slog.LevelError + 4, true, "ERROR"},
	}
	for _, tc := range tests {
		r := slog.NewRecord(time.Now(), tc.level, "boom", 0)
		w, ok := WarningFromRecord(r)
		if ok != tc.want {
			t.Errorf("level %v: ok = %v, want %v", tc.level, ok, tc.want)
			continue
		}
		if !ok {
			continue
		}
		if w.Level != tc.levelStr || w.Message != "boom" {
			t.Errorf("level %v: got %+v", tc.level, w)
		}
		if err := w.Validate(); err != nil {
			t.Errorf("level %v: Validate: %v", tc.level, err)
		}
	}
}

func TestSortConstructionWarnings(t *testing.T) {
	in := []Warning{
		{Level: "WARN", Message: "b"},
		{Level: "ERROR", Message: "a"},
		{Level: "WARN", Message: "a"},
		{Level: "WARN", Message: "b"},
	}
	got := SortConstructionWarnings(in)
	want := []Warning{
		{Level: "ERROR", Message: "a"},
		{Level: "WARN", Message: "a"},
		{Level: "WARN", Message: "b"},
		{Level: "WARN", Message: "b"},
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("got %+v, want %+v", got, want)
	}
	// The input is untouched and its own order (emission order) is preserved for callers that
	// must not sort it (getter-call and update warnings).
	if in[0].Message != "b" || in[1].Message != "a" {
		t.Fatalf("input mutated: %+v", in)
	}
}

func TestWarningValidate(t *testing.T) {
	if err := (Warning{Level: "WARN", Message: "x"}).Validate(); err != nil {
		t.Errorf("WARN: %v", err)
	}
	if err := (Warning{Level: "ERROR", Message: "x"}).Validate(); err != nil {
		t.Errorf("ERROR: %v", err)
	}
	for _, level := range []string{"INFO", "DEBUG", "", "warn"} {
		if err := (Warning{Level: level, Message: "x"}).Validate(); !errors.Is(err, ErrWarningLevel) {
			t.Errorf("level %q: got %v, want ErrWarningLevel", level, err)
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
