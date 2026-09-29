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
	"os/exec"
	"path/filepath"
	"strings"

	"github.com/DataDog/datadog-agent/cmd/config-recorder/driver"
	"github.com/DataDog/datadog-agent/cmd/config-recorder/record"
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
	if err := fs.Parse(args); err != nil {
		return nil, fmt.Errorf("%w: %v", errUsage, err)
	}
	if len(f.casesDirs) == 0 || f.workdir == "" || f.out == "" || f.schemaPath == "" || f.agentCommit == "" ||
		f.containerImage == "" || f.inputsDigest == "" || fs.NArg() > 0 {
		return nil, fmt.Errorf("%w: drive --cases <dir> [--cases <dir>...] --workdir <dir> --out <file> --schema <file> "+
			"--agent-commit <sha> --container-image <ref@sha256:digest> --inputs-digest <sha256:hex>", errUsage)
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

// driveMain runs the baseline and each case in its own run-case process and writes the corpus
// lines. It does not compute side effects.
func driveMain(args []string) error {
	flags, err := parseDriveFlags(args)
	if err != nil {
		return err
	}
	opts := driver.Options{
		CaseDirs:       flags.casesDirs,
		Out:            flags.out,
		AgentCommit:    flags.agentCommit,
		ContainerImage: flags.containerImage,
		InputsDigest:   flags.inputsDigest,
	}
	err = driver.Drive(opts, &execRunner{workdir: flags.workdir, schemaPath: flags.schemaPath})
	if errors.Is(err, driver.ErrDuplicateCase) {
		return fmt.Errorf("%w: %w", errUsage, err)
	}
	return err
}

// execRunner runs each case as a run-case process of this binary, with exactly the case's env
// (`env -i` plus the case's entries) and a fresh work directory.
type execRunner struct {
	workdir    string
	schemaPath string
}

// Run runs one run-case process with exactly the given env and a fresh work directory, and reads
// its result file.
func (r *execRunner) Run(name string, env map[string]string, caseArgs ...string) (*record.RunResult, error) {
	workdir, schemaPath := r.workdir, r.schemaPath
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
