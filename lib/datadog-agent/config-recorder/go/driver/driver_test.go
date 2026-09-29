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
	"strings"
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
	cases, files, err := LoadCases([]string{a, b})
	if err != nil {
		t.Fatal(err)
	}
	if len(cases) != 2 || cases[0].Name != "one" || cases[1].Name != "two" || len(files) != 2 {
		t.Fatalf("got %d cases, %v", len(cases), files)
	}
	writeCase(t, b, "one", "group: baseline\nkeys: [a]\n")
	if _, _, err := LoadCases([]string{a, b}); !errors.Is(err, ErrDuplicateCase) {
		t.Fatalf("duplicate name: got %v, want ErrDuplicateCase", err)
	}
}

// fakeRunner records each run and answers with one key line per case key, all from the default
// layer, or with a startup error for the cases named in fail.
type fakeRunner struct {
	runs []string
	envs []map[string]string
	fail map[string]bool
}

func (f *fakeRunner) Run(name string, env map[string]string, caseArgs ...string) (*record.RunResult, error) {
	f.runs = append(f.runs, name+" "+strings.Join(caseArgs, " "))
	f.envs = append(f.envs, env)
	run := &record.CaseRun{Origin: "datadog.yaml", Features: []string{}, Snapshot: map[string]record.Setting{}}
	if name == "baseline" {
		return &record.RunResult{Run: run}, nil
	}
	c, err := record.ParseCaseFile(caseArgs[1])
	if err != nil {
		return nil, err
	}
	if f.fail[c.Name] {
		msg := "boom"
		return &record.RunResult{StartupError: &msg}, nil
	}
	for _, k := range c.Keys {
		run.Keys = append(run.Keys, record.KeyLine{Case: c.Name, Key: k.Key,
			Snapshot: &record.Setting{Source: "default"}, SnapshotRead: record.Read{GoType: "<nil>", Source: "default"}})
	}
	return &record.RunResult{Run: run}, nil
}

func TestDriveOrdersCasesAndKeys(t *testing.T) {
	a, b := t.TempDir(), t.TempDir()
	writeCase(t, a, "zeta", "group: breadth\nenv: {DD_Z: \"1\"}\nkeys: [z, b, a]\n")
	writeCase(t, a, "alpha", "group: breadth\nkeys: [m]\n")
	writeCase(t, b, "mid", "group: breadth\nyaml: \"x: 1\\n\"\nkeys: [k]\n")
	out := filepath.Join(t.TempDir(), "corpus.jsonl")
	runner := &fakeRunner{fail: map[string]bool{"mid": true}}
	opts := Options{CaseDirs: []string{a, b}, Out: out, AgentCommit: strings.Repeat("a", 40),
		ContainerImage: "img@sha256:" + strings.Repeat("0", 64), InputsDigest: "sha256:" + strings.Repeat("1", 64)}
	if err := Drive(opts, runner); err != nil {
		t.Fatal(err)
	}

	// The baseline runs first, then the cases in directory order and then file name order.
	wantRuns := []string{
		"baseline --baseline",
		"case-alpha --case " + filepath.Join(a, "alpha.yaml"),
		"case-zeta --case " + filepath.Join(a, "zeta.yaml"),
		"case-mid --case " + filepath.Join(b, "mid.yaml"),
	}
	if !reflect.DeepEqual(runner.runs, wantRuns) {
		t.Fatalf("runs:\n got %v\nwant %v", runner.runs, wantRuns)
	}
	if runner.envs[0] != nil || !reflect.DeepEqual(runner.envs[2], map[string]string{"DD_Z": "1"}) {
		t.Errorf("envs: %v", runner.envs)
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

func (missingKeyRunner) Run(string, map[string]string, ...string) (*record.RunResult, error) {
	return &record.RunResult{Run: &record.CaseRun{Origin: "datadog.yaml", Features: []string{}}}, nil
}

func between(s, start, end string) string {
	_, rest, _ := strings.Cut(s, start)
	v, _, _ := strings.Cut(rest, end)
	return v
}
