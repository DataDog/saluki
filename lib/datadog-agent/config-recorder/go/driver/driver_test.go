// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package driver

import (
	"errors"
	"os"
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"sync"
	"testing"

	"github.com/DataDog/datadog-agent/cmd/config-recorder/record"
)

func writeCase(t *testing.T, dir, name, body string) {
	t.Helper()
	if err := os.WriteFile(filepath.Join(dir, name+".yaml"), []byte("name: "+name+"\n"+body), 0o644); err != nil {
		t.Fatal(err)
	}
}

func TestLoadCasesAcrossDirectories(t *testing.T) {
	a, b := t.TempDir(), t.TempDir()
	writeCase(t, a, "one", "group: baseline\nkeys: [a]\n")
	writeCase(t, b, "two", "group: baseline\nkeys: [a]\n")
	cases, err := LoadCases([]string{a, b})
	if err != nil {
		t.Fatal(err)
	}
	if len(cases) != 2 || cases[0].Name != "one" || cases[1].Name != "two" {
		t.Fatalf("got %d cases", len(cases))
	}
	writeCase(t, b, "one", "group: baseline\nkeys: [a]\n")
	if _, err := LoadCases([]string{a, b}); !errors.Is(err, ErrDuplicateCase) {
		t.Fatalf("duplicate name: got %v, want ErrDuplicateCase", err)
	}
}

// fakeRunner answers each case with one key line per case key, all from the default layer, or
// with a startup error for the cases named in fail, or by answer when it is set.
type fakeRunner struct {
	mu   sync.Mutex
	runs []string
	// scratch are the scratches of the case runs, in run order.
	scratch []Scratch
	fail    map[string]bool
	answer  func(c *record.Case) *record.RunResult
}

func (f *fakeRunner) RunBaseline() (*record.RunResult, error) {
	f.mu.Lock()
	f.runs = append(f.runs, "baseline")
	f.mu.Unlock()
	snap := map[string]record.Setting{"b": {Source: "default"}, "gone": {Source: "default"}}
	return &record.RunResult{Run: &record.CaseRun{Origin: "datadog.yaml", Features: []string{}, Snapshot: snap}}, nil
}

func (f *fakeRunner) Run(c *record.Case, s Scratch) (*record.RunResult, error) {
	f.mu.Lock()
	f.runs = append(f.runs, c.Name)
	f.scratch = append(f.scratch, s)
	f.mu.Unlock()
	if f.answer != nil {
		return f.answer(c), nil
	}
	if f.fail[c.Name] {
		msg := "boom"
		return &record.RunResult{StartupError: &msg}, nil
	}
	return defaultRun(c, nil), nil
}

// defaultRun answers every key from the default layer, except the keys in sources.
func defaultRun(c *record.Case, sources map[string]string) *record.RunResult {
	run := &record.CaseRun{Origin: "datadog.yaml", Features: []string{}, Snapshot: map[string]record.Setting{
		"b": {Source: "default"}, "gone": {Source: "default"}}}
	for _, k := range c.Keys {
		src := "default"
		if s, ok := sources[k.Key]; ok {
			src = s
		}
		st := record.Setting{Source: src}
		run.Snapshot[k.Key] = st
		run.Keys = append(run.Keys, record.KeyLine{Case: c.Name, Key: k.Key,
			Snapshot: &st, SnapshotRead: record.Read{GoType: "<nil>", Source: src}})
	}
	return &record.RunResult{Run: run}
}

func TestDriveOrdersCasesAndKeys(t *testing.T) {
	a, b := t.TempDir(), t.TempDir()
	writeCase(t, a, "zeta", "group: behavior\nwhy: [w]\nenv: {DD_Z: \"1\"}\nkeys: [z, b, a]\n")
	writeCase(t, a, "alpha", "group: breadth\nkeys: [m]\n")
	writeCase(t, b, "mid", "group: breadth\nyaml: \"x: 1\\n\"\nkeys: [k]\n")
	out := filepath.Join(t.TempDir(), "corpus.jsonl")
	runner := &fakeRunner{fail: map[string]bool{"mid": true}}
	opts := Options{CaseDirs: []string{a, b}, Out: out, AgentCommit: strings.Repeat("a", 40),
		ContainerImage: "img@sha256:" + strings.Repeat("0", 64), InputsDigest: "sha256:" + strings.Repeat("1", 64)}
	if err := Drive(opts, runner); err != nil {
		t.Fatal(err)
	}

	slices.Sort(runner.runs)
	if want := []string{"alpha", "baseline", "mid", "zeta"}; !reflect.DeepEqual(runner.runs, want) {
		t.Fatalf("runs: got %v, want %v", runner.runs, want)
	}

	// The corpus has the header, then cases by name, each followed by its keys in byte order.
	data, err := os.ReadFile(out)
	if err != nil {
		t.Fatal(err)
	}
	var got []string
	for _, line := range strings.Split(strings.TrimSuffix(string(data), "\n"), "\n") {
		switch {
		case strings.Contains(line, `"type":"header"`):
			got = append(got, "header")
		case strings.Contains(line, `"type":"case"`):
			got = append(got, "case "+between(line, `"case":"`, `"`))
		default:
			got = append(got, "key "+between(line, `"case":"`, `"`)+" "+between(line, `"key":"`, `"`))
		}
	}
	want := []string{"header", "case alpha", "key alpha m", "case mid", "case zeta", "key zeta a", "key zeta b",
		"key zeta z"}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("lines:\n got %v\nwant %v", got, want)
	}
}

func TestDriveRejectsBadRecord(t *testing.T) {
	a := t.TempDir()
	writeCase(t, a, "one", "group: breadth\nkeys: [a]\n")
	opts := Options{CaseDirs: []string{a}, Out: filepath.Join(t.TempDir(), "corpus.jsonl"),
		AgentCommit: strings.Repeat("a", 40), ContainerImage: "img@sha256:" + strings.Repeat("0", 64),
		InputsDigest: "sha256:" + strings.Repeat("1", 64)}
	if err := Drive(opts, &missingKeyRunner{}); err == nil {
		t.Fatal("a run with no key lines must fail the case check")
	}
}

// missingKeyRunner answers every case with no key lines.
type missingKeyRunner struct{}

func (missingKeyRunner) Run(*record.Case, Scratch) (*record.RunResult, error) {
	return &record.RunResult{Run: &record.CaseRun{Origin: "datadog.yaml", Features: []string{}}}, nil
}

func (missingKeyRunner) RunBaseline() (*record.RunResult, error) {
	return &record.RunResult{Run: &record.CaseRun{Origin: "datadog.yaml", Features: []string{}}}, nil
}

func between(s, start, end string) string {
	_, rest, _ := strings.Cut(s, start)
	v, _, _ := strings.Cut(rest, end)
	return v
}
