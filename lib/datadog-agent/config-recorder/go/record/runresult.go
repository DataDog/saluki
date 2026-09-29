// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package record

import (
	"encoding/gob"
	"errors"
	"fmt"
	"os"
)

// RunResult is what one case process hands the driver, as a gob file. It holds the parts of the
// case line and key lines the process knows, never finished corpus lines: the driver re-reads the
// case file for the inputs, and computes the header comparison and side effects itself.
//
// It is a sum type: exactly one of StartupError and Run is set.
type RunResult struct {
	// StartupError is the construction error's text when the config could not be built.
	StartupError *string
	// Run is what a case whose config was built recorded.
	Run *CaseRun
}

// CaseRun is the outcome of a case whose config was built.
type CaseRun struct {
	// Origin is the first snapshot's origin.
	Origin string
	// ConstructionWarnings are the warnings logged before the first snapshot, already filtered.
	ConstructionWarnings []Warning
	// Containerized and Features are the process's values.
	Containerized bool
	Features      []string
	// Keys holds one key line per case key, in the case's `keys` order.
	Keys []KeyLine
	// Snapshot is every setting of the first snapshot, by key.
	Snapshot map[string]Setting
	// Updates holds one result per case update, in order.
	Updates []UpdateResult
}

// ErrRunResult marks a result file that is not exactly one of a startup failure and a run.
var ErrRunResult = errors.New("run result must hold exactly one of a startup error and a run")

// WriteRunResult writes r to path as gob.
func WriteRunResult(path string, r *RunResult) error {
	if (r.StartupError == nil) == (r.Run == nil) {
		return ErrRunResult
	}
	f, err := os.Create(path)
	if err != nil {
		return err
	}
	if err := gob.NewEncoder(f).Encode(r); err != nil {
		f.Close()
		return err
	}
	return f.Close()
}

// ReadRunResult reads a result file WriteRunResult wrote.
func ReadRunResult(path string) (*RunResult, error) {
	f, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer f.Close()
	r := &RunResult{}
	if err := gob.NewDecoder(f).Decode(r); err != nil {
		return nil, fmt.Errorf("%s: %w", path, err)
	}
	if (r.StartupError == nil) == (r.Run == nil) {
		return nil, fmt.Errorf("%s: %w", path, ErrRunResult)
	}
	return r, nil
}

// Errors for CheckCaseRecord.
var (
	ErrRecordKeyLines    = errors.New("key lines do not match the case's keys")
	ErrRecordFinalRead   = errors.New("reads.final must be present if and only if the case has updates")
	ErrRecordKeyLineCase = errors.New("key line names another case")
	ErrRecordUpdates     = errors.New("case line must have one update result per case update")
	ErrRecordEventIndex  = errors.New("event names an update the case does not have")
)

// CheckCaseRecord checks a case line against its key lines: there are none on startup error;
// otherwise they correspond one to one, in order, with the case's `keys`, and each has a final read
// if and only if the case has updates. The case line has one update result per case update (none
// on startup error), and every event names one of them. It also validates every line on its own.
func CheckCaseRecord(c *CaseLine, keys []KeyLine) error {
	if err := c.Validate(); err != nil {
		return err
	}
	if c.StartupError != nil {
		if len(keys) != 0 {
			return fmt.Errorf("%w: %d key lines on startup error", ErrRecordKeyLines, len(keys))
		}
		if len(c.Updates) != 0 {
			return fmt.Errorf("%w: %d results on startup error", ErrRecordUpdates, len(c.Updates))
		}
		return nil
	}
	if len(keys) != len(c.Inputs.Keys) {
		return fmt.Errorf("%w: %d key lines for %d keys", ErrRecordKeyLines, len(keys), len(c.Inputs.Keys))
	}
	if len(c.Updates) != len(c.Inputs.Updates) {
		return fmt.Errorf("%w: %d results for %d updates", ErrRecordUpdates, len(c.Updates), len(c.Inputs.Updates))
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
		k.BindCase(c.Inputs.Updates)
		if err := k.Validate(); err != nil {
			return err
		}
	}
	return nil
}
