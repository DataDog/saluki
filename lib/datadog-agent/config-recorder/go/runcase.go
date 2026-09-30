// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package main

import (
	"errors"
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"time"

	"github.com/DataDog/datadog-agent/comp/core/config"
	"github.com/DataDog/datadog-agent/pkg/config/env"
	"github.com/DataDog/datadog-agent/pkg/config/model"
	pb "github.com/DataDog/datadog-agent/pkg/proto/pbgo/core"

	"github.com/DataDog/datadog-agent/cmd/config-recorder/agentcfg"
	"github.com/DataDog/datadog-agent/cmd/config-recorder/record"
	"github.com/DataDog/datadog-agent/cmd/config-recorder/schema"
)

func runCaseMain(args []string) error {
	fs := flag.NewFlagSet("run-case", flag.ContinueOnError)
	casePath := fs.String("case", "", "case file to run")
	baseline := fs.Bool("baseline", false, "run the baseline, a case with no inputs and no keys, instead of --case")
	workdir := fs.String("workdir", "", "fresh directory for the case's config files")
	resultOut := fs.String("result-out", "", "result file to write")
	schemaPath := fs.String("schema", "", "the Agent's merged core schema YAML (required)")
	if err := fs.Parse(args); err != nil {
		return fmt.Errorf("%w: %v", errUsage, err)
	}
	if *workdir == "" || *resultOut == "" || *schemaPath == "" || (*casePath == "") == !*baseline || fs.NArg() > 0 {
		return fmt.Errorf("%w: run-case (--case <file> | --baseline) --workdir <dir> --result-out <file> --schema <file>", errUsage)
	}

	c := &record.Case{}
	if !*baseline {
		var err error
		if c, err = record.ParseCaseFile(*casePath); err != nil {
			return err
		}
	}
	if err := checkEnv(c.Env); err != nil {
		return err
	}
	sch, err := schema.Load(*schemaPath)
	if err != nil {
		return err
	}
	capture, err := installLogCapture()
	if err != nil {
		return err
	}
	params, err := writeInputs(c, *workdir)
	if err != nil {
		return err
	}
	result, err := runCase(c, params, capture, sch, *baseline)
	if err != nil {
		return err
	}
	return record.WriteRunResult(*resultOut, result)
}

// checkEnv checks that the process environment is exactly the case's env.
func checkEnv(want map[string]string) error {
	got := map[string]string{}
	for _, kv := range os.Environ() {
		k, v, _ := strings.Cut(kv, "=")
		got[k] = v
	}
	var diff []string
	for k, v := range want {
		if g, ok := got[k]; !ok || g != v {
			diff = append(diff, "missing or different: "+k)
		}
	}
	for k := range got {
		if _, ok := want[k]; !ok {
			diff = append(diff, "unexpected: "+k)
		}
	}
	if len(diff) > 0 {
		sort.Strings(diff)
		return fmt.Errorf("process environment is not the case env: %s", strings.Join(diff, ", "))
	}
	return nil
}

// writeInputs writes the case's config files into workdir and returns the config params.
func writeInputs(c *record.Case, workdir string) (config.Params, error) {
	if err := os.MkdirAll(workdir, 0o755); err != nil {
		return config.Params{}, err
	}
	var yamlText string
	if c.YAML != nil {
		yamlText = *c.YAML
	}
	cfgPath := filepath.Join(workdir, "datadog.yaml")
	if err := os.WriteFile(cfgPath, []byte(yamlText), 0o644); err != nil {
		return config.Params{}, err
	}
	var opts []func(*config.Params)
	if c.FleetPolicy != nil {
		fleetDir := filepath.Join(workdir, "fleet")
		if err := os.MkdirAll(fleetDir, 0o755); err != nil {
			return config.Params{}, err
		}
		if err := os.WriteFile(filepath.Join(fleetDir, "datadog.yaml"), []byte(*c.FleetPolicy), 0o644); err != nil {
			return config.Params{}, err
		}
		opts = append(opts, config.WithFleetPoliciesDirPath(fleetDir))
	}
	for _, o := range c.CLI {
		opts = append(opts, config.WithCLIOverride(o.Key, o.Value))
	}
	return config.NewAgentParams(cfgPath, opts...), nil
}

// updateWait is how long the recorder waits for an update's events (record.md §4.2).
const updateWait = 5 * time.Second

// runCase runs one case (or, when baseline is true, the baseline: a config built with no
// inputs): it reads the first snapshot and the case's keys, applies the case's updates, and reads
// the keys again. When baseline is true it also checks that the schema's leaves match the Agent's
// own key set (getter-map.md §1). A construction error in the baseline fails the recorder rather
// than producing a startup_error (record.md §4.1): the baseline has no inputs to get wrong, so
// its construction failure means the recorder itself is broken.
func runCase(c *record.Case, params config.Params, capture *logCapture, sch schema.Schema, baseline bool) (*record.RunResult, error) {
	run := &record.CaseRun{}
	err := withFirstSnapshot(params, func(sess *session) error {
		cfg := sess.cfg
		names := record.CaseWarningNames(c)
		construction := capture.take()
		run.ConstructionWarnings = record.FilterWarnings(construction, names)
		if baseline {
			if err := agentcfg.CheckSchemaKeys(sch, cfg); err != nil {
				return err
			}
			if err := agentcfg.CheckEnvBindings(sch, cfg); err != nil {
				return err
			}
		}
		run.Origin = sess.snapshot.GetOrigin()
		run.Snapshot = map[string]record.Setting{}
		for _, s := range sess.snapshot.GetSettings() {
			setting, err := agentcfg.SettingFromProto(s)
			if err != nil {
				return fmt.Errorf("key %q: %w", s.GetKey(), err)
			}
			run.Snapshot[s.GetKey()] = setting
		}
		run.Containerized = env.IsContainerized()
		for f := range env.GetDetectedFeatures() {
			run.Features = append(run.Features, string(f))
		}
		sort.Strings(run.Features)

		lists, err := getterLists(cfg, c.Keys, sch)
		capture.take()
		if err != nil {
			return err
		}
		keys := make([]string, len(c.Keys))
		for i, entry := range c.Keys {
			keys[i] = entry.Key
			read, err := readKey(cfg, entry.Key, lists[i], capture)
			if err != nil {
				return err
			}
			kl := record.KeyLine{Case: c.Name, Key: entry.Key, SnapshotRead: read}
			if s, ok := run.Snapshot[entry.Key]; ok {
				kl.Snapshot = &s
			}
			run.Keys = append(run.Keys, kl)
		}
		if len(c.Updates) == 0 {
			// record.md §4.2: the sequence-end check still applies, taken right after the
			// snapshot reads since there are no updates and so no final reads.
			final, err := relativeSeq(sess, cfg.GetSequenceID())
			if err != nil {
				return err
			}
			return record.CheckSequenceEnd(final, nil)
		}

		ranges, events, err := applyUpdates(sess, c, capture, names, run)
		if err != nil {
			return err
		}
		attributed, err := record.Attribute(keys, ranges, events)
		if err != nil {
			return err
		}
		for i, entry := range c.Keys {
			run.Keys[i].Events = attributed.KeyEvents[i]
			read, err := readKey(cfg, entry.Key, lists[i], capture)
			if err != nil {
				return err
			}
			run.Keys[i].FinalRead = &read
		}
		final, err := relativeSeq(sess, cfg.GetSequenceID())
		if err != nil {
			return err
		}
		return record.CheckSequenceEnd(final, ranges)
	})
	var ce errConstruction
	if errors.As(err, &ce) {
		if baseline {
			// record.md §4.1: a startup_error in the baseline process fails the recorder.
			return nil, fmt.Errorf("baseline: config construction failed: %w", ce)
		}
		msg := ce.Error()
		return &record.RunResult{StartupError: &msg}, nil
	}
	if err != nil {
		return nil, err
	}
	return &record.RunResult{Run: run}, nil
}

// applyUpdates applies the case's updates in order (case.md §5). For each it reads the sequence
// ID before and after the call, records the warnings logged during the call, and, only when the
// sequence ID moved, receives events until one reaches the update's last sequence ID or the wait
// times out (record.md §4.2). It returns each update's range and every event received during a
// wait, in arrival order, for attribution.
func applyUpdates(sess *session, c *record.Case, capture *logCapture, names []string, run *record.CaseRun) ([]record.UpdateRange, []record.StreamEvent, error) {
	cfg := sess.cfg
	var events []record.StreamEvent
	var ranges []record.UpdateRange
	for i, u := range c.Updates {
		before, err := relativeSeq(sess, cfg.GetSequenceID())
		if err != nil {
			return nil, nil, err
		}
		if i == 0 {
			if err := record.CheckSequenceStart(before); err != nil {
				return nil, nil, err
			}
		}
		capture.take()
		switch u.Op {
		case "set":
			cfg.Set(u.Key, u.Value, model.Source(u.Source))
		case "unset":
			cfg.UnsetForSource(u.Key, model.Source(u.Source))
		default:
			return nil, nil, fmt.Errorf("update %d: unknown op %q", i, u.Op)
		}
		// The stream's notification callback runs inside Set and UnsetForSource, so its warnings
		// land here; the stream goroutine's own warnings (resync) do not.
		warnings := record.FilterWarnings(capture.take(), names)
		after, err := relativeSeq(sess, cfg.GetSequenceID())
		if err != nil {
			return nil, nil, err
		}
		r := record.UpdateRange{Before: before, After: after}
		ranges = append(ranges, r)
		result := record.UpdateResult{SeqDelta: r.SeqDelta(), Warnings: warnings}
		if r.WaitsFor() {
			received, timedOut, err := waitForUpdate(sess, r)
			if err != nil {
				return nil, nil, fmt.Errorf("update %d: %w", i, err)
			}
			events = append(events, received...)
			result.TimedOut = timedOut
		}
		run.Updates = append(run.Updates, result)
	}
	return ranges, events, nil
}

// waitForUpdate receives events until one whose relative sequence reaches r.After arrives, or
// updateWait passes. It returns the events received, in arrival order.
func waitForUpdate(sess *session, r record.UpdateRange) ([]record.StreamEvent, bool, error) {
	var out []record.StreamEvent
	deadline := time.NewTimer(updateWait)
	defer deadline.Stop()
	for {
		select {
		case ev, ok := <-sess.events:
			if !ok {
				return nil, false, errors.New("config stream closed during an update")
			}
			se, err := streamEvent(sess, ev)
			if err != nil {
				return nil, false, err
			}
			out = append(out, se)
			if r.EndsWait(se.Seq) {
				return out, false, nil
			}
		case <-deadline.C:
			return out, true, nil
		}
	}
}

// relativeSeq is seq relative to the first snapshot's sequence ID.
func relativeSeq(sess *session, seq uint64) (uint64, error) {
	if seq < sess.base {
		return 0, fmt.Errorf("sequence ID %d is before the first snapshot's %d", seq, sess.base)
	}
	return seq - sess.base, nil
}

// eventSeq is a stream event's int32 sequence ID relative to the first snapshot's.
func eventSeq(sess *session, id int32) (uint64, error) {
	if id < 0 {
		return 0, fmt.Errorf("negative sequence ID %d", id)
	}
	return relativeSeq(sess, uint64(id))
}

// streamEvent converts one stream event received during a wait. A resync ConfigSnapshot after
// the first snapshot fails the recorder in format 1 (record.md §4.2, §5.2); it needs a sequence
// gap this recorder never produces (one update at a time, values that always encode).
func streamEvent(sess *session, ev *pb.ConfigEvent) (record.StreamEvent, error) {
	u := ev.GetUpdate()
	if u == nil {
		return record.StreamEvent{}, errors.New("config stream event is a resync snapshot, a harness failure in format 1")
	}
	seq, err := eventSeq(sess, u.GetSequenceId())
	if err != nil {
		return record.StreamEvent{}, fmt.Errorf("update event: %w", err)
	}
	setting, err := agentcfg.SettingFromProto(u.GetSetting())
	if err != nil {
		return record.StreamEvent{}, fmt.Errorf("update event, key %q: %w", u.GetSetting().GetKey(), err)
	}
	return record.StreamEvent{Seq: seq, Key: u.GetSetting().GetKey(), Setting: setting}, nil
}

// getterLists selects each key's getter list. The caller discards the warnings of the
// GetAllSources calls it makes.
func getterLists(cfg model.Reader, keys []record.KeyEntry, sch schema.Schema) ([][]string, error) {
	lists := make([][]string, len(keys))
	for i, entry := range keys {
		list, _, _, err := agentcfg.SelectGetters(cfg, entry, sch)
		if err != nil {
			return nil, fmt.Errorf("key %q: %w", entry.Key, err)
		}
		lists[i] = list
	}
	return lists, nil
}

// readKey calls each getter in order, attributing to each the warnings logged during its call,
// then Get and GetSource.
func readKey(cfg model.Reader, key string, getters []string, capture *logCapture) (record.Read, error) {
	names := []string{strings.ToLower(key)}
	var read record.Read
	for _, g := range getters {
		capture.take()
		v, err := agentcfg.CallGetter(cfg, g, key)
		if err != nil {
			return read, err
		}
		warnings := record.FilterWarnings(capture.take(), names)
		encoded, err := record.EncodeResult(v)
		if err != nil {
			return read, fmt.Errorf("key %q, %s: %w", key, g, err)
		}
		read.Getters = append(read.Getters, record.GetterResult{Getter: g, Result: encoded, Warnings: warnings})
	}
	read.GoType = fmt.Sprintf("%T", cfg.Get(key))
	read.Source = cfg.GetSource(key).String()
	capture.take()
	return read, nil
}
