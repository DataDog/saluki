// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package main

import (
	"encoding/gob"
	"errors"
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"github.com/DataDog/datadog-agent/comp/core/config"
	"github.com/DataDog/datadog-agent/pkg/config/env"
	"github.com/DataDog/datadog-agent/pkg/config/model"
	pb "github.com/DataDog/datadog-agent/pkg/proto/pbgo/core"

	"github.com/DataDog/datadog-agent/cmd/config-recorder/corpus"
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

	c := &corpus.Case{}
	if !*baseline {
		var err error
		if c, err = corpus.ParseCaseFile(*casePath); err != nil {
			return err
		}
	}
	if len(c.Updates) > 0 {
		return errors.New("updates not supported yet")
	}
	if err := checkEnv(c.Env); err != nil {
		return err
	}
	schema, err := loadSchema(*schemaPath)
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
	result, err := runCase(c, params, capture, schema, *baseline)
	if err != nil {
		return err
	}
	return writeResult(*resultOut, result)
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
func writeInputs(c *corpus.Case, workdir string) (config.Params, error) {
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

// runCase runs one case (or, when baseline is true, the baseline: a config built with no
// inputs) and reads the first snapshot and the case's keys. When baseline is true it also checks
// that the schema's leaves match the Agent's own key set (getter-map.md §1), and a construction
// error is a harness failure rather than a startup_error (record.md §4.1): the baseline has no
// inputs to get wrong, so a construction failure there means the harness itself is broken.
func runCase(c *corpus.Case, params config.Params, capture *logCapture, schema agentSchema, baseline bool) (*corpus.RunResult, error) {
	result := &corpus.RunResult{}
	err := withFirstSnapshot(params, func(cfg config.Component, snapshot *pb.ConfigSnapshot) error {
		construction := capture.take()
		result.ConstructionWarnings = corpus.FilterWarnings(construction, corpus.CaseWarningNames(c))
		if baseline {
			if err := checkSchemaKeys(schema, cfg); err != nil {
				return err
			}
		}
		origin := snapshot.GetOrigin()
		result.Origin = &origin
		result.Snapshot = map[string]corpus.Setting{}
		for _, s := range snapshot.GetSettings() {
			setting, err := corpus.SettingFromProto(s)
			if err != nil {
				return fmt.Errorf("key %q: %w", s.GetKey(), err)
			}
			result.Snapshot[s.GetKey()] = setting
		}
		result.Containerized = env.IsContainerized()
		for f := range env.GetDetectedFeatures() {
			result.Features = append(result.Features, string(f))
		}
		sort.Strings(result.Features)

		lists, err := getterLists(cfg, c.Keys, schema)
		capture.take()
		if err != nil {
			return err
		}
		for i, entry := range c.Keys {
			read, err := readKey(cfg, entry.Key, lists[i], capture)
			if err != nil {
				return err
			}
			kl := corpus.KeyLine{Case: c.Name, Key: entry.Key, SnapshotRead: read}
			if s, ok := result.Snapshot[entry.Key]; ok {
				kl.Snapshot = &s
			}
			result.Keys = append(result.Keys, kl)
		}
		return nil
	})
	var ce errConstruction
	if errors.As(err, &ce) {
		if baseline {
			// record.md §4.1: a startup_error in the baseline process is a harness failure.
			return nil, fmt.Errorf("baseline: config construction failed: %w", ce)
		}
		msg := ce.Error()
		return &corpus.RunResult{StartupError: &msg}, nil
	}
	if err != nil {
		return nil, err
	}
	return result, nil
}

// getterLists selects each key's getter list. GetAllSources is called only on schema leaves
// without an override, and its warnings are discarded by the caller.
func getterLists(cfg model.Reader, keys []corpus.KeyEntry, schema agentSchema) ([][]string, error) {
	lists := make([][]string, len(keys))
	for i, entry := range keys {
		sk, ok := schema[entry.Key]
		if !ok {
			sk = schemaKey{Kind: corpus.KeyUnknown}
		}
		var typ string
		var hasDefault bool
		if entry.Getters == nil && sk.Kind == corpus.KeyLeaf {
			typ, hasDefault = corpus.DefaultLayerType(cfg, entry.Key)
		}
		list, err := corpus.GettersForKey(entry, sk.Kind, hasDefault, typ, sk.DeclaredType, sk.ElementType, sk.Tags)
		if err != nil {
			return nil, fmt.Errorf("key %q: %w", entry.Key, err)
		}
		lists[i] = list
	}
	return lists, nil
}

// readKey calls each getter in order, attributing to each the warnings logged during its call,
// then Get and GetSource.
func readKey(cfg model.Reader, key string, getters []string, capture *logCapture) (corpus.Read, error) {
	names := []string{strings.ToLower(key)}
	var read corpus.Read
	for _, g := range getters {
		capture.take()
		v, err := corpus.CallGetter(cfg, g, key)
		if err != nil {
			return read, err
		}
		warnings := corpus.FilterWarnings(capture.take(), names)
		encoded, err := corpus.EncodeResult(v)
		if err != nil {
			return read, fmt.Errorf("key %q, %s: %w", key, g, err)
		}
		read.Getters = append(read.Getters, corpus.GetterResult{Getter: g, Result: encoded, Warnings: warnings})
	}
	read.GoType = fmt.Sprintf("%T", cfg.Get(key))
	read.Source = cfg.GetSource(key).String()
	capture.take()
	return read, nil
}

func writeResult(path string, r *corpus.RunResult) error {
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

func readResult(path string) (*corpus.RunResult, error) {
	f, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer f.Close()
	r := &corpus.RunResult{}
	if err := gob.NewDecoder(f).Decode(r); err != nil {
		return nil, fmt.Errorf("%s: %w", path, err)
	}
	return r, nil
}
