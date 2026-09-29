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
	"regexp"
	"runtime"
	"slices"
	"sort"
	"strings"

	"github.com/DataDog/datadog-agent/cmd/config-recorder/corpus"
)

// commitPattern matches a full lowercase hex git commit ID.
var commitPattern = regexp.MustCompile(`^[0-9a-f]{40}$`)

// inputsDigestPattern matches the header's inputs_digest: `sha256:` and 64 lowercase hex
// (record.md §2).
var inputsDigestPattern = regexp.MustCompile(`^sha256:[0-9a-f]{64}$`)

// driveFlags holds the parsed flags of the drive subcommand.
type driveFlags struct {
	casesDir       string
	workdir        string
	out            string
	schemaPath     string
	agentCommit    string
	containerImage string
	inputsDigest   string
}

// parseDriveFlags parses and validates the drive subcommand's flags. Every flag is required. A
// missing or malformed flag is a usage error.
func parseDriveFlags(args []string) (*driveFlags, error) {
	var f driveFlags
	fs := flag.NewFlagSet("drive", flag.ContinueOnError)
	fs.StringVar(&f.casesDir, "cases", "", "directory of *.yaml case files")
	fs.StringVar(&f.workdir, "workdir", "", "directory for per-case work directories and result files")
	fs.StringVar(&f.out, "out", "", "corpus file to write")
	fs.StringVar(&f.schemaPath, "schema", "", "the Agent's merged core schema YAML, passed to each case process")
	fs.StringVar(&f.agentCommit, "agent-commit", "", "full 40-hex Agent commit the recorder was built from")
	fs.StringVar(&f.containerImage, "container-image", "", "digest-pinned reference of the image the recorder runs in")
	fs.StringVar(&f.inputsDigest, "inputs-digest", "", "sha256: and 64 lowercase hex digest of the recorder inputs (record.md \u00a72)")
	if err := fs.Parse(args); err != nil {
		return nil, fmt.Errorf("%w: %v", errUsage, err)
	}
	if f.casesDir == "" || f.workdir == "" || f.out == "" || f.schemaPath == "" || f.agentCommit == "" ||
		f.containerImage == "" || f.inputsDigest == "" || fs.NArg() > 0 {
		return nil, fmt.Errorf("%w: drive --cases <dir> --workdir <dir> --out <file> --schema <file> "+
			"--agent-commit <sha> --container-image <ref@sha256:digest> --inputs-digest <sha256:hex>", errUsage)
	}
	if !commitPattern.MatchString(f.agentCommit) {
		return nil, fmt.Errorf("%w: --agent-commit %q is not a 40-character lowercase hex commit", errUsage, f.agentCommit)
	}
	if !strings.Contains(f.containerImage, "@sha256:") {
		return nil, fmt.Errorf("%w: --container-image %q is not digest-pinned (no @sha256:)", errUsage, f.containerImage)
	}
	if !inputsDigestPattern.MatchString(f.inputsDigest) {
		return nil, fmt.Errorf("%w: --inputs-digest %q is not sha256: and 64 lowercase hex digits", errUsage, f.inputsDigest)
	}
	return &f, nil
}

// driveMain is a minimal driver: it runs the baseline and each case in its own process and writes
// the corpus lines. It does not compute side effects.
func driveMain(args []string) error {
	flags, err := parseDriveFlags(args)
	if err != nil {
		return err
	}
	files, err := filepath.Glob(filepath.Join(flags.casesDir, "*.yaml"))
	if err != nil {
		return err
	}
	sort.Strings(files)

	baseResult, err := runChild(flags.workdir, "baseline", nil, flags.schemaPath, "--baseline")
	if err != nil {
		return err
	}
	base := baseResult.Run
	if base == nil {
		return fmt.Errorf("baseline: no run result")
	}
	header := &corpus.HeaderLine{
		AgentCommit:    flags.agentCommit,
		GOOS:           runtime.GOOS,
		GOARCH:         runtime.GOARCH,
		GoVersion:      runtime.Version(),
		ContainerImage: flags.containerImage,
		Containerized:  base.Containerized,
		Features:       base.Features,
		InputsDigest:   flags.inputsDigest,
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
		r, err := runChild(flags.workdir, "case-"+c.Name, c.Env, flags.schemaPath, "--case", file)
		if err != nil {
			return err
		}
		cl := &corpus.CaseLine{Inputs: c, StartupError: r.StartupError}
		var keys []corpus.KeyLine
		if run := r.Run; run != nil {
			cl.Origin = &run.Origin
			cl.ConstructionWarnings = run.ConstructionWarnings
			cl.Updates = run.Updates
			keys = run.Keys
			if run.Containerized != header.Containerized {
				v := run.Containerized
				cl.Containerized = &v
			}
			if !slices.Equal(run.Features, header.Features) {
				v := run.Features
				cl.Features = &v
			}
		}
		if err := corpus.CheckCaseRecord(cl, keys); err != nil {
			return fmt.Errorf("case %q: %w", c.Name, err)
		}
		records = append(records, record{cl, keys})
	}
	sort.Slice(records, func(i, j int) bool { return records[i].caseLine.Inputs.Name < records[j].caseLine.Inputs.Name })

	f, err := os.Create(flags.out)
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
	return corpus.ReadRunResult(resultPath)
}
