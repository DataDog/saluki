// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package record

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"math/rand"
	"reflect"
	"sort"
	"strings"
	"testing"
)

func TestReadGoldenLines(t *testing.T) {
	c, err := ReadCorpus([]byte(goldenLines))
	if err != nil {
		t.Fatal(err)
	}
	if len(c.Cases) != 2 {
		t.Fatalf("cases: %d", len(c.Cases))
	}
	// The elided inputs.keys is rebuilt from the key lines, and the omitted read source from the
	// snapshot's.
	if got := c.Cases[0].Line.Inputs.Keys; !reflect.DeepEqual(got, []KeyEntry{{Key: "additional_endpoints"}}) {
		t.Errorf("keys: %v", got)
	}
	if got := c.Cases[1].Keys[0].SnapshotRead.Source; got != "file" {
		t.Errorf("read source: %q", got)
	}
}

const readerHeader = `{"agent_commit":"281d921619d52ce7b99aef40607285992c9c2e89","container_image":"i","containerized":false,"features":[],"format":1,"go_version":"go1.26.7","goarch":"arm64","goos":"linux","inputs_digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000","type":"header"}` + "\n"

const readerKeyLine = `{"case":"c","events":[{"seq":1,"source":"agent-runtime","value":2}],"key":"a","reads":{"final":{"getters":[],"go_type":"int"},"snapshot":{"getters":[],"go_type":"int"}},"snapshot":{"source":"file","value":1},"type":"key"}` + "\n"

func TestReadRejectsNonCanonical(t *testing.T) {
	good := readerHeader +
		`{"case":"c","group":"breadth","inputs":{"updates":[{"key":"a","source":"agent-runtime","value":2}],"yaml":"a: 1\n"},"origin":"datadog.yaml","type":"case","updates":[{"seq_delta":1}],"why":[]}` + "\n" +
		readerKeyLine
	c, err := ReadCorpus([]byte(good))
	if err != nil {
		t.Fatalf("good corpus: %v", err)
	}
	if ev := c.Cases[0].Keys[0].Events[0]; ev.Update != 0 || ev.Seq != 1 {
		t.Errorf("event: %+v", ev)
	}
	for name, bad := range map[string]string{
		"explicit op set":      strings.Replace(good, `{"key":"a","source"`, `{"key":"a","op":"set","source"`, 1),
		"keys written":         strings.Replace(good, `"inputs":{`, `"inputs":{"keys":[{"key":"a"}],`, 1),
		"update written":       strings.Replace(good, `"seq":1,`, `"seq":1,"update":0,`, 1),
		"index written":        strings.Replace(good, `[{"seq_delta":1}]`, `[{"index":0,"seq_delta":1}]`, 1),
		"events count":         strings.Replace(good, `[{"seq_delta":1}]`, `[{"events":1,"seq_delta":1}]`, 1),
		"source not elided":    strings.Replace(good, `"go_type":"int"},"snapshot"`, `"go_type":"int","source":"agent-runtime"},"snapshot"`, 1),
		"whitespace":           strings.Replace(good, `"case":"c",`, `"case": "c",`, 1),
		"unknown member":       strings.Replace(good, `"origin":`, `"extra":1,"origin":`, 1),
		"seq 0":                strings.Replace(good, `"seq":1,`, `"seq":0,`, 1),
		"key line before case": readerHeader + readerKeyLine,
	} {
		if _, err := ReadCorpus([]byte(bad)); err == nil {
			t.Errorf("%s: accepted", name)
		}
	}
	// With a startup failure, inputs.keys must be written.
	failed := readerHeader +
		`{"case":"c","group":"breadth","inputs":{"yaml":"a: ["},"startup_error":"boom","type":"case","why":[]}` + "\n"
	if _, err := ReadCorpus([]byte(failed)); !errors.Is(err, ErrReadLine) {
		t.Errorf("startup failure without keys: got %v", err)
	}
}

// randomRecord builds one case line and its key lines, in the case's keys order, from r.
func randomRecord(r *rand.Rand, name string) (*CaseLine, []KeyLine) {
	c := &Case{Name: name, Group: "behavior", Why: []string{"w"}}
	nkeys := 1 + r.Intn(4)
	var keys []string
	for len(keys) < nkeys {
		k := fmt.Sprintf("k%d", r.Intn(6))
		if !contains(keys, k) {
			keys = append(keys, k)
		}
	}
	if r.Intn(2) == 0 {
		sort.Strings(keys)
	}
	for _, k := range keys {
		e := KeyEntry{Key: k}
		if r.Intn(5) == 0 {
			e.Getters = []string{"Get"}
		}
		c.Keys = append(c.Keys, e)
	}
	if r.Intn(2) == 0 {
		y := "k0: 1\n"
		c.YAML = &y
	}
	cl := &CaseLine{Inputs: c}
	if r.Intn(6) == 0 {
		cl.StartupError = strp("boom")
		return cl, nil
	}
	cl.Origin = strp("datadog.yaml")
	nupdates := r.Intn(4)
	for i := 0; i < nupdates; i++ {
		u := Update{Op: "set", Key: keys[r.Intn(len(keys))], Source: "agent-runtime", Value: r.Intn(3)}
		if r.Intn(3) == 0 {
			u = Update{Op: "unset", Key: u.Key, Source: "remote-config"}
		}
		if u.Op == "set" && r.Intn(4) == 0 {
			u.Value = 1.5
		}
		c.Updates = append(c.Updates, u)
		cl.Updates = append(cl.Updates, UpdateResult{SeqDelta: 1})
	}
	var lines []KeyLine
	for _, k := range keys {
		kl := KeyLine{Case: name, Key: k, Snapshot: &Setting{Source: "default", Value: json.RawMessage(`0`)},
			SnapshotRead: Read{GoType: "int", Source: []string{"default", "file"}[r.Intn(2)]}}
		if nupdates > 0 {
			for i, u := range c.Updates {
				if u.Key == k || r.Intn(4) == 0 {
					kl.Events = append(kl.Events, Event{Setting: Setting{Source: u.Source}, Seq: 1, Update: i})
				}
			}
			kl.FinalRead = &Read{GoType: "int", Source: []string{"default", "agent-runtime"}[r.Intn(2)]}
		}
		lines = append(lines, kl)
	}
	return cl, lines
}

func contains(ss []string, s string) bool {
	for _, x := range ss {
		if x == s {
			return true
		}
	}
	return false
}

// TestPropertyRoundTrip writes random records, reads them back, and checks that every value,
// including the elided members the reader reconstructs, equals what was written.
func TestPropertyRoundTrip(t *testing.T) {
	r := rand.New(rand.NewSource(1))
	for iter := 0; iter < 300; iter++ {
		var buf bytes.Buffer
		header := &HeaderLine{AgentCommit: strings.Repeat("a", 40), GOOS: "linux", GOARCH: "arm64", GoVersion: "go",
			ContainerImage: "i", InputsDigest: "sha256:" + strings.Repeat("0", 64)}
		if err := WriteLine(&buf, header); err != nil {
			t.Fatal(err)
		}
		type rec struct {
			cl   *CaseLine
			keys []KeyLine
		}
		var recs []rec
		for n := 0; n < 3; n++ {
			cl, keys := randomRecord(r, fmt.Sprintf("case-%d", n))
			if err := CheckCaseRecord(cl, keys); err != nil {
				t.Fatalf("iter %d: %v", iter, err)
			}
			recs = append(recs, rec{cl, keys})
			if err := WriteLine(&buf, cl); err != nil {
				t.Fatal(err)
			}
			sorted := append([]KeyLine(nil), keys...)
			sort.Slice(sorted, func(i, j int) bool { return sorted[i].Key < sorted[j].Key })
			for i := range sorted {
				if err := WriteLine(&buf, &sorted[i]); err != nil {
					t.Fatal(err)
				}
			}
		}
		got, err := ReadCorpus(buf.Bytes())
		if err != nil {
			t.Fatalf("iter %d: %v\n%s", iter, err, buf.String())
		}
		for i, want := range recs {
			g := got.Cases[i]
			if !reflect.DeepEqual(g.Line.Inputs, want.cl.Inputs) {
				t.Fatalf("iter %d case %d inputs:\n got %#v\nwant %#v", iter, i, g.Line.Inputs, want.cl.Inputs)
			}
			if !reflect.DeepEqual(g.Line.Updates, want.cl.Updates) {
				t.Fatalf("iter %d case %d updates differ", iter, i)
			}
			sorted := append([]KeyLine(nil), want.keys...)
			sort.Slice(sorted, func(i, j int) bool { return sorted[i].Key < sorted[j].Key })
			if !reflect.DeepEqual(g.Keys, sorted) {
				t.Fatalf("iter %d case %d key lines:\n got %#v\nwant %#v", iter, i, g.Keys, sorted)
			}
		}
	}
}
