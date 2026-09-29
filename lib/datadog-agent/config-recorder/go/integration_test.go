// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package main

import (
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"sort"
	"strings"
	"testing"

	"github.com/DataDog/datadog-agent/cmd/config-recorder/record"
)

// TestDriveOnTestdata builds the recorder binary and runs its real `drive` subcommand (which in
// turn runs `run-case` in its own process per case) over testdata/, checking the split rules end to end
// against the real Agent. It needs the merged Agent schema, given by regenerate.sh as
// CONFIG_RECORDER_TEST_SCHEMA; without it (for example, a plain `go test` outside the container
// pipeline) it skips.
func TestDriveOnTestdata(t *testing.T) {
	schema := os.Getenv("CONFIG_RECORDER_TEST_SCHEMA")
	if schema == "" {
		t.Skip("CONFIG_RECORDER_TEST_SCHEMA not set; run under regenerate.sh")
	}

	dir := t.TempDir()
	bin := filepath.Join(dir, "config-recorder")
	build := exec.Command("go", "build", "-o", bin, ".")
	if out, err := build.CombinedOutput(); err != nil {
		t.Fatalf("go build: %v\n%s", err, out)
	}

	out := filepath.Join(dir, "corpus.jsonl")
	work := filepath.Join(dir, "work")
	cmd := exec.Command(bin, "drive",
		"--cases", "testdata",
		"--workdir", work,
		"--out", out,
		"--schema", schema,
		"--agent-commit", strings.Repeat("a", 40),
		"--container-image", "test@sha256:"+strings.Repeat("0", 64),
		"--inputs-digest", "sha256:"+strings.Repeat("1", 64),
		"--jobs", "2",
	)
	if o, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("drive: %v\n%s", err, o)
	}

	data, err := os.ReadFile(out)
	if err != nil {
		t.Fatal(err)
	}
	// keysByCase and sourceByKey read the key lines' case, key and snapshot source.
	keysByCase := map[string][]string{}
	sourceByKey := map[string]string{}
	for _, line := range strings.Split(strings.TrimSuffix(string(data), "\n"), "\n") {
		switch {
		case strings.Contains(line, `"type":"case"`):
			keysByCase[between(line, `"case":"`, `"`)] = nil
		case strings.Contains(line, `"type":"key"`):
			c, k := between(line, `"case":"`, `"`), between(line, `"key":"`, `"`)
			keysByCase[c] = append(keysByCase[c], k)
			// The line has both `reads.snapshot.source` and the top-level `snapshot.source`; the
			// latter is the streamed source and is the last "source" field on the line.
			if i := strings.LastIndex(line, `"source":"`); i >= 0 {
				sourceByKey[c+"/"+k] = between(line[i:], `"source":"`, `"`)
			}
		}
	}

	// The YAML case's first run, as loaded: every key it sets is expected to stream `file`.
	const root = "breadth-example-yaml"
	loaded := []string{"cmd_port", "log_level", "proxy.http", "proxy.https"}
	first := readDump(t, filepath.Join(work, "snapshots", "discarded", root, "1-"+root+".jsonl"))

	// Names: the env case and the YAML root keep their names; every other case is a single-key
	// part of the YAML root named after its key.
	var parts []string
	for name, keys := range keysByCase {
		switch {
		case name == root || name == "breadth-example-env":
		case strings.HasPrefix(name, root+"--"):
			if len(keys) != 1 || name != root+"--"+record.Sanitize(keys[0]) {
				t.Errorf("part %s holds %v, want the single key its name gives", name, keys)
			}
			parts = append(parts, keys...)
		default:
			t.Errorf("unexpected case %s", name)
		}
	}
	// Partition: the root's recorded keys and its parts' keys are the loaded keys, each once.
	all := slices.Concat(slices.Clone(keysByCase[root]), parts)
	sort.Strings(all)
	if !slices.Equal(all, loaded) {
		t.Fatalf("root keys %v and part keys %v do not partition %v", keysByCase[root], parts, loaded)
	}
	// Parts only for keys that misbehaved: a key is a part exactly when its source in the root's
	// first run was not the `file` it was set from.
	for _, k := range loaded {
		if isPart := slices.Contains(parts, k); isPart != (first[k] != "file") {
			t.Errorf("key %s: first-run source %q, but part = %v", k, first[k], isPart)
		}
	}
	sort.Strings(parts)
	if want := []string{"proxy.http", "proxy.https"}; !slices.Equal(parts, want) {
		t.Errorf("parts %v, want %v", parts, want)
	}

	// The root's processes, in order: the first run, the two peeled parts, the clean rerun.
	entries, err := os.ReadDir(filepath.Join(work, "run", root))
	if err != nil {
		t.Fatal(err)
	}
	var procs []string
	for _, e := range entries {
		procs = append(procs, e.Name())
	}
	if want := []string{"1-" + root, "2-" + root + "--proxy-http", "3-" + root + "--proxy-https", "4-" + root}; !slices.Equal(procs, want) {
		t.Errorf("processes %v, want %v", procs, want)
	}

	// Each key's snapshot source. Pin-bump canary: proxy.http and proxy.https, set by YAML, stream
	// config-post-init because the Agent's proxy fixup rewrites them after loading, which is what
	// makes them misbehave in the batch. If a new Agent pin changes that fixup, this expectation,
	// and the peeled parts above, change with it, and the split rules must be checked again.
	// log_level and cmd_port stream the YAML source they were set from. The env case's keys stream
	// environment-variable and are never split.
	wantSources := map[string]string{
		root + "--proxy-http/proxy.http":   "config-post-init",
		root + "--proxy-https/proxy.https": "config-post-init",
		root + "/log_level":                "file",
		root + "/cmd_port":                 "file",
		"breadth-example-env/log_level":    "environment-variable",
		"breadth-example-env/cmd_port":     "environment-variable",
	}
	for k, want := range wantSources {
		if got := sourceByKey[k]; got != want {
			t.Errorf("source of %s: got %q, want %q", k, got, want)
		}
	}
}

// readDump reads a first-snapshot dump into each key's streamed source.
func readDump(t *testing.T, path string) map[string]string {
	t.Helper()
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	out := map[string]string{}
	for _, line := range strings.Split(strings.TrimSuffix(string(data), "\n"), "\n") {
		out[between(line, `"key":"`, `"`)] = between(line, `"source":"`, `"`)
	}
	return out
}

func between(s, start, end string) string {
	_, rest, ok := strings.Cut(s, start)
	if !ok {
		return ""
	}
	v, _, _ := strings.Cut(rest, end)
	return v
}
