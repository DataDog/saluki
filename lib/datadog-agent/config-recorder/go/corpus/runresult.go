// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package corpus

import (
	"errors"
	"fmt"
)

// RunResult is what one case process hands the driver, as a gob file. It holds the parts of the
// case line and key lines the process knows, never finished corpus lines: the driver re-reads the
// case file for the inputs, and computes the header comparison and side effects itself.
type RunResult struct {
	// Exactly one of Origin and StartupError is set.
	Origin       *string
	StartupError *string
	// ConstructionWarnings are the warnings logged before the first snapshot, already filtered.
	ConstructionWarnings []Warning
	// Containerized and Features are the process's values; unset on startup failure.
	Containerized bool
	Features      []string
	// Keys holds one key line per case key, in the case's `keys` order; empty on startup failure.
	Keys []KeyLine
	// Snapshot is every setting of the first snapshot, by key; empty on startup failure.
	Snapshot map[string]Setting
}

// Errors for CheckCaseRecord.
var (
	ErrRecordKeyLines    = errors.New("key lines do not match the case's keys")
	ErrRecordFinalRead   = errors.New("reads.final must be present if and only if the case has updates")
	ErrRecordKeyLineCase = errors.New("key line names another case")
)

// CheckCaseRecord checks a case line against its key lines: there are none on startup error;
// otherwise they correspond one to one, in order, with the case's `keys`, and each has a final read
// if and only if the case has updates. It also validates every line on its own.
func CheckCaseRecord(c *CaseLine, keys []KeyLine) error {
	if err := c.Validate(); err != nil {
		return err
	}
	if c.StartupError != nil {
		if len(keys) != 0 {
			return fmt.Errorf("%w: %d key lines on startup error", ErrRecordKeyLines, len(keys))
		}
		return nil
	}
	if len(keys) != len(c.Inputs.Keys) {
		return fmt.Errorf("%w: %d key lines for %d keys", ErrRecordKeyLines, len(keys), len(c.Inputs.Keys))
	}
	hasUpdates := len(c.Inputs.Updates) > 0
	for i := range keys {
		k := &keys[i]
		if k.Case != c.Inputs.Name {
			return fmt.Errorf("%w: %q in case %q", ErrRecordKeyLineCase, k.Case, c.Inputs.Name)
		}
		if k.Key != c.Inputs.Keys[i].Key {
			return fmt.Errorf("%w: key line %d is %q, want %q", ErrRecordKeyLines, i, k.Key, c.Inputs.Keys[i].Key)
		}
		if (k.FinalRead != nil) != hasUpdates {
			return fmt.Errorf("%w: key %q", ErrRecordFinalRead, k.Key)
		}
		if err := k.Validate(); err != nil {
			return err
		}
	}
	return nil
}
