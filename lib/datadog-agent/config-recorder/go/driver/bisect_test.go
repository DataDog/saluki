// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package driver

import (
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"sync"
	"testing"

	"github.com/DataDog/datadog-agent/pkg/config/model"

	"github.com/DataDog/datadog-agent/cmd/config-recorder/record"
)

func keyEntries(keys ...string) []record.KeyEntry {
	var out []record.KeyEntry
	for _, k := range keys {
		out = append(out, record.KeyEntry{Key: k})
	}
	return out
}

func strp(s string) *string { return &s }

var testBase = map[string]record.Setting{"b": {Source: "default"}, "gone": {Source: "default"},
	"p": {Source: "config-post-init"}}

func runWith(c *record.Case, sources map[string]string, absent ...string) *record.RunResult {
	r := defaultRun(c, sources)
	for i := range r.Run.Keys {
		if slices.Contains(absent, r.Run.Keys[i].Key) {
			r.Run.Keys[i].Snapshot = nil
		}
	}
	return r
}

func TestIsCleanAbsentKeys(t *testing.T) {
	c := &record.Case{Name: "c", Group: record.GroupUnknown, Keys: keyEntries("b", "new")}
	if IsClean(runWith(c, nil, "b"), nil, testBase) {
		t.Error("a key absent from the case but present in the baseline is not clean")
	}
	if !IsClean(runWith(c, map[string]string{}, "new"), nil, testBase) {
		t.Error("a key absent from both the case and the baseline is clean")
	}
}

func TestExpectedSourcesMultiplySet(t *testing.T) {
	c := &record.Case{Name: "c", Env: map[string]string{"DD_B": "1"}, YAML: strp("b: 2\np: 3\n"),
		CLI: []record.CLIOverride{{Key: "p", Value: "4"}}, Keys: keyEntries("b", "p")}
	got, err := ExpectedSources(c, map[string][]string{"DD_B": {"b"}})
	if err != nil {
		t.Fatal(err)
	}
	want := map[string]model.Source{"b": model.SourceEnvVar, "p": model.SourceCLI}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("got %v, want %v", got, want)
	}
}

func TestProjectInputs(t *testing.T) {
	c := &record.Case{Name: "c", Group: record.GroupBreadth, Why: []string{},
		Env:  map[string]string{"DD_SHARED": "x", "DD_A": "1"},
		YAML: strp("a: !!str 1\nsec:\n  b: 'q'\n  c: \"r\"\n"),
		Updates: []record.Update{{Key: "sec.c", Value: 1, Source: "cli"}, {Key: "a", Value: 2, Source: "cli"},
			{Key: "sec.c", Value: 3, Source: "cli"}},
		Keys: keyEntries("a", "sec.b", "sec.c")}
	envKeys := map[string][]string{"DD_SHARED": {"a", "sec.c"}, "DD_A": {"a"}}
	sp, err := newSplitter(c, envKeys)
	if err != nil {
		t.Fatal(err)
	}
	p0, err := sp.project(c.Keys[:1], "c--a")
	if err != nil {
		t.Fatal(err)
	}
	p1, err := sp.project(c.Keys[1:], "c")
	if err != nil {
		t.Fatal(err)
	}
	if p0.Name != "c--a" || p1.Name != "c" || p1.Group != c.Group {
		t.Fatalf("names %q %q", p0.Name, p1.Name)
	}
	if !reflect.DeepEqual(p0.Env, map[string]string{"DD_SHARED": "x", "DD_A": "1"}) ||
		!reflect.DeepEqual(p1.Env, map[string]string{"DD_SHARED": "x"}) {
		t.Errorf("env: %v %v", p0.Env, p1.Env)
	}
	if *p0.YAML != "a: !!str 1\n" || *p1.YAML != "sec:\n    b: 'q'\n    c: \"r\"\n" {
		t.Errorf("yaml: %q %q", *p0.YAML, *p1.YAML)
	}
	var vals []interface{}
	for _, u := range p1.Updates {
		vals = append(vals, u.Value)
	}
	if len(p0.Updates) != 1 || !reflect.DeepEqual(vals, []interface{}{1, 3}) {
		t.Errorf("updates: %v %v", p0.Updates, p1.Updates)
	}
	bad := *c
	bad.CLI = []record.CLIOverride{{Key: "other", Value: "1"}}
	if _, err := newSplitter(&bad, envKeys); !errors.Is(err, ErrUnassignable) {
		t.Errorf("unassignable cli: %v", err)
	}
}

func TestSplitterRejectsYAMLAliases(t *testing.T) {
	for _, tc := range []struct{ yaml, want string }{
		{"a: &x 1\nb: *x\n", "anchor &x"},
		{"sec: {b: 1}\nother:\n  <<: {c: 2}\n", "merge key <<"},
	} {
		c := &record.Case{Name: "c", Group: record.GroupBreadth, YAML: strp(tc.yaml), Keys: keyEntries("a", "b")}
		_, err := newSplitter(c, nil)
		if !errors.Is(err, ErrYAMLAlias) || !strings.Contains(err.Error(), `case "c"`) ||
			!strings.Contains(err.Error(), tc.want) {
			t.Errorf("yaml %q: got %v, want ErrYAMLAlias naming the case and %s", tc.yaml, err, tc.want)
		}
	}
	// A case that is clean is never split, so its aliases are no failure.
	a := t.TempDir()
	writeCase(t, a, "c", "group: breadth\nyaml: \"a: &x 1\\nb: *x\\n\"\nkeys: [a, b]\n")
	runner := &splitRunner{clean: "file"}
	if _, _, err := driveSplit(t, a, runner); err != nil {
		t.Fatalf("clean case with aliases: %v", err)
	}
	runner = &splitRunner{clean: "file", dirty: map[string]bool{"a": true}}
	if _, _, err := driveSplit(t, a, runner); !errors.Is(err, ErrYAMLAlias) {
		t.Fatalf("split case with aliases: got %v, want ErrYAMLAlias", err)
	}
}

// splitRunner answers runs of cases over keys that are all in its baseline, from the layer clean
// (default `default`): a key in dirty streams `cli` instead, and a run whose keys satisfy fails gets a startup
// error. It records every run's scratch.
type splitRunner struct {
	mu      sync.Mutex
	scratch []string
	clean   string
	dirty   map[string]bool
	fails   func(keys map[string]bool) bool
}

var splitBase = []string{"a", "b", "c", "d", "e"}

func (f *splitRunner) RunBaseline() (*record.RunResult, error) {
	snap := map[string]record.Setting{}
	for _, k := range splitBase {
		snap[k] = record.Setting{Source: "default"}
	}
	return &record.RunResult{Run: &record.CaseRun{Origin: "datadog.yaml", Features: []string{}, Snapshot: snap}}, nil
}

func (f *splitRunner) Run(c *record.Case, s Scratch) (*record.RunResult, error) {
	f.mu.Lock()
	f.scratch = append(f.scratch, s.Path())
	f.mu.Unlock()
	if f.fails != nil && f.fails(caseKeySet(c)) {
		msg := "boom"
		return &record.RunResult{StartupError: &msg}, nil
	}
	snap := map[string]record.Setting{}
	for _, k := range splitBase {
		snap[k] = record.Setting{Source: "default"}
	}
	run := &record.CaseRun{Origin: "datadog.yaml", Features: []string{}, Snapshot: snap}
	for _, k := range c.Keys {
		st := record.Setting{Source: "default"}
		if f.clean != "" {
			st.Source = f.clean
		}
		if f.dirty[k.Key] {
			st.Source = "cli"
		}
		snap[k.Key] = st
		run.Keys = append(run.Keys, record.KeyLine{Case: c.Name, Key: k.Key,
			Snapshot: &st, SnapshotRead: record.Read{GoType: "<nil>", Source: st.Source}})
	}
	return &record.RunResult{Run: run}, nil
}

// driveSplit drives the case files in dir with runner and returns each recorded case's keys, by
// case name, and the snapshot dump directory.
func driveSplit(t *testing.T, dir string, runner Runner) (map[string][]string, string, error) {
	t.Helper()
	out := filepath.Join(t.TempDir(), "corpus.jsonl")
	snaps := t.TempDir()
	opts := Options{CaseDirs: []string{dir}, Out: out, AgentCommit: strings.Repeat("a", 40),
		ContainerImage: "img@sha256:" + strings.Repeat("0", 64), InputsDigest: "sha256:" + strings.Repeat("1", 64),
		Jobs: 3, SnapshotDir: snaps}
	if err := Drive(opts, runner); err != nil {
		return nil, snaps, err
	}
	data, err := os.ReadFile(out)
	if err != nil {
		t.Fatal(err)
	}
	got := map[string][]string{}
	for _, line := range strings.Split(strings.TrimSuffix(string(data), "\n"), "\n") {
		switch {
		case strings.Contains(line, `"type":"case"`):
			got[between(line, `"case":"`, `"`)] = []string{}
		case strings.Contains(line, `"type":"key"`):
			c := between(line, `"case":"`, `"`)
			got[c] = append(got[c], between(line, `"key":"`, `"`))
		}
	}
	return got, snaps, nil
}

func TestDriveSplits(t *testing.T) {
	for _, tc := range []struct {
		name    string
		keys    string
		dirty   []string
		fails   func(keys map[string]bool) bool
		scratch []string
		records map[string][]string
		// discarded are the discarded runs with a first snapshot.
		discarded []string
	}{
		{
			name: "peel one key, rerun clean", keys: "a, b, c", dirty: []string{"b"},
			scratch:   []string{"c/1-c", "c/2-c--b", "c/3-c"},
			records:   map[string][]string{"c": {"a", "c"}, "c--b": {"b"}},
			discarded: []string{"c/1-c"},
		},
		{
			name: "peel all keys", keys: "a, b, c", dirty: []string{"a", "b", "c"},
			scratch:   []string{"c/1-c", "c/2-c--a", "c/3-c--b", "c/4-c--c"},
			records:   map[string][]string{"c--a": {"a"}, "c--b": {"b"}, "c--c": {"c"}},
			discarded: []string{"c/1-c"},
		},
		{
			name: "one culprit", keys: "a, b, c, d", fails: func(k map[string]bool) bool { return k["c"] },
			scratch:   []string{"c/1-c", "c/2-c", "c/3-c", "c/4-c--c", "c/5-c--d", "c/6-c"},
			records:   map[string][]string{"c": {"a", "b", "d"}, "c--c": {}},
			discarded: []string{"c/2-c", "c/5-c--d"},
		},
		{
			name: "two culprits in different halves", keys: "a, b, c, d",
			fails:     func(k map[string]bool) bool { return k["a"] || k["d"] },
			scratch:   []string{"c/1-c", "c/2-c", "c/3-c--a", "c/4-c--b", "c/5-c", "c/6-c--c", "c/7-c--d", "c/8-c"},
			records:   map[string][]string{"c": {"b", "c"}, "c--a": {}, "c--d": {}},
			discarded: []string{"c/4-c--b", "c/6-c--c"},
		},
		{
			name: "a culprit, then a mismatch on the rerun", keys: "a, b, c, d", dirty: []string{"c"},
			fails:     func(k map[string]bool) bool { return k["a"] },
			scratch:   []string{"c/1-c", "c/2-c", "c/3-c--a", "c/4-c--b", "c/5-c", "c/6-c", "c/7-c--c", "c/8-c"},
			records:   map[string][]string{"c": {"b", "d"}, "c--a": {}, "c--c": {"c"}},
			discarded: []string{"c/4-c--b", "c/5-c", "c/6-c"},
		},
		{
			name: "a failure that needs both halves", keys: "a, b, c, d",
			fails:     func(k map[string]bool) bool { return k["a"] && k["d"] },
			scratch:   []string{"c/1-c", "c/2-c", "c/3-c", "c/4-c--a", "c/5-c--b", "c/6-c--c", "c/7-c--d"},
			records:   map[string][]string{"c--a": {"a"}, "c--b": {"b"}, "c--c": {"c"}, "c--d": {"d"}},
			discarded: []string{"c/2-c", "c/3-c"},
		},
		{
			name: "the last remaining key fails alone", keys: "a, b", dirty: []string{"a"},
			fails: func(k map[string]bool) bool { return k["b"] && !k["a"] },
			// The rerun of the root with only b fails: that run is b's own failed run.
			scratch:   []string{"c/1-c", "c/2-c--a", "c/3-c"},
			records:   map[string][]string{"c--a": {"a"}, "c--b": {}},
			discarded: []string{"c/1-c"},
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			dir := t.TempDir()
			writeCase(t, dir, "c", "group: breadth\nkeys: ["+tc.keys+"]\n")
			runner := &splitRunner{dirty: map[string]bool{}, fails: tc.fails}
			for _, k := range tc.dirty {
				runner.dirty[k] = true
			}
			got, snaps, err := driveSplit(t, dir, runner)
			if err != nil {
				t.Fatal(err)
			}
			if !reflect.DeepEqual(runner.scratch, tc.scratch) {
				t.Errorf("processes (%d):\n got %v\nwant %v", len(runner.scratch), runner.scratch, tc.scratch)
			}
			if !reflect.DeepEqual(got, tc.records) {
				t.Errorf("records:\n got %v\nwant %v", got, tc.records)
			}
			var discarded []string
			_ = filepath.WalkDir(filepath.Join(snaps, "discarded"), func(p string, d os.DirEntry, err error) error {
				if err == nil && !d.IsDir() {
					rel, _ := filepath.Rel(filepath.Join(snaps, "discarded"), p)
					discarded = append(discarded, strings.TrimSuffix(rel, ".jsonl"))
				}
				return nil
			})
			if !slices.Equal(discarded, tc.discarded) {
				t.Errorf("discarded dumps %v, want %v", discarded, tc.discarded)
			}
			for name, keys := range tc.records {
				_, err := os.Stat(filepath.Join(snaps, name+".jsonl"))
				if started := len(keys) > 0; started != (err == nil) {
					t.Errorf("dump of recorded case %s: %v", name, err)
				}
			}
		})
	}
}

func TestDriveNeverSplitsBehaviorOrSingleKey(t *testing.T) {
	dir := t.TempDir()
	writeCase(t, dir, "h", "group: behavior\nwhy: [w]\nkeys: [a, b]\n")
	writeCase(t, dir, "s", "group: breadth\nkeys: [a]\n")
	runner := &splitRunner{dirty: map[string]bool{"a": true, "b": true}}
	got, _, err := driveSplit(t, dir, runner)
	if err != nil {
		t.Fatal(err)
	}
	slices.Sort(runner.scratch)
	if want := []string{"h/1-h", "s/1-s"}; !slices.Equal(runner.scratch, want) {
		t.Errorf("processes %v, want %v", runner.scratch, want)
	}
	if want := map[string][]string{"h": {"a", "b"}, "s": {"a"}}; !reflect.DeepEqual(got, want) {
		t.Errorf("records %v, want %v", got, want)
	}
}

func TestDrivePartNameCollision(t *testing.T) {
	dir := t.TempDir()
	writeCase(t, dir, "c", "group: breadth\nkeys: [a, b]\n")
	writeCase(t, dir, "c--b", "group: breadth\nkeys: [a]\n")
	runner := &splitRunner{dirty: map[string]bool{"b": true}}
	if _, _, err := driveSplit(t, dir, runner); !errors.Is(err, ErrPartName) {
		t.Fatalf("got %v, want ErrPartName", err)
	}
	// The colliding part never ran: the name is checked before the run.
	slices.Sort(runner.scratch)
	if want := []string{"c--b/1-c--b", "c/1-c"}; !slices.Equal(runner.scratch, want) {
		t.Fatalf("processes %v, want %v", runner.scratch, want)
	}
}

func TestDriveDumpsBaseline(t *testing.T) {
	dir := t.TempDir()
	writeCase(t, dir, "c", "group: breadth\nkeys: [x]\n")
	_, snaps, err := driveSplit(t, dir, &fakeRunner{})
	if err != nil {
		t.Fatal(err)
	}
	dump, err := os.ReadFile(filepath.Join(snaps, BaselineName+".jsonl"))
	if err != nil || string(dump) != "{\"key\":\"b\",\"source\":\"default\"}\n{\"key\":\"gone\",\"source\":\"default\"}\n" {
		t.Fatalf("baseline dump %q, %v", dump, err)
	}
}

func TestSideEffects(t *testing.T) {
	base := map[string]record.Setting{
		"same": {Source: "default", Value: json.RawMessage(`1`)},
		"val":  {Source: "default", Value: json.RawMessage(`1`)},
		"uns":  {Source: "default"},
		"gone": {Source: "default"},
		"k":    {Source: "default"},
	}
	snap := map[string]record.Setting{
		"same": {Source: "default", Value: json.RawMessage(`1`)},
		"val":  {Source: "default", Value: json.RawMessage(`2`)},
		"uns":  {Source: "default", UnsetSource: "file"},
		"B":    {Source: "file"},
		"k":    {Source: "file"},
	}
	got := SideEffects(base, snap, keyEntries("k"))
	var lines []string
	for _, se := range got {
		b, err := record.MarshalSideEffect(se)
		if err != nil {
			t.Fatal(err)
		}
		lines = append(lines, string(b))
	}
	want := []string{`{"key":"B","source":"file"}`, `{"absent":true,"key":"gone"}`,
		`{"key":"uns","source":"default","unset_source":"file"}`, `{"key":"val","source":"default","value":2}`}
	if !reflect.DeepEqual(lines, want) {
		t.Fatalf("got %v\nwant %v", lines, want)
	}
}
