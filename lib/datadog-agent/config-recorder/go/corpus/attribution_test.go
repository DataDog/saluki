// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package corpus

import (
	"encoding/json"
	"errors"
	"reflect"
	"testing"
)

func updateEvent(seq uint64, key, source string) StreamEvent {
	return StreamEvent{Seq: seq, Key: key, Setting: Setting{Source: source, Value: json.RawMessage(`1`)}}
}

func TestUpdateRange(t *testing.T) {
	r := UpdateRange{Before: 2, After: 4}
	for seq, want := range map[uint64]bool{2: false, 3: true, 4: true, 5: false} {
		if got := r.Contains(seq); got != want {
			t.Errorf("Contains(%d) = %v", seq, got)
		}
	}
	if !r.WaitsFor() || r.SeqDelta() != 2 || r.EndsWait(3) || !r.EndsWait(4) || !r.EndsWait(5) {
		t.Errorf("wait rule wrong for %+v", r)
	}
	zero := UpdateRange{Before: 3, After: 3}
	if zero.WaitsFor() || zero.SeqDelta() != 0 || zero.Contains(3) {
		t.Errorf("a seq_delta 0 update must not wait or own a sequence ID")
	}
}

func TestAttributeRanges(t *testing.T) {
	// Update 0 issues seq 1, update 1 issues nothing, update 2 issues seq 2 and 3.
	ranges := []UpdateRange{{0, 1}, {1, 1}, {1, 3}}
	events := []StreamEvent{updateEvent(1, "a", "cli"), updateEvent(2, "b", "cli"), updateEvent(3, "a", "file")}
	got, err := Attribute([]string{"a", "b", "c"}, ranges, events)
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(got.UpdateEvents, []int{1, 0, 2}) {
		t.Errorf("update events %v", got.UpdateEvents)
	}
	a := got.KeyEvents[0]
	if len(a) != 2 || a[0].Update != 0 || a[1].Update != 2 || a[1].Source != "file" {
		t.Errorf("key a: %+v", a)
	}
	if len(got.KeyEvents[1]) != 1 || got.KeyEvents[1][0].Update != 2 || len(got.KeyEvents[2]) != 0 {
		t.Errorf("keys b, c: %+v", got.KeyEvents[1:])
	}
}

func TestAttributeOutOfRange(t *testing.T) {
	for _, seq := range []uint64{2, 5} {
		_, err := Attribute([]string{"a"}, []UpdateRange{{1, 1}, {2, 4}}, []StreamEvent{updateEvent(seq, "a", "cli")})
		if !errors.Is(err, ErrAttribution) {
			t.Errorf("seq %d: got %v, want ErrAttribution", seq, err)
		}
	}
	// With no updates at all, any event is in no update's range.
	if _, err := Attribute([]string{"a"}, nil, []StreamEvent{updateEvent(1, "a", "cli")}); !errors.Is(err, ErrAttribution) {
		t.Errorf("no ranges: got %v, want ErrAttribution", err)
	}
}

func TestAttributeOrphanKey(t *testing.T) {
	_, err := Attribute([]string{"a"}, []UpdateRange{{0, 1}}, []StreamEvent{updateEvent(1, "b", "cli")})
	if !errors.Is(err, ErrOrphanEvent) {
		t.Errorf("got %v, want ErrOrphanEvent", err)
	}
}

func TestCheckSequenceStart(t *testing.T) {
	if err := CheckSequenceStart(0); err != nil {
		t.Errorf("before 0: got %v", err)
	}
	if err := CheckSequenceStart(1); !errors.Is(err, ErrSequenceStart) {
		t.Errorf("before 1: got %v, want ErrSequenceStart", err)
	}
}

func TestCheckSequenceEnd(t *testing.T) {
	ranges := []UpdateRange{{0, 1}, {1, 1}, {1, 3}}
	if err := CheckSequenceEnd(3, ranges); err != nil {
		t.Errorf("matching final: got %v", err)
	}
	if err := CheckSequenceEnd(2, ranges); !errors.Is(err, ErrSequenceEnd) {
		t.Errorf("mismatched final: got %v, want ErrSequenceEnd", err)
	}
	if err := CheckSequenceEnd(0, nil); err != nil {
		t.Errorf("no updates, final 0: got %v", err)
	}
	if err := CheckSequenceEnd(1, nil); !errors.Is(err, ErrSequenceEnd) {
		t.Errorf("no updates, final 1: got %v, want ErrSequenceEnd", err)
	}
}
