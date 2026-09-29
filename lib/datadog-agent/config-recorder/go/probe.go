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
	"sort"
	"strings"

	"github.com/DataDog/datadog-agent/cmd/config-recorder/corpus"
)

// probeMain builds the config with no inputs and reports getter selection for every schema leaf,
// without calling any getter.
func probeMain(args []string) error {
	fs := flag.NewFlagSet("probe", flag.ContinueOnError)
	schemaPath := fs.String("schema", "", "the Agent's merged core schema YAML (required)")
	if err := fs.Parse(args); err != nil || *schemaPath == "" || fs.NArg() > 0 {
		return fmt.Errorf("%w: probe --schema <file>", errUsage)
	}
	schema, err := corpus.LoadSchema(*schemaPath)
	if err != nil {
		return err
	}
	capture, err := installLogCapture()
	if err != nil {
		return err
	}
	workdir, err := os.MkdirTemp("", "config-recorder-probe-")
	if err != nil {
		return err
	}
	defer os.RemoveAll(workdir)
	params, err := writeInputs(&corpus.Case{}, workdir)
	if err != nil {
		return err
	}
	withDefault := map[string]int{}
	noDefault := map[string]int{}
	var unknown []string
	err = withFirstSnapshot(params, func(sess *session) error {
		cfg := sess.cfg
		capture.take()
		if err := corpus.CheckSchemaKeys(schema, cfg); err != nil {
			return err
		}
		for _, key := range schema.Leaves() {
			sk := schema[key]
			list, typ, hasDefault, err := corpus.SelectGetters(cfg, corpus.KeyEntry{Key: key}, schema)
			if errors.Is(err, corpus.ErrGetterDefaultType) {
				unknown = append(unknown, fmt.Sprintf("%s %s", key, typ))
				continue
			}
			if err != nil {
				return err
			}
			getters := strings.Join(list, ",")
			if hasDefault {
				if strings.HasPrefix(typ, "[]map[string]") {
					typ = "[]map[string]…"
				}
				withDefault[fmt.Sprintf("%-28s -> %s", typ, getters)]++
			} else {
				row := sk.DeclaredType
				if row == "" {
					row = "(no type)"
				}
				if sk.ElementType != "" {
					row += " of " + sk.ElementType
				}
				noDefault[fmt.Sprintf("%-28s -> %s", row, getters)]++
			}
		}
		capture.take()
		return nil
	})
	if err != nil {
		return err
	}
	fmt.Printf("schema leaves: %d\n", len(schema.Leaves()))
	printCounts("with a default, by default-layer %T", withDefault)
	printCounts("no default, by declared schema type", noDefault)
	fmt.Printf("default %%T not in the getter table: %d\n", len(unknown))
	for _, u := range unknown {
		fmt.Printf("  %s\n", u)
	}
	return nil
}

func printCounts(title string, counts map[string]int) {
	total := 0
	rows := make([]string, 0, len(counts))
	for r, n := range counts {
		rows = append(rows, r)
		total += n
	}
	sort.Strings(rows)
	fmt.Printf("%s: %d\n", title, total)
	for _, r := range rows {
		fmt.Printf("  %5d  %s\n", counts[r], r)
	}
}
