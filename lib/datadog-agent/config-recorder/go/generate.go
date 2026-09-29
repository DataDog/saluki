// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package main

import (
	"flag"
	"fmt"
	"os"
	"path/filepath"

	"github.com/DataDog/datadog-agent/cmd/config-recorder/corpus"
	"github.com/DataDog/datadog-agent/cmd/config-recorder/gen"
)

// generateMain writes the generated case files. It builds the Agent config with no inputs, as
// probe does, to learn each schema key's default-layer Go type and the env vars the Agent binds.
func generateMain(args []string) error {
	fs := flag.NewFlagSet("generate", flag.ContinueOnError)
	schemaPath := fs.String("schema", "", "the Agent's merged core schema YAML (required)")
	overlayPath := fs.String("overlay", "", "saluki's schema overlay YAML (required)")
	out := fs.String("out", "", "directory to write the case files to; must exist and be empty (required)")
	if err := fs.Parse(args); err != nil || *schemaPath == "" || *overlayPath == "" || *out == "" || fs.NArg() > 0 {
		return fmt.Errorf("%w: generate --schema <file> --overlay <file> --out <dir>", errUsage)
	}
	entries, err := os.ReadDir(*out)
	if err != nil {
		return err
	}
	if len(entries) > 0 {
		return fmt.Errorf("%w: --out %s is not empty", errUsage, *out)
	}
	schemaData, err := os.ReadFile(*schemaPath)
	if err != nil {
		return err
	}
	schema, err := gen.ParseSchema(schemaData)
	if err != nil {
		return err
	}
	overlayData, err := os.ReadFile(*overlayPath)
	if err != nil {
		return err
	}
	overlay, err := gen.ParseOverlay(overlayData)
	if err != nil {
		return err
	}
	agentSchema, err := corpus.LoadSchema(*schemaPath)
	if err != nil {
		return err
	}

	capture, err := installLogCapture()
	if err != nil {
		return err
	}
	workdir, err := os.MkdirTemp("", "config-recorder-generate-")
	if err != nil {
		return err
	}
	defer os.RemoveAll(workdir)
	params, err := writeInputs(&corpus.Case{}, workdir)
	if err != nil {
		return err
	}
	facts := &gen.AgentFacts{DefaultType: map[string]string{}, EnvVars: map[string]bool{}}
	err = withFirstSnapshot(params, func(sess *session) error {
		cfg := sess.cfg
		capture.take()
		if err := corpus.CheckSchemaKeys(agentSchema, cfg); err != nil {
			return err
		}
		for _, key := range schema.Keys() {
			typ, ok := corpus.DefaultLayerType(cfg, key)
			if !ok {
				typ = "<nil>"
			}
			facts.DefaultType[key] = typ
		}
		for _, v := range cfg.GetEnvVars() {
			facts.EnvVars[v] = true
		}
		capture.take()
		return nil
	})
	if err != nil {
		return err
	}

	res, err := gen.Generate(schema, overlay, facts)
	if err != nil {
		return err
	}
	for _, c := range res.Cases {
		data, err := c.Marshal()
		if err != nil {
			return err
		}
		if err := os.WriteFile(filepath.Join(*out, c.Name+".yaml"), data, 0o644); err != nil {
			return err
		}
	}
	fmt.Fprintf(os.Stderr, "[*] generated %d cases\n", len(res.Cases))
	for _, s := range res.Skipped {
		fmt.Fprintf(os.Stderr, "[*] skipped %s %s %s: %s\n", s.Group, s.Source, s.Key, s.Reason)
	}
	return nil
}
