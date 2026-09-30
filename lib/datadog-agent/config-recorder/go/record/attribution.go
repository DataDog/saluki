// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package record

import (
	"errors"
	"fmt"
)

// ErrAttribution marks a stream event whose sequence ID falls in no update's range. In format 1
// every event is an update (record.md §4.2, §5.2); a resync snapshot is rejected earlier, before
// attribution.
var ErrAttribution = errors.New("stream event is in no update's sequence range")

// ErrOrphanEvent marks an update event whose key is not one of the case's keys (record.md §4.2).
var ErrOrphanEvent = errors.New("update event's key is not one of the case's keys")

// ErrSequenceStart marks update 0's Before, relative to the first snapshot's sequence ID, not
// being zero (record.md §4.2: update 0's before must equal base).
var ErrSequenceStart = errors.New("update 0 must start at the first snapshot's sequence ID")

// ErrSequenceEnd marks the sequence ID read after the last update's wait (or, with no updates,
// after the snapshot reads), relative to the first snapshot's, not matching the last update's
// After, or zero when the case has no updates (record.md §4.2).
var ErrSequenceEnd = errors.New("final sequence ID must equal the last update's after")

// UpdateRange is the config's sequence ID before and after one update, relative to the first
// snapshot's sequence ID. The update owns the sequence IDs in (Before, After].
type UpdateRange struct {
	Before uint64
	After  uint64
}

// SeqDelta is the number of notifications the update issued.
func (r UpdateRange) SeqDelta() uint64 { return r.After - r.Before }

// Contains reports whether seq is in the update's range.
func (r UpdateRange) Contains(seq uint64) bool { return r.Before < seq && seq <= r.After }

// WaitsFor reports whether the recorder waits for an event after the update. It does not wait
// when the update issued no notification.
func (r UpdateRange) WaitsFor() bool { return r.After != r.Before }

// EndsWait reports whether an event with relative sequence seq ends the update's wait.
func (r UpdateRange) EndsWait(seq uint64) bool { return seq >= r.After }

// CheckSequenceStart checks that before, update 0's GetSequenceID read relative to the first
// snapshot's sequence ID, is zero (record.md §4.2).
func CheckSequenceStart(before uint64) error {
	if before != 0 {
		return fmt.Errorf("%w: got %d", ErrSequenceStart, before)
	}
	return nil
}

// CheckSequenceEnd checks that final -- the GetSequenceID read after the last update's wait ends,
// or after the snapshot reads in a case with no updates, relative to the first snapshot's
// sequence ID -- equals the last update's After, or zero when ranges is empty (record.md §4.2).
func CheckSequenceEnd(final uint64, ranges []UpdateRange) error {
	want := uint64(0)
	if n := len(ranges); n > 0 {
		want = ranges[n-1].After
	}
	if final != want {
		return fmt.Errorf("%w: got %d, want %d", ErrSequenceEnd, final, want)
	}
	return nil
}

// StreamEvent is one update event received after the first snapshot (record.md §4.2). A resync
// snapshot is rejected before it becomes a StreamEvent (§5.2).
type StreamEvent struct {
	// Seq is the event's sequence ID relative to the first snapshot's.
	Seq uint64
	// Key and Setting are the event's setting.
	Key     string
	Setting Setting
}

// Attribution is the stream events of a case sorted onto its key lines and updates.
type Attribution struct {
	// KeyEvents holds, per recorded key in `keys` order, the events carrying it in arrival order.
	KeyEvents [][]Event
}

// Attribute attributes each event, in arrival order, to the update whose range holds its
// sequence ID. An event whose sequence ID is in no update's range is ErrAttribution; an event
// whose key is not one of keys is ErrOrphanEvent (record.md §4.2). Both fail the case run.
func Attribute(keys []string, ranges []UpdateRange, events []StreamEvent) (*Attribution, error) {
	a := &Attribution{KeyEvents: make([][]Event, len(keys))}
	for n, ev := range events {
		update := -1
		for i, r := range ranges {
			if r.Contains(ev.Seq) {
				update = i
				break
			}
		}
		if update < 0 {
			return nil, fmt.Errorf("%w: event %d has seq %d; ranges %s", ErrAttribution, n, ev.Seq, describeRanges(ranges))
		}
		found := false
		for k, key := range keys {
			if key == ev.Key {
				found = true
				a.KeyEvents[k] = append(a.KeyEvents[k], Event{Setting: ev.Setting, Seq: ev.Seq - ranges[update].Before, Update: update})
			}
		}
		if !found {
			return nil, fmt.Errorf("%w: event %d, key %q", ErrOrphanEvent, n, ev.Key)
		}
	}
	return a, nil
}

func describeRanges(ranges []UpdateRange) string {
	parts := make([]string, len(ranges))
	for i, r := range ranges {
		parts[i] = fmt.Sprintf("%d:(%d,%d]", i, r.Before, r.After)
	}
	return fmt.Sprint(parts)
}
