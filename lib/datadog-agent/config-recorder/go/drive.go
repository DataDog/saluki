// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package main

import (
	"bytes"
	"errors"
	"flag"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"

	"github.com/DataDog/datadog-agent/cmd/config-recorder/driver"
	"github.com/DataDog/datadog-agent/cmd/config-recorder/record"
	"github.com/DataDog/datadog-agent/cmd/config-recorder/schema"
)

// driveFlags holds the parsed flags of the drive subcommand.
type driveFlags struct {
	casesDirs      caseDirs
	workdir        string
	out            string
	schemaPath     string
	agentCommit    string
	containerImage string
	inputsDigest   string
	jobs           int
}

// parseDriveFlags parses and validates the drive subcommand's flags. Every flag is required. A
// missing or malformed flag is a usage error.
func parseDriveFlags(args []string) (*driveFlags, error) {
	var f driveFlags
	fs := flag.NewFlagSet("drive", flag.ContinueOnError)
	fs.Var(&f.casesDirs, "cases", "directory of *.yaml case files; may be repeated")
	fs.StringVar(&f.workdir, "workdir", "", "directory for per-case work directories and result files")
	fs.StringVar(&f.out, "out", "", "corpus file to write")
	fs.StringVar(&f.schemaPath, "schema", "", "the Agent's merged core schema YAML, passed to each case process")
	fs.StringVar(&f.agentCommit, "agent-commit", "", "full 40-hex Agent commit the recorder was built from")
	fs.StringVar(&f.containerImage, "container-image", "", "digest-pinned reference of the image the recorder runs in")
	fs.StringVar(&f.inputsDigest, "inputs-digest", "", "sha256: and 64 lowercase hex digest of the recorder inputs (record.md \u00a72)")
	fs.IntVar(&f.jobs, "jobs", 0, "case processes to run at once; 0 means the number of CPUs")
	if err := fs.Parse(args); err != nil {
		return nil, fmt.Errorf("%w: %v", errUsage, err)
	}
	if len(f.casesDirs) == 0 || f.workdir == "" || f.out == "" || f.schemaPath == "" || f.agentCommit == "" ||
		f.containerImage == "" || f.inputsDigest == "" || f.jobs < 0 || fs.NArg() > 0 {
		return nil, fmt.Errorf("%w: drive --cases <dir> [--cases <dir>...] --workdir <dir> --out <file> --schema <file> "+
			"--agent-commit <sha> --container-image <ref@sha256:digest> --inputs-digest <sha256:hex> [--jobs <n>]", errUsage)
	}
	if !record.CommitPattern.MatchString(f.agentCommit) {
		return nil, fmt.Errorf("%w: --agent-commit %q is not a 40-character lowercase hex commit", errUsage, f.agentCommit)
	}
	if !strings.Contains(f.containerImage, "@sha256:") {
		return nil, fmt.Errorf("%w: --container-image %q is not digest-pinned (no @sha256:)", errUsage, f.containerImage)
	}
	if !record.InputsDigestPattern.MatchString(f.inputsDigest) {
		return nil, fmt.Errorf("%w: --inputs-digest %q is not sha256: and 64 lowercase hex digits", errUsage, f.inputsDigest)
	}
	return &f, nil
}

// driveMain runs the baseline and each case in its own run-case process, splitting batches that
// are not clean, and writes the corpus lines with each case's side effects. The workdir holds each
// process's private scratch directory (`run/<root>/<i>-<name>/`: its case file, work directory
// and result file) and the first-snapshot dumps (`snapshots/`).
func driveMain(args []string) error {
	flags, err := parseDriveFlags(args)
	if err != nil {
		return err
	}
	sch, err := schema.Load(flags.schemaPath)
	if err != nil {
		return err
	}
	for _, sub := range []string{"run", "snapshots"} {
		dir := filepath.Join(flags.workdir, sub)
		if err := os.RemoveAll(dir); err != nil {
			return err
		}
		if err := os.MkdirAll(dir, 0o755); err != nil {
			return err
		}
	}
	opts := driver.Options{
		CaseDirs:       flags.casesDirs,
		Out:            flags.out,
		AgentCommit:    flags.agentCommit,
		ContainerImage: flags.containerImage,
		InputsDigest:   flags.inputsDigest,
		EnvKeys:        schema.EnvKeys(sch.EnvBindings()),
		Jobs:           flags.jobs,
		SnapshotDir:    filepath.Join(flags.workdir, "snapshots"),
	}
	err = driver.Drive(opts, &execRunner{workdir: flags.workdir, schemaPath: flags.schemaPath})
	if errors.Is(err, driver.ErrDuplicateCase) {
		return fmt.Errorf("%w: %w", errUsage, err)
	}
	return err
}

// execRunner runs each case as a run-case process of this binary, with exactly the case's env
// (`env -i` plus the case's entries) and a fresh work directory. It is safe for concurrent use:
// every process has its own scratch directory (scratchDir).
type execRunner struct {
	workdir    string
	schemaPath string
}

// scratchDir is a process's private scratch directory, `<workdir>/run/<root>/<i>-<name>`.
func scratchDir(workdir string, s driver.Scratch) string {
	return filepath.Join(workdir, "run", s.Path())
}

// RunBaseline runs the baseline process.
func (r *execRunner) RunBaseline() (*record.RunResult, error) {
	return r.run(driver.BaselineScratch, nil, "--baseline")
}

// Run writes the case to `<name>.yaml` in its scratch directory and runs a run-case process on it.
func (r *execRunner) Run(c *record.Case, s driver.Scratch) (*record.RunResult, error) {
	dir := scratchDir(r.workdir, s)
	if err := os.RemoveAll(dir); err != nil {
		return nil, err
	}
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return nil, err
	}
	path := filepath.Join(dir, c.Name+".yaml")
	if err := record.WriteCaseFile(path, c); err != nil {
		return nil, fmt.Errorf("case %q: %w", c.Name, err)
	}
	return r.run(s, c.Env, "--case", path)
}

// run runs one run-case process with its work directory `work/` and result file `result.gob` in
// the process's scratch directory.
func (r *execRunner) run(s driver.Scratch, env map[string]string, caseArgs ...string) (*record.RunResult, error) {
	dir := scratchDir(r.workdir, s)
	workDir := filepath.Join(dir, "work")
	resultPath := filepath.Join(dir, "result.gob")
	for _, p := range []string{workDir, resultPath} {
		if err := os.RemoveAll(p); err != nil {
			return nil, err
		}
	}
	if err := os.MkdirAll(workDir, 0o755); err != nil {
		return nil, err
	}
	args := append([]string{"run-case"}, caseArgs...)
	args = append(args, "--workdir", workDir, "--result-out", resultPath, "--schema", r.schemaPath)
	cmd := exec.Command(os.Args[0], args...)
	cmd.Env = []string{}
	for k, v := range env {
		cmd.Env = append(cmd.Env, k+"="+v)
	}
	var out bytes.Buffer
	cmd.Stdout = &out
	cmd.Stderr = &out
	if err := cmd.Run(); err != nil {
		os.Stderr.Write(out.Bytes())
		return nil, fmt.Errorf("%s: %w", s.Path(), err)
	}
	return record.ReadRunResult(resultPath)
}

// caseDirs is the repeatable --cases flag.
type caseDirs []string

func (d *caseDirs) String() string { return strings.Join(*d, ",") }

func (d *caseDirs) Set(v string) error {
	if v == "" {
		return errors.New("empty directory")
	}
	*d = append(*d, v)
	return nil
}
