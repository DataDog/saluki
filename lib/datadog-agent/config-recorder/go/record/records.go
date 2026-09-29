// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package record

import (
	"encoding/json"
	"errors"
	"fmt"
	"regexp"
	"sort"
)

// RecordFormat is the record format version the config recorder writes.
const RecordFormat = 1

// Errors for the record consistency rules.
var (
	ErrRecordFormat         = errors.New("header format is not the version this writer knows")
	ErrRecordCommit         = errors.New("agent_commit must be 40 lowercase hex digits")
	ErrRecordFeaturesOrder  = errors.New("features must be sorted")
	ErrRecordOrigin         = errors.New("case line must have exactly one of origin and startup_error")
	ErrRecordStartupFailure = errors.New("case line with startup_error must omit features, containerized and side_effects")
	ErrRecordTimedOut       = errors.New("update timed_out requires seq_delta > 0")
	ErrRecordSideEffects    = errors.New("side_effects must be sorted by unique key")
	ErrRecordAbsent         = errors.New("absent setting must have empty sources and no value")
	ErrRecordField          = errors.New("required member is empty")
)

// CommitPattern matches a full lowercase hex git commit ID.
var CommitPattern = regexp.MustCompile(`^[0-9a-f]{40}$`)

// InputsDigestPattern matches the header's inputs_digest: `sha256:` and 64 lowercase hex
// (record.md §2).
var InputsDigestPattern = regexp.MustCompile(`^sha256:[0-9a-f]{64}$`)

// ErrRecordInputsDigest marks a header inputs_digest that is not `sha256:` and 64 lowercase hex.
var ErrRecordInputsDigest = errors.New("inputs_digest must be sha256: and 64 lowercase hex digits")

// ErrWarningLevel marks a warning whose level record.md §7 does not allow.
var ErrWarningLevel = errors.New("warning level must be WARN or ERROR")

// Warning is one log record at WARN or above.
type Warning struct {
	Level   string
	Message string
}

// Validate checks that the warning's level is one record.md §7 allows.
func (w Warning) Validate() error {
	if w.Level != "WARN" && w.Level != "ERROR" {
		return fmt.Errorf("%w: %q", ErrWarningLevel, w.Level)
	}
	return nil
}

// Setting is one streamed pb.ConfigSetting without its key. Value holds compacted protojson bytes,
// or nil when the proto value field is unset.
type Setting struct {
	Source      string
	UnsetSource string
	Value       json.RawMessage
}

// SideEffect is a setting that differs from the baseline process's first snapshot.
type SideEffect struct {
	Key string
	Setting
	Absent bool
}

// Event is one later stream event carrying a key. Every event belongs to an update (record.md
// §4.2, §5.2); a resync snapshot is a harness failure in format 1, so no event kind or absence
// remains here (side_effects still uses Absent, on SideEffect).
type Event struct {
	Setting
	// Seq is the event's sequence ID minus the `before` sequence ID of its update, so an update's
	// first event has Seq 1 (record.md §5.2).
	Seq uint64
	// Update is the index of the update the event belongs to.
	Update int
}

// UpdateResult is what one case update did.
type UpdateResult struct {
	SeqDelta uint64
	TimedOut bool
	Warnings []Warning
}

// GetterResult is one getter call at a checkpoint. Result is the EncodeResult output.
type GetterResult struct {
	Getter   string
	Result   json.RawMessage
	Warnings []Warning
}

// Read is the getter reads at one checkpoint. Source is always the GetSource result; the writer
// leaves it out when it equals the streamed source a reader compares it with.
type Read struct {
	Getters []GetterResult
	GoType  string
	Source  string
}

// HeaderLine is the first corpus line.
type HeaderLine struct {
	AgentCommit    string
	GOOS           string
	GOARCH         string
	GoVersion      string
	ContainerImage string
	Containerized  bool
	Features       []string
	InputsDigest   string
}

// CaseLine is the one line per case. Its case, group and why come from Inputs, its parsed case:
// there is exactly one identity for a case, and the line writer never disagrees with it.
type CaseLine struct {
	Inputs *Case
	// Containerized and Features are nil unless they differ from the header.
	Containerized *bool
	Features      *[]string
	// Exactly one of Origin and StartupError is set.
	Origin               *string
	StartupError         *string
	SideEffects          []SideEffect
	ConstructionWarnings []Warning
	Updates              []UpdateResult
}

// KeyLine is the one line per recorded key.
type KeyLine struct {
	Case string
	Key  string
	// Snapshot is nil when the key is absent from the first snapshot.
	Snapshot *Setting
	Events   []Event
	// FinalRead is set only when the case has updates.
	SnapshotRead Read
	FinalRead    *Read

	// caseUpdates are the case's updates, which decide whether events write `update`. The line
	// writer needs them only for a key line with events; see BindCase.
	caseUpdates []Update
	bound       bool
}

// ErrRecordUnboundEvents marks a key line with events whose case updates were never given.
var ErrRecordUnboundEvents = errors.New("key line with events needs its case's updates (BindCase)")

// BindCase gives the key line its case's updates. The line writer omits each event's `update`
// when exactly one of them names the line's key and every event belongs to it (record.md §5.2).
func (k *KeyLine) BindCase(updates []Update) {
	k.caseUpdates = updates
	k.bound = true
}

// impliedUpdate is the update a reader attributes the key's events to when they carry no
// `update`: the only case update whose key is key, or -1 when there is not exactly one.
func impliedUpdate(key string, updates []Update) int {
	found := -1
	for i, u := range updates {
		if u.Key == key {
			if found >= 0 {
				return -1
			}
			found = i
		}
	}
	return found
}

// omitsUpdate reports whether the line's events leave out `update`.
func (k *KeyLine) omitsUpdate() bool {
	implied := impliedUpdate(k.Key, k.caseUpdates)
	if implied < 0 {
		return false
	}
	for _, e := range k.Events {
		if e.Update != implied {
			return false
		}
	}
	return true
}

// Validate checks the header line.
func (h *HeaderLine) Validate() error {
	if !CommitPattern.MatchString(h.AgentCommit) {
		return fmt.Errorf("%w: %q", ErrRecordCommit, h.AgentCommit)
	}
	for name, v := range map[string]string{"goos": h.GOOS, "goarch": h.GOARCH, "go_version": h.GoVersion, "container_image": h.ContainerImage} {
		if v == "" {
			return fmt.Errorf("%w: %s", ErrRecordField, name)
		}
	}
	if !sort.StringsAreSorted(h.Features) {
		return ErrRecordFeaturesOrder
	}
	if !InputsDigestPattern.MatchString(h.InputsDigest) {
		return fmt.Errorf("%w: %q", ErrRecordInputsDigest, h.InputsDigest)
	}
	return nil
}

func (h *HeaderLine) object() (jsonObject, error) {
	features := h.Features
	if features == nil {
		features = []string{}
	}
	return jsonObject{
		"type":            "header",
		"format":          RecordFormat,
		"agent_commit":    h.AgentCommit,
		"goos":            h.GOOS,
		"goarch":          h.GOARCH,
		"go_version":      h.GoVersion,
		"container_image": h.ContainerImage,
		"containerized":   h.Containerized,
		"features":        features,
		"inputs_digest":   h.InputsDigest,
	}, nil
}

// Validate checks the case line's consistency rules.
func (c *CaseLine) Validate() error {
	if c.Inputs == nil || c.Inputs.Name == "" || c.Inputs.Group == "" {
		return fmt.Errorf("%w: case, group or inputs", ErrRecordField)
	}
	if (c.Origin == nil) == (c.StartupError == nil) {
		return ErrRecordOrigin
	}
	if c.StartupError != nil && (c.Features != nil || c.Containerized != nil || len(c.SideEffects) > 0) {
		return ErrRecordStartupFailure
	}
	if c.Features != nil && !sort.StringsAreSorted(*c.Features) {
		return ErrRecordFeaturesOrder
	}
	for i, se := range c.SideEffects {
		if i > 0 && c.SideEffects[i-1].Key >= se.Key {
			return fmt.Errorf("%w: %q", ErrRecordSideEffects, se.Key)
		}
		if se.Absent && (se.Source != "" || se.UnsetSource != "" || se.Value != nil) {
			return fmt.Errorf("%w: side effect %q", ErrRecordAbsent, se.Key)
		}
	}
	for _, w := range c.ConstructionWarnings {
		if err := w.Validate(); err != nil {
			return err
		}
	}
	for i, u := range c.Updates {
		if u.TimedOut && u.SeqDelta == 0 {
			return fmt.Errorf("%w: update %d", ErrRecordTimedOut, i)
		}
		for _, w := range u.Warnings {
			if err := w.Validate(); err != nil {
				return err
			}
		}
	}
	return nil
}

func (c *CaseLine) object() (jsonObject, error) {
	why := c.Inputs.Why
	if why == nil {
		why = []string{}
	}
	inputs, err := inputsObject(c.Inputs, c.StartupError != nil)
	if err != nil {
		return nil, err
	}
	obj := jsonObject{
		"type":   "case",
		"case":   c.Inputs.Name,
		"group":  string(c.Inputs.Group),
		"why":    why,
		"inputs": inputs,
	}
	if c.Containerized != nil {
		obj["containerized"] = *c.Containerized
	}
	if c.Features != nil {
		obj["features"] = append([]string{}, *c.Features...)
	}
	if c.Origin != nil {
		obj["origin"] = *c.Origin
	}
	if c.StartupError != nil {
		obj["startup_error"] = *c.StartupError
	}
	if len(c.SideEffects) > 0 {
		list := make([]interface{}, 0, len(c.SideEffects))
		for _, se := range c.SideEffects {
			list = append(list, sideEffectObject(se))
		}
		obj["side_effects"] = list
	}
	if len(c.ConstructionWarnings) > 0 {
		obj["construction_warnings"] = warningsList(c.ConstructionWarnings)
	}
	if len(c.Updates) > 0 {
		list := make([]interface{}, 0, len(c.Updates))
		for _, u := range c.Updates {
			o := jsonObject{"seq_delta": u.SeqDelta}
			if u.TimedOut {
				o["timed_out"] = true
			}
			if len(u.Warnings) > 0 {
				o["warnings"] = warningsList(u.Warnings)
			}
			list = append(list, o)
		}
		obj["updates"] = list
	}
	return obj, nil
}

// inputsObject re-encodes the case inputs as the case file gave them.
func inputsObject(c *Case, startupFailed bool) (jsonObject, error) {
	obj := jsonObject{}
	if c.Env != nil {
		env := jsonObject{}
		for k, v := range c.Env {
			env[k] = v
		}
		obj["env"] = env
	}
	if c.YAML != nil {
		obj["yaml"] = *c.YAML
	}
	if c.FleetPolicy != nil {
		obj["fleet_policy"] = *c.FleetPolicy
	}
	if len(c.CLI) > 0 {
		list := make([]interface{}, 0, len(c.CLI))
		for _, o := range c.CLI {
			v, err := encodeTypedValue(o.Value)
			if err != nil {
				return nil, fmt.Errorf("cli %q: %w", o.Key, err)
			}
			list = append(list, jsonObject{"key": o.Key, "value": v})
		}
		obj["cli"] = list
	}
	if len(c.Updates) > 0 {
		list := make([]interface{}, 0, len(c.Updates))
		for _, u := range c.Updates {
			o := jsonObject{"key": u.Key, "source": u.Source}
			if u.Op == "unset" {
				o["op"] = u.Op
			} else {
				v, err := encodeTypedValue(u.Value)
				if err != nil {
					return nil, fmt.Errorf("update %q: %w", u.Key, err)
				}
				o["value"] = v
			}
			list = append(list, o)
		}
		obj["updates"] = list
	}
	if keysElided(c, startupFailed) {
		return obj, nil
	}
	keys := make([]interface{}, 0, len(c.Keys))
	for _, k := range c.Keys {
		o := jsonObject{"key": k.Key}
		if k.Getters != nil {
			o["getters"] = append([]string{}, k.Getters...)
		}
		keys = append(keys, o)
	}
	obj["keys"] = keys
	return obj, nil
}

// keysElided reports whether `inputs.keys` is left out: the case started, no key has a `getters`
// override, and its keys are in byte order, so its key lines list exactly them (record.md §3.1).
func keysElided(c *Case, startupFailed bool) bool {
	if startupFailed {
		return false
	}
	for i, k := range c.Keys {
		if k.Getters != nil || (i > 0 && c.Keys[i-1].Key >= k.Key) {
			return false
		}
	}
	return true
}

// Validate checks the key line's consistency rules.
func (k *KeyLine) Validate() error {
	if k.Case == "" || k.Key == "" {
		return fmt.Errorf("%w: case or key", ErrRecordField)
	}
	if err := validateRead(&k.SnapshotRead); err != nil {
		return err
	}
	if k.FinalRead != nil {
		if err := validateRead(k.FinalRead); err != nil {
			return err
		}
	}
	if len(k.Events) > 0 && !k.bound {
		return fmt.Errorf("%w: key %q", ErrRecordUnboundEvents, k.Key)
	}
	for j, e := range k.Events {
		if e.Seq == 0 {
			return fmt.Errorf("%w: key %q, event %d has seq 0", ErrRecordEventSeq, k.Key, j)
		}
		if k.bound && (e.Update < 0 || e.Update >= len(k.caseUpdates)) {
			return fmt.Errorf("%w: key %q, event %d names update %d", ErrRecordEventIndex, k.Key, j, e.Update)
		}
	}
	return nil
}

// ErrRecordEventSeq marks an event whose relative seq is not positive.
var ErrRecordEventSeq = errors.New("event seq must be at least 1")

// validateRead checks every getter warning in one read.
func validateRead(r *Read) error {
	for _, g := range r.Getters {
		for _, w := range g.Warnings {
			if err := w.Validate(); err != nil {
				return err
			}
		}
	}
	return nil
}

func (k *KeyLine) object() (jsonObject, error) {
	obj := jsonObject{"type": "key", "case": k.Case, "key": k.Key}
	if k.Snapshot != nil {
		obj["snapshot"] = settingObject(k.Snapshot)
	} else {
		obj["snapshot"] = nil
	}
	if len(k.Events) > 0 {
		omit := k.omitsUpdate()
		list := make([]interface{}, 0, len(k.Events))
		for _, e := range k.Events {
			o := settingObject(&e.Setting)
			o["seq"] = e.Seq
			if !omit {
				o["update"] = e.Update
			}
			list = append(list, o)
		}
		obj["events"] = list
	}

	// Each read's source is compared with the streamed source a reader would reconstruct it from.
	var snapshotSource *string
	if k.Snapshot != nil {
		snapshotSource = &k.Snapshot.Source
	}
	reads := jsonObject{"snapshot": readObject(&k.SnapshotRead, snapshotSource)}
	if k.FinalRead != nil {
		finalSource := snapshotSource
		if n := len(k.Events); n > 0 {
			finalSource = &k.Events[n-1].Source
		}
		reads["final"] = readObject(k.FinalRead, finalSource)
	}
	obj["reads"] = reads
	return obj, nil
}

func readObject(r *Read, streamed *string) jsonObject {
	getters := make([]interface{}, 0, len(r.Getters))
	for _, g := range r.Getters {
		o := jsonObject{"getter": g.Getter, "result": g.Result}
		if len(g.Warnings) > 0 {
			o["warnings"] = warningsList(g.Warnings)
		}
		getters = append(getters, o)
	}
	obj := jsonObject{"getters": getters, "go_type": r.GoType}
	if streamed == nil || *streamed != r.Source {
		obj["source"] = r.Source
	}
	return obj
}

func settingObject(s *Setting) jsonObject {
	obj := jsonObject{"source": s.Source}
	if s.UnsetSource != "" {
		obj["unset_source"] = s.UnsetSource
	}
	if s.Value != nil {
		obj["value"] = s.Value
	}
	return obj
}

func warningsList(ws []Warning) []interface{} {
	list := make([]interface{}, 0, len(ws))
	for _, w := range ws {
		list = append(list, jsonObject{"level": w.Level, "message": w.Message})
	}
	return list
}
