// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

// Package driver is what the config recorder's `drive` subcommand does: it loads the case files,
// runs the baseline and each case through a Runner, builds and checks the case lines, and writes
// the corpus in order and reads it back. It runs no Agent code itself.
package driver

import (
	"bufio"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"sort"

	"github.com/DataDog/datadog-agent/cmd/config-recorder/record"
)

// ErrDuplicateCase marks a case name found in more than one case file.
var ErrDuplicateCase = errors.New("case name is used by two case files")

// Runner runs one case process and returns its result. name is unique per run; env is the whole
// process environment; caseArgs selects the baseline or a case file.
type Runner interface {
	Run(name string, env map[string]string, caseArgs ...string) (*record.RunResult, error)
}

// Options are the drive inputs that are not case files.
type Options struct {
	// CaseDirs are the directories of *.yaml case files.
	CaseDirs []string
	// Out is the corpus file to write.
	Out            string
	AgentCommit    string
	ContainerImage string
	InputsDigest   string
}

type caseRecord struct {
	caseLine *record.CaseLine
	keys     []record.KeyLine
}

// Drive runs the baseline and every case with runner, and writes the corpus to opts.Out.
func Drive(opts Options, runner Runner) error {
	cases, files, err := LoadCases(opts.CaseDirs)
	if err != nil {
		return err
	}

	baseResult, err := runner.Run("baseline", nil, "--baseline")
	if err != nil {
		return err
	}
	base := baseResult.Run
	if base == nil {
		return fmt.Errorf("baseline: no run result")
	}
	header := &record.HeaderLine{
		AgentCommit:    opts.AgentCommit,
		GOOS:           runtime.GOOS,
		GOARCH:         runtime.GOARCH,
		GoVersion:      runtime.Version(),
		ContainerImage: opts.ContainerImage,
		Containerized:  base.Containerized,
		Features:       base.Features,
		InputsDigest:   opts.InputsDigest,
	}

	var records []caseRecord
	for i, c := range cases {
		r, err := runner.Run("case-"+c.Name, c.Env, "--case", files[i])
		if err != nil {
			return err
		}
		rec, err := buildRecord(header, c, r)
		if err != nil {
			return err
		}
		records = append(records, rec)
	}
	return writeCorpus(opts.Out, header, records)
}

// buildRecord builds and checks one case's lines from its run result.
func buildRecord(header *record.HeaderLine, c *record.Case, r *record.RunResult) (caseRecord, error) {
	cl := &record.CaseLine{Inputs: c, StartupError: r.StartupError}
	var keys []record.KeyLine
	if run := r.Run; run != nil {
		cl.Origin = &run.Origin
		cl.ConstructionWarnings = record.SortConstructionWarnings(run.ConstructionWarnings)
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
	if err := record.CheckCaseRecord(cl, keys); err != nil {
		return caseRecord{}, fmt.Errorf("case %q: %w", c.Name, err)
	}
	return caseRecord{cl, keys}, nil
}

// orderedLines is the header, then each case line followed by its key lines, cases by name and
// keys by key (record.md §1).
func orderedLines(header *record.HeaderLine, records []caseRecord) []record.Line {
	records = slices.Clone(records)
	sort.Slice(records, func(i, j int) bool { return records[i].caseLine.Inputs.Name < records[j].caseLine.Inputs.Name })
	lines := []record.Line{header}
	for _, rec := range records {
		lines = append(lines, rec.caseLine)
		keys := slices.Clone(rec.keys)
		sort.Slice(keys, func(i, j int) bool { return keys[i].Key < keys[j].Key })
		for i := range keys {
			lines = append(lines, &keys[i])
		}
	}
	return lines
}

func writeCorpus(path string, header *record.HeaderLine, records []caseRecord) error {
	f, err := os.Create(path)
	if err != nil {
		return err
	}
	w := bufio.NewWriter(f)
	for _, l := range orderedLines(header, records) {
		if err := record.WriteLine(w, l); err != nil {
			f.Close()
			return err
		}
	}
	if err := w.Flush(); err != nil {
		f.Close()
		return err
	}
	if err := f.Close(); err != nil {
		return err
	}
	// Read the corpus back with the strict reader, so what the writer wrote is exactly what a reader
	// accepts and reconstructs.
	data, err := os.ReadFile(path)
	if err != nil {
		return err
	}
	if _, err := record.ReadCorpus(data); err != nil {
		return fmt.Errorf("corpus does not read back: %w", err)
	}
	return nil
}

// LoadCases parses every *.yaml case file in each directory, in directory order and then file
// name order, and returns the cases with their files. A case name found twice across the
// directories is ErrDuplicateCase.
func LoadCases(dirs []string) ([]*record.Case, []string, error) {
	var cases []*record.Case
	var files []string
	names := map[string]string{}
	for _, dir := range dirs {
		dirFiles, err := filepath.Glob(filepath.Join(dir, "*.yaml"))
		if err != nil {
			return nil, nil, err
		}
		sort.Strings(dirFiles)
		for _, file := range dirFiles {
			c, err := record.ParseCaseFile(file)
			if err != nil {
				return nil, nil, err
			}
			if other, dup := names[c.Name]; dup {
				return nil, nil, fmt.Errorf("%w: case name %q is in both %s and %s", ErrDuplicateCase, c.Name, other, file)
			}
			names[c.Name] = file
			cases = append(cases, c)
			files = append(files, file)
		}
	}
	return cases, files, nil
}
