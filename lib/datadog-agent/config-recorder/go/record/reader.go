// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package record

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"sort"
)

// Errors for the corpus reader.
var (
	ErrReadLine         = errors.New("corpus line is not a valid record")
	ErrReadNonCanonical = errors.New("corpus line is not in canonical form")
	ErrReadOrder        = errors.New("corpus lines are out of order")
)

// CaseRecord is one case line and its key lines, in corpus order.
type CaseRecord struct {
	Line *CaseLine
	Keys []KeyLine
}

// Corpus is a whole corpus as read back, with every elided member reconstructed.
type Corpus struct {
	Header *HeaderLine
	Cases  []CaseRecord
}

type rawHeader struct {
	Type           string   `json:"type"`
	Format         int      `json:"format"`
	AgentCommit    string   `json:"agent_commit"`
	GOOS           string   `json:"goos"`
	GOARCH         string   `json:"goarch"`
	GoVersion      string   `json:"go_version"`
	ContainerImage string   `json:"container_image"`
	Containerized  bool     `json:"containerized"`
	Features       []string `json:"features"`
	InputsDigest   string   `json:"inputs_digest"`
}

type rawWarning struct {
	Level   string `json:"level"`
	Message string `json:"message"`
}

type rawSetting struct {
	Key         string          `json:"key"`
	Source      string          `json:"source"`
	UnsetSource string          `json:"unset_source"`
	Value       json.RawMessage `json:"value"`
	Absent      bool            `json:"absent"`
	Seq         uint64          `json:"seq"`
	Update      *int            `json:"update"`
}

type rawInputs struct {
	Env         map[string]string `json:"env"`
	YAML        *string           `json:"yaml"`
	FleetPolicy *string           `json:"fleet_policy"`
	CLI         []struct {
		Key   string          `json:"key"`
		Value json.RawMessage `json:"value"`
	} `json:"cli"`
	Updates []struct {
		Op     string          `json:"op"`
		Key    string          `json:"key"`
		Value  json.RawMessage `json:"value"`
		Source string          `json:"source"`
	} `json:"updates"`
	Keys *[]struct {
		Key     string   `json:"key"`
		Getters []string `json:"getters"`
	} `json:"keys"`
}

type rawCaseLine struct {
	Type                 string       `json:"type"`
	Case                 string       `json:"case"`
	Group                string       `json:"group"`
	Why                  []string     `json:"why"`
	Inputs               rawInputs    `json:"inputs"`
	Containerized        *bool        `json:"containerized"`
	Features             *[]string    `json:"features"`
	Origin               *string      `json:"origin"`
	StartupError         *string      `json:"startup_error"`
	SideEffects          []rawSetting `json:"side_effects"`
	ConstructionWarnings []rawWarning `json:"construction_warnings"`
	Updates              []struct {
		SeqDelta uint64       `json:"seq_delta"`
		TimedOut bool         `json:"timed_out"`
		Warnings []rawWarning `json:"warnings"`
	} `json:"updates"`
}

type rawRead struct {
	Getters []struct {
		Getter   string          `json:"getter"`
		Result   json.RawMessage `json:"result"`
		Warnings []rawWarning    `json:"warnings"`
	} `json:"getters"`
	GoType string  `json:"go_type"`
	Source *string `json:"source"`
}

type rawKeyLine struct {
	Type     string          `json:"type"`
	Case     string          `json:"case"`
	Key      string          `json:"key"`
	Snapshot json.RawMessage `json:"snapshot"`
	Events   []rawSetting    `json:"events"`
	Reads    struct {
		Snapshot *rawRead `json:"snapshot"`
		Final    *rawRead `json:"final"`
	} `json:"reads"`
}

func decodeStrict(line []byte, v interface{}) error {
	dec := json.NewDecoder(bytes.NewReader(line))
	dec.DisallowUnknownFields()
	if err := dec.Decode(v); err != nil {
		return fmt.Errorf("%w: %v", ErrReadLine, err)
	}
	if dec.More() {
		return fmt.Errorf("%w: trailing data", ErrReadLine)
	}
	return nil
}

func warnings(raw []rawWarning) []Warning {
	var out []Warning
	for _, w := range raw {
		out = append(out, Warning{Level: w.Level, Message: w.Message})
	}
	return out
}

// ReadCorpus parses a corpus, reconstructs every member the writer elides (record.md §3.1, §5.2,
// §5.3), and checks that each line is exactly what the line writer writes for what was read: a
// non-canonical form, such as an explicit `"op":"set"` or `inputs.keys` written where it must be
// elided, is ErrReadNonCanonical.
func ReadCorpus(data []byte) (*Corpus, error) {
	if len(data) == 0 || data[len(data)-1] != '\n' {
		return nil, fmt.Errorf("%w: corpus must end in a newline", ErrReadLine)
	}
	lines := bytes.Split(data[:len(data)-1], []byte("\n"))
	var probe struct {
		Type string `json:"type"`
	}
	if err := json.Unmarshal(lines[0], &probe); err != nil || probe.Type != "header" {
		return nil, fmt.Errorf("%w: the first line must be the header", ErrReadOrder)
	}
	header, err := readHeader(lines[0])
	if err != nil {
		return nil, fmt.Errorf("line 1: %w", err)
	}
	c := &Corpus{Header: header}
	var pending [][]byte
	flush := func() error {
		if len(c.Cases) == 0 {
			return nil
		}
		rec := &c.Cases[len(c.Cases)-1]
		if err := finishCase(rec, pending); err != nil {
			return fmt.Errorf("case %q: %w", rec.Line.Inputs.Name, err)
		}
		pending = nil
		return nil
	}
	for i, line := range lines[1:] {
		if err := json.Unmarshal(line, &probe); err != nil {
			return nil, fmt.Errorf("line %d: %w: %v", i+2, ErrReadLine, err)
		}
		switch probe.Type {
		case "case":
			if err := flush(); err != nil {
				return nil, err
			}
			cl, err := readCase(line)
			if err != nil {
				return nil, fmt.Errorf("line %d: %w", i+2, err)
			}
			if n := len(c.Cases); n > 0 && c.Cases[n-1].Line.Inputs.Name >= cl.Inputs.Name {
				return nil, fmt.Errorf("line %d: %w: case %q", i+2, ErrReadOrder, cl.Inputs.Name)
			}
			c.Cases = append(c.Cases, CaseRecord{Line: cl})
			pending = append(pending, line)
		case "key":
			if len(c.Cases) == 0 {
				return nil, fmt.Errorf("line %d: %w: key line before any case line", i+2, ErrReadOrder)
			}
			pending = append(pending, line)
		default:
			return nil, fmt.Errorf("line %d: %w: type %q", i+2, ErrReadLine, probe.Type)
		}
	}
	if err := flush(); err != nil {
		return nil, err
	}
	return c, nil
}

func canonical(l Line, line []byte) error {
	b, err := MarshalLine(l)
	if err != nil {
		return err
	}
	if !bytes.Equal(b, line) {
		return fmt.Errorf("%w:\n got  %s\n want %s", ErrReadNonCanonical, line, b)
	}
	return nil
}

func readHeader(line []byte) (*HeaderLine, error) {
	var r rawHeader
	if err := decodeStrict(line, &r); err != nil {
		return nil, err
	}
	if r.Format != RecordFormat {
		return nil, fmt.Errorf("%w: %d", ErrRecordFormat, r.Format)
	}
	h := &HeaderLine{AgentCommit: r.AgentCommit, GOOS: r.GOOS, GOARCH: r.GOARCH, GoVersion: r.GoVersion,
		ContainerImage: r.ContainerImage, Containerized: r.Containerized, Features: r.Features,
		InputsDigest: r.InputsDigest}
	return h, canonical(h, line)
}

// readCase parses a case line. Its inputs.keys, when elided, is filled in by finishCase.
func readCase(line []byte) (*CaseLine, error) {
	var r rawCaseLine
	if err := decodeStrict(line, &r); err != nil {
		return nil, err
	}
	c := &Case{Name: r.Case, Group: r.Group, Why: r.Why, Env: r.Inputs.Env, YAML: r.Inputs.YAML,
		FleetPolicy: r.Inputs.FleetPolicy}
	if len(c.Why) == 0 {
		c.Why = nil
	}
	for _, o := range r.Inputs.CLI {
		v, err := decodeTypedJSON(o.Value)
		if err != nil {
			return nil, fmt.Errorf("cli %q: %w", o.Key, err)
		}
		c.CLI = append(c.CLI, CLIOverride{Key: o.Key, Value: v})
	}
	for _, u := range r.Inputs.Updates {
		up := Update{Op: "set", Key: u.Key, Source: u.Source}
		if u.Op != "" {
			up.Op = u.Op
		}
		if up.Op == "set" {
			v, err := decodeTypedJSON(u.Value)
			if err != nil {
				return nil, fmt.Errorf("update %q: %w", u.Key, err)
			}
			up.Value = v
		}
		c.Updates = append(c.Updates, up)
	}
	if r.Inputs.Keys != nil {
		c.Keys = []KeyEntry{}
		for _, k := range *r.Inputs.Keys {
			c.Keys = append(c.Keys, KeyEntry{Key: k.Key, Getters: k.Getters})
		}
	}
	cl := &CaseLine{Inputs: c, Containerized: r.Containerized, Features: r.Features, Origin: r.Origin,
		StartupError: r.StartupError, ConstructionWarnings: warnings(r.ConstructionWarnings)}
	for _, se := range r.SideEffects {
		if se.Seq != 0 || se.Update != nil {
			return nil, fmt.Errorf("%w: side effect %q has event members", ErrReadLine, se.Key)
		}
		cl.SideEffects = append(cl.SideEffects, SideEffect{Key: se.Key, Absent: se.Absent,
			Setting: Setting{Source: se.Source, UnsetSource: se.UnsetSource, Value: se.Value}})
	}
	for _, u := range r.Updates {
		cl.Updates = append(cl.Updates, UpdateResult{SeqDelta: u.SeqDelta, TimedOut: u.TimedOut,
			Warnings: warnings(u.Warnings)})
	}
	return cl, nil
}

// finishCase reads a case's key lines, reconstructs the case's elided inputs.keys, and checks the
// case line and each key line against what the writer writes for them.
func finishCase(rec *CaseRecord, lines [][]byte) error {
	cl := rec.Line
	for i, line := range lines[1:] {
		k, err := readKey(line, cl.Inputs.Updates)
		if err != nil {
			return fmt.Errorf("key line %d: %w", i+1, err)
		}
		if k.Case != cl.Inputs.Name {
			return fmt.Errorf("%w: key line of case %q", ErrReadOrder, k.Case)
		}
		if n := len(rec.Keys); n > 0 && rec.Keys[n-1].Key >= k.Key {
			return fmt.Errorf("%w: key %q", ErrReadOrder, k.Key)
		}
		rec.Keys = append(rec.Keys, *k)
		if err := canonical(k, line); err != nil {
			return err
		}
	}
	if cl.Inputs.Keys == nil {
		if cl.StartupError != nil {
			return fmt.Errorf("%w: inputs.keys is missing on a startup failure", ErrReadLine)
		}
		for _, k := range rec.Keys {
			cl.Inputs.Keys = append(cl.Inputs.Keys, KeyEntry{Key: k.Key})
		}
	}
	if len(cl.Inputs.Keys) == 0 {
		return fmt.Errorf("%w: a case has at least one key", ErrReadLine)
	}
	if err := canonical(cl, lines[0]); err != nil {
		return err
	}
	// CheckCaseRecord wants key lines in the case's `keys` order.
	order := map[string]int{}
	for i, k := range cl.Inputs.Keys {
		order[k.Key] = i
	}
	keys := append([]KeyLine(nil), rec.Keys...)
	sort.SliceStable(keys, func(i, j int) bool { return order[keys[i].Key] < order[keys[j].Key] })
	return CheckCaseRecord(cl, keys)
}

func readRead(r *rawRead, streamed *string) (Read, error) {
	if r == nil {
		return Read{}, fmt.Errorf("%w: missing read", ErrReadLine)
	}
	out := Read{GoType: r.GoType}
	for _, g := range r.Getters {
		out.Getters = append(out.Getters, GetterResult{Getter: g.Getter, Result: g.Result, Warnings: warnings(g.Warnings)})
	}
	switch {
	case r.Source != nil:
		out.Source = *r.Source
	case streamed != nil:
		out.Source = *streamed
	default:
		return out, fmt.Errorf("%w: read source is required when the snapshot setting is null", ErrReadLine)
	}
	return out, nil
}

// readKey parses a key line and reconstructs its events' `update` and its reads' `source`.
func readKey(line []byte, updates []Update) (*KeyLine, error) {
	var r rawKeyLine
	if err := decodeStrict(line, &r); err != nil {
		return nil, err
	}
	k := &KeyLine{Case: r.Case, Key: r.Key}
	k.BindCase(updates)
	if r.Snapshot == nil {
		return nil, fmt.Errorf("%w: snapshot is required", ErrReadLine)
	}
	if !bytes.Equal(r.Snapshot, []byte("null")) {
		var s rawSetting
		if err := decodeStrict(r.Snapshot, &s); err != nil {
			return nil, err
		}
		if s.Key != "" || s.Absent || s.Seq != 0 || s.Update != nil {
			return nil, fmt.Errorf("%w: snapshot setting has extra members", ErrReadLine)
		}
		k.Snapshot = &Setting{Source: s.Source, UnsetSource: s.UnsetSource, Value: s.Value}
	}
	implied := impliedUpdate(k.Key, updates)
	for j, e := range r.Events {
		if e.Key != "" || e.Absent {
			return nil, fmt.Errorf("%w: event %d has extra members", ErrReadLine, j)
		}
		ev := Event{Setting: Setting{Source: e.Source, UnsetSource: e.UnsetSource, Value: e.Value}, Seq: e.Seq}
		switch {
		case e.Update != nil:
			ev.Update = *e.Update
		case implied >= 0:
			ev.Update = implied
		default:
			return nil, fmt.Errorf("%w: event %d has no update and none is implied", ErrReadLine, j)
		}
		k.Events = append(k.Events, ev)
	}
	var snapshotSource *string
	if k.Snapshot != nil {
		snapshotSource = &k.Snapshot.Source
	}
	var err error
	if k.SnapshotRead, err = readRead(r.Reads.Snapshot, snapshotSource); err != nil {
		return nil, err
	}
	if r.Reads.Final != nil {
		finalSource := snapshotSource
		if n := len(k.Events); n > 0 {
			finalSource = &k.Events[n-1].Source
		}
		fr, err := readRead(r.Reads.Final, finalSource)
		if err != nil {
			return nil, err
		}
		k.FinalRead = &fr
	}
	return k, nil
}
