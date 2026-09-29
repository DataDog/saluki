// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package corpus

import (
	"errors"
	"testing"
)

const checkCase = `name: two-keys
group: behavior
why: [x]
keys: [a, b]
`

const checkCaseUpdates = `name: with-updates
group: behavior
why: [x]
updates:
  - {key: a, value: 1, source: remote-config}
keys: [a]
`

func keyLine(c, k string, final bool) KeyLine {
	kl := KeyLine{Case: c, Key: k, SnapshotRead: Read{GoType: "<nil>", Source: "default"}}
	if final {
		kl.FinalRead = &Read{GoType: "<nil>", Source: "default"}
	}
	return kl
}

func TestCheckCaseRecord(t *testing.T) {
	c := mustParse(t, checkCase)
	u := mustParse(t, checkCaseUpdates)
	ok := &CaseLine{Inputs: c, Origin: strp("datadog.yaml")}
	failed := &CaseLine{Inputs: c, StartupError: strp("boom")}
	oneUpdate := []UpdateResult{{Index: 0, SeqDelta: 1, Events: 1}}
	updated := &CaseLine{Inputs: u, Origin: strp("datadog.yaml"), Updates: oneUpdate}
	noResults := &CaseLine{Inputs: u, Origin: strp("datadog.yaml")}
	timedOutZero := &CaseLine{Inputs: u, Origin: strp("datadog.yaml"), Updates: []UpdateResult{{Index: 0, TimedOut: true}}}
	failedUpdates := &CaseLine{Inputs: u, StartupError: strp("boom"), Updates: oneUpdate}
	withEvents := func(k string, events ...Event) []KeyLine {
		kl := keyLine("with-updates", k, true)
		kl.Events = events
		return []KeyLine{kl}
	}
	tests := []struct {
		name string
		line *CaseLine
		keys []KeyLine
		want error
	}{
		{"in order", ok, []KeyLine{keyLine("two-keys", "a", false), keyLine("two-keys", "b", false)}, nil},
		{"out of order", ok, []KeyLine{keyLine("two-keys", "b", false), keyLine("two-keys", "a", false)}, ErrRecordKeyLines},
		{"missing", ok, []KeyLine{keyLine("two-keys", "a", false)}, ErrRecordKeyLines},
		{"other case", ok, []KeyLine{keyLine("other", "a", false), keyLine("two-keys", "b", false)}, ErrRecordKeyLineCase},
		{"final without updates", ok, []KeyLine{keyLine("two-keys", "a", true), keyLine("two-keys", "b", false)}, ErrRecordFinalRead},
		{"startup error, no keys", failed, nil, nil},
		{"startup error with keys", failed, []KeyLine{keyLine("two-keys", "a", false)}, ErrRecordKeyLines},
		{"updates with final", updated, []KeyLine{keyLine("with-updates", "a", true)}, nil},
		{"updates without final", updated, []KeyLine{keyLine("with-updates", "a", false)}, ErrRecordFinalRead},
		{"invalid case line", &CaseLine{Inputs: c}, nil, ErrRecordOrigin},
		{"missing update results", noResults, []KeyLine{keyLine("with-updates", "a", true)}, ErrRecordUpdates},
		{"update results on startup error", failedUpdates, nil, ErrRecordUpdates},
		{"timed out with seq_delta 0", timedOutZero, []KeyLine{keyLine("with-updates", "a", true)}, ErrRecordTimedOut},
		{"event names no update", updated, withEvents("a", Event{Seq: 1, Update: 1}), ErrRecordEventIndex},
		{"event names update -1", updated, withEvents("a", Event{Seq: 1, Update: -1}), ErrRecordEventIndex},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			err := CheckCaseRecord(tt.line, tt.keys)
			if tt.want == nil && err != nil || tt.want != nil && !errors.Is(err, tt.want) {
				t.Fatalf("got %v, want %v", err, tt.want)
			}
		})
	}
}

func TestRunResultRoundTrip(t *testing.T) {
	path := t.TempDir() + "/r.gob"
	kl := keyLine("c", "a", true)
	kl.Events = []Event{{Seq: 1, Update: 0}, {Seq: 2, Update: 1}}
	in := &RunResult{Run: &CaseRun{Origin: "datadog.yaml", Keys: []KeyLine{kl}, Updates: []UpdateResult{{Index: 0, SeqDelta: 1, Events: 1}}}}
	if err := WriteRunResult(path, in); err != nil {
		t.Fatal(err)
	}
	out, err := ReadRunResult(path)
	if err != nil {
		t.Fatal(err)
	}
	ev := out.Run.Keys[0].Events
	if ev[0].Update != 0 || ev[1].Update != 1 {
		t.Fatalf("events did not round-trip: %+v", ev)
	}
	if err := WriteRunResult(path, &RunResult{}); !errors.Is(err, ErrRunResult) {
		t.Fatalf("empty result: got %v", err)
	}
}
