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
	"bytes"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"sort"
	"strings"
	"sync"

	"github.com/DataDog/datadog-agent/cmd/config-recorder/record"
)

// ErrDuplicateCase marks a case name found in more than one case file.
var ErrDuplicateCase = errors.New("case name is used by two case files")

// ErrPartName marks a single-key part whose name equals another case's name (case.md §3.1).
var ErrPartName = errors.New("part name equals another case's name")

// BaselineName is the name of the baseline process and its snapshot dump.
const BaselineName = "_baseline"

// Scratch identifies one process: the root it serves (case.md §3.1), the case name it runs under,
// and its position among the root's processes, counted from 1. A root's processes run one after
// another, so the position is deterministic, and it makes the process's scratch files private
// even when two of the root's processes share a case name.
type Scratch struct {
	Root  string
	Index int
	Name  string
}

// Path is the process's own relative scratch path, `<root>/<i>-<name>`.
func (s Scratch) Path() string {
	return filepath.Join(s.Root, fmt.Sprintf("%d-%s", s.Index, s.Name))
}

// BaselineScratch is the baseline process's scratch.
var BaselineScratch = Scratch{Root: BaselineName, Index: 1, Name: BaselineName}

// Runner runs case processes. Run must be safe for concurrent use.
type Runner interface {
	// RunBaseline runs the baseline process, a case with no inputs, in BaselineScratch.
	RunBaseline() (*record.RunResult, error)
	// Run runs one case, a root or a part, in its own process, with its scratch files under s.
	Run(c *record.Case, s Scratch) (*record.RunResult, error)
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
	// EnvKeys maps each env var name the Agent binds to the keys it binds, for splitting.
	EnvKeys map[string][]string
	// Jobs is how many case processes run at once; 0 means the number of CPUs.
	Jobs int
	// SnapshotDir, when set, receives the first-snapshot dumps: `<name>.jsonl` for each recorded
	// case and for the baseline, and `discarded/<root>/<i>-<name>.jsonl` for every other run.
	SnapshotDir string
}

type caseRecord struct {
	caseLine *record.CaseLine
	keys     []record.KeyLine
}

// Drive runs the baseline and every case with runner, splitting batches that are not clean
// (case.md §3.1), and writes the corpus to opts.Out.
func Drive(opts Options, runner Runner) error {
	cases, err := LoadCases(opts.CaseDirs)
	if err != nil {
		return err
	}
	baseResult, err := runner.RunBaseline()
	if err != nil {
		return err
	}
	base := baseResult.Run
	if base == nil {
		return fmt.Errorf("baseline: no run result")
	}
	if err := dumpSnapshot(opts.SnapshotDir, BaselineName, base.Snapshot); err != nil {
		return err
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
	d := &drive{opts: opts, runner: runner, base: base.Snapshot, header: header, loaded: map[string]bool{},
		recorded: map[string]bool{}}
	for _, c := range cases {
		d.loaded[c.Name] = true
	}
	jobs := opts.Jobs
	if jobs <= 0 {
		jobs = runtime.NumCPU()
	}
	d.sem = make(chan struct{}, jobs)

	var (
		wg   sync.WaitGroup
		errs = make([]error, len(cases))
		recs = make([][]caseRecord, len(cases))
	)
	for i, c := range cases {
		wg.Add(1)
		go func() {
			defer wg.Done()
			x := &rootRun{drive: d, root: c}
			errs[i] = x.explore()
			recs[i] = x.records
		}()
	}
	wg.Wait()
	var failed []error
	for _, err := range errs {
		if err != nil {
			failed = append(failed, err)
		}
	}
	if len(failed) > 0 {
		slices.SortFunc(failed, func(a, b error) int { return strings.Compare(a.Error(), b.Error()) })
		return errors.Join(failed...)
	}
	return writeCorpus(opts.Out, header, slices.Concat(recs...))
}

// drive is the state shared by every root's procedure.
type drive struct {
	opts   Options
	runner Runner
	base   map[string]record.Setting
	header *record.HeaderLine
	// loaded are the loaded cases' names; it is not written after Drive starts the roots.
	loaded map[string]bool
	// sem bounds the processes running at once.
	sem chan struct{}

	mu sync.Mutex
	// recorded are the names of the recorded cases so far.
	recorded map[string]bool
}

// rootRun is the procedure for one loaded case, its root (case.md §3.1). It runs its processes
// one after another, so it needs no lock of its own.
type rootRun struct {
	*drive
	root *record.Case
	// n is the number of processes run so far for the root.
	n int
	// records are the root's recorded cases: the root itself and its single-key parts.
	records []caseRecord
}

// run runs one process for the root. A case under a part name is checked against the loaded
// case names before it runs.
func (x *rootRun) run(c *record.Case) (*record.RunResult, Scratch, error) {
	if c.Name != x.root.Name && x.loaded[c.Name] {
		return nil, Scratch{}, fmt.Errorf("%w: case %q: part %q", ErrPartName, x.root.Name, c.Name)
	}
	x.n++
	s := Scratch{Root: x.root.Name, Index: x.n, Name: c.Name}
	x.sem <- struct{}{}
	r, err := x.runner.Run(c, s)
	<-x.sem
	if err != nil {
		return nil, s, err
	}
	return r, s, nil
}

// discard dumps the first snapshot of a run that is not recorded, if it has one.
func (x *rootRun) discard(r *record.RunResult, s Scratch) error {
	if r.Run == nil || x.opts.SnapshotDir == "" {
		return nil
	}
	return dumpSnapshot(filepath.Join(x.opts.SnapshotDir, "discarded"), s.Path(), r.Run.Snapshot)
}

// record records c from its run r and dumps the run's first snapshot under c's name.
func (x *rootRun) record(c *record.Case, r *record.RunResult) error {
	x.mu.Lock()
	dup := x.recorded[c.Name] || (c.Name != x.root.Name && x.loaded[c.Name])
	x.recorded[c.Name] = true
	x.mu.Unlock()
	if dup {
		return fmt.Errorf("%w: case %q: part %q", ErrPartName, x.root.Name, c.Name)
	}
	if r.Run != nil {
		if err := dumpSnapshot(x.opts.SnapshotDir, c.Name, r.Run.Snapshot); err != nil {
			return err
		}
	}
	rec, err := buildRecord(x.header, x.base, c, r)
	if err != nil {
		return err
	}
	x.records = append(x.records, rec)
	return nil
}

// runPart runs and records the single-key part for key.
func (x *rootRun) runPart(sp *splitter, key record.KeyEntry) error {
	p, err := sp.project([]record.KeyEntry{key}, record.PartName(x.root.Name, key.Key))
	if err != nil {
		return err
	}
	r, _, err := x.run(p)
	if err != nil {
		return err
	}
	return x.record(p, r)
}

// explore runs the root and splits it by the rules of case.md §3.1. A root that is never split
// (its group, or a single key) is recorded as it ran. Otherwise, while keys remain: a run that
// started and has dirty keys peels each into a single-key part and reruns the rest; a run that
// failed to start searches for culprits, records each culprit's own failed run as its part and
// reruns the rest, or, with no culprit, makes every key a part. A clean run is recorded under the
// root's name.
func (x *rootRun) explore() error {
	r, s, err := x.run(x.root)
	if err != nil {
		return err
	}
	if !x.root.Group.Bisectable() || len(x.root.Keys) < 2 {
		return x.record(x.root, r)
	}
	var sp *splitter
	cur, keys := x.root, slices.Clone(x.root.Keys)
	for {
		var gone []string
		if started(r) {
			expected, err := ExpectedSources(cur, x.opts.EnvKeys)
			if err != nil {
				return fmt.Errorf("case %q: %w", x.root.Name, err)
			}
			gone = DirtyKeys(r, expected, x.base)
			if len(gone) == 0 {
				return x.record(cur, r)
			}
		}
		if sp == nil {
			if sp, err = newSplitter(x.root, x.opts.EnvKeys); err != nil {
				return err
			}
		}
		if err := x.discard(r, s); err != nil {
			return err
		}
		switch {
		case len(gone) > 0:
			for _, k := range keys {
				if slices.Contains(gone, k.Key) {
					if err := x.runPart(sp, k); err != nil {
						return err
					}
				}
			}
		case len(keys) == 1:
			// The root's last key failed on its own: this run is the culprit's own failed run, and
			// its inputs are exactly the part's, so it is recorded as the part.
			p, err := sp.project(keys, record.PartName(x.root.Name, keys[0].Key))
			if err != nil {
				return err
			}
			return x.record(p, r)
		default:
			if gone, err = x.culprits(sp, keys); err != nil {
				return err
			}
			if len(gone) == 0 {
				// The failure needs keys from both halves: every key becomes a part.
				for _, k := range keys {
					if err := x.runPart(sp, k); err != nil {
						return err
					}
				}
				return nil
			}
		}
		keys = slices.DeleteFunc(keys, func(k record.KeyEntry) bool { return slices.Contains(gone, k.Key) })
		if len(keys) == 0 {
			return nil
		}
		if cur, err = sp.project(keys, x.root.Name); err != nil {
			return err
		}
		if r, s, err = x.run(cur); err != nil {
			return err
		}
	}
}

// culprits searches keys, whose run failed to start, for the single keys that fail alone
// (case.md §3.1): it halves them in key order into n/2 keys and the rest, runs each half, discards
// a half that starts and searches a half that fails, down to one key. A culprit's run is recorded
// as its single-key part, so a one-key half runs under the part name. It returns the culprits in
// key order.
func (x *rootRun) culprits(sp *splitter, keys []record.KeyEntry) ([]string, error) {
	var found []string
	mid := len(keys) / 2
	for _, half := range [][]record.KeyEntry{keys[:mid], keys[mid:]} {
		name := x.root.Name
		if len(half) == 1 {
			name = record.PartName(x.root.Name, half[0].Key)
		}
		hc, err := sp.project(half, name)
		if err != nil {
			return nil, err
		}
		r, s, err := x.run(hc)
		if err != nil {
			return nil, err
		}
		switch {
		case started(r):
			if err := x.discard(r, s); err != nil {
				return nil, err
			}
		case len(half) == 1:
			if err := x.record(hc, r); err != nil {
				return nil, err
			}
			found = append(found, half[0].Key)
		default:
			if err := x.discard(r, s); err != nil {
				return nil, err
			}
			sub, err := x.culprits(sp, half)
			if err != nil {
				return nil, err
			}
			found = append(found, sub...)
		}
	}
	return found, nil
}

// dumpSnapshot writes a first snapshot to <dir>/<name>.jsonl, one line per setting sorted by key,
// each written as a side_effects element is. name may hold directories. It writes nothing when dir
// is empty.
func dumpSnapshot(dir, name string, snap map[string]record.Setting) error {
	if dir == "" {
		return nil
	}
	path := filepath.Join(dir, name+".jsonl")
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return err
	}
	keys := make([]string, 0, len(snap))
	for k := range snap {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	var buf bytes.Buffer
	for _, k := range keys {
		b, err := record.MarshalSideEffect(record.SideEffect{Key: k, Setting: snap[k]})
		if err != nil {
			return fmt.Errorf("snapshot %s: key %q: %w", name, k, err)
		}
		buf.Write(b)
		buf.WriteByte('\n')
	}
	return os.WriteFile(path, buf.Bytes(), 0o644)
}

// buildRecord builds and checks one case's lines from its run result.
func buildRecord(header *record.HeaderLine, base map[string]record.Setting, c *record.Case,
	r *record.RunResult) (caseRecord, error) {
	cl := &record.CaseLine{Inputs: c, StartupError: r.StartupError}
	var keys []record.KeyLine
	if run := r.Run; run != nil {
		cl.Origin = &run.Origin
		cl.ConstructionWarnings = record.SortConstructionWarnings(run.ConstructionWarnings)
		cl.Updates = run.Updates
		cl.SideEffects = SideEffects(base, run.Snapshot, c.Keys)
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
// name order. A case name found twice across the directories is ErrDuplicateCase.
func LoadCases(dirs []string) ([]*record.Case, error) {
	var cases []*record.Case
	names := map[string]string{}
	for _, dir := range dirs {
		dirFiles, err := filepath.Glob(filepath.Join(dir, "*.yaml"))
		if err != nil {
			return nil, err
		}
		sort.Strings(dirFiles)
		for _, file := range dirFiles {
			c, err := record.ParseCaseFile(file)
			if err != nil {
				return nil, err
			}
			if other, dup := names[c.Name]; dup {
				return nil, fmt.Errorf("%w: case name %q is in both %s and %s", ErrDuplicateCase, c.Name, other, file)
			}
			names[c.Name] = file
			cases = append(cases, c)
		}
	}
	return cases, nil
}
