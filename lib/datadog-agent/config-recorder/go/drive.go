// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package main

import (
	"bufio"
	"flag"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"runtime/debug"
	"slices"
	"sort"

	"github.com/DataDog/datadog-agent/cmd/config-recorder/corpus"
)

// placeholderCommit stands in for the Agent commit when the build does not record it.
const placeholderCommit = "0000000000000000000000000000000000000000"

// driveMain is a minimal driver: it runs the baseline and each case in its own process and writes
// the corpus lines. It does not compute side effects.
func driveMain(args []string) error {
	fs := flag.NewFlagSet("drive", flag.ContinueOnError)
	casesDir := fs.String("cases", "", "directory of *.yaml case files")
	workdir := fs.String("workdir", "", "directory for per-case work directories and result files")
	out := fs.String("out", "", "corpus file to write")
	schemaPath := fs.String("schema", "", "the Agent's merged core schema YAML, passed to each case process (required)")
	image := fs.String("container-image", "unknown", "image reference written in the header")
	if err := fs.Parse(args); err != nil {
		return fmt.Errorf("%w: %v", errUsage, err)
	}
	if *casesDir == "" || *workdir == "" || *out == "" || *schemaPath == "" || fs.NArg() > 0 {
		return fmt.Errorf("%w: drive --cases <dir> --workdir <dir> --out <file> --schema <file>", errUsage)
	}
	files, err := filepath.Glob(filepath.Join(*casesDir, "*.yaml"))
	if err != nil {
		return err
	}
	sort.Strings(files)

	base, err := runChild(*workdir, "baseline", nil, *schemaPath, "--baseline")
	if err != nil {
		return err
	}
	header := &corpus.HeaderLine{
		AgentCommit:    agentCommit(),
		GOOS:           runtime.GOOS,
		GOARCH:         runtime.GOARCH,
		GoVersion:      runtime.Version(),
		ContainerImage: *image,
		Containerized:  base.Containerized,
		Features:       base.Features,
	}

	type record struct {
		caseLine *corpus.CaseLine
		keys     []corpus.KeyLine
	}
	var records []record
	for _, file := range files {
		c, err := corpus.ParseCaseFile(file)
		if err != nil {
			return err
		}
		r, err := runChild(*workdir, "case-"+c.Name, c.Env, *schemaPath, "--case", file)
		if err != nil {
			return err
		}
		cl := &corpus.CaseLine{
			Inputs:               c,
			Origin:               r.Origin,
			StartupError:         r.StartupError,
			ConstructionWarnings: r.ConstructionWarnings,
		}
		if r.StartupError == nil {
			if r.Containerized != header.Containerized {
				v := r.Containerized
				cl.Containerized = &v
			}
			if !slices.Equal(r.Features, header.Features) {
				v := r.Features
				cl.Features = &v
			}
		}
		if err := corpus.CheckCaseRecord(cl, r.Keys); err != nil {
			return fmt.Errorf("case %q: %w", c.Name, err)
		}
		records = append(records, record{cl, r.Keys})
	}
	sort.Slice(records, func(i, j int) bool { return records[i].caseLine.Inputs.Name < records[j].caseLine.Inputs.Name })

	f, err := os.Create(*out)
	if err != nil {
		return err
	}
	w := bufio.NewWriter(f)
	lines := []corpus.Line{header}
	for _, rec := range records {
		lines = append(lines, rec.caseLine)
		keys := slices.Clone(rec.keys)
		sort.Slice(keys, func(i, j int) bool { return keys[i].Key < keys[j].Key })
		for i := range keys {
			lines = append(lines, &keys[i])
		}
	}
	for _, l := range lines {
		if err := corpus.WriteLine(w, l); err != nil {
			f.Close()
			return err
		}
	}
	if err := w.Flush(); err != nil {
		f.Close()
		return err
	}
	return f.Close()
}

// runChild runs one run-case process with exactly the given env and a fresh work directory, and
// reads its result file.
func runChild(workdir, name string, env map[string]string, schemaPath string, caseArgs ...string) (*corpus.RunResult, error) {
	dir := filepath.Join(workdir, name)
	if err := os.RemoveAll(dir); err != nil {
		return nil, err
	}
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return nil, err
	}
	resultPath := filepath.Join(workdir, name+".gob")
	if err := os.RemoveAll(resultPath); err != nil {
		return nil, err
	}
	args := append([]string{"run-case"}, caseArgs...)
	args = append(args, "--workdir", dir, "--result-out", resultPath)
	if schemaPath != "" {
		args = append(args, "--schema", schemaPath)
	}
	cmd := exec.Command(os.Args[0], args...)
	cmd.Env = []string{}
	for k, v := range env {
		cmd.Env = append(cmd.Env, k+"="+v)
	}
	cmd.Stdout = os.Stderr
	cmd.Stderr = os.Stderr
	if err := cmd.Run(); err != nil {
		return nil, fmt.Errorf("%s: %w", name, err)
	}
	return readResult(resultPath)
}

// agentCommit is the VCS revision the build recorded, or a placeholder.
func agentCommit() string {
	if info, ok := debug.ReadBuildInfo(); ok {
		for _, s := range info.Settings {
			if s.Key == "vcs.revision" && len(s.Value) == 40 {
				return s.Value
			}
		}
	}
	return placeholderCommit
}
