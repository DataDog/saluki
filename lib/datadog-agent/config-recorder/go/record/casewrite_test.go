// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package record

import (
	"fmt"
	"math"
	"math/rand"
	"os"
	"path/filepath"
	"reflect"
	"sort"
	"testing"
)

// caseTexts are strings that stress YAML quoting and block scalars.
var caseTexts = []string{
	"", " ", "a", "yes", "no", "null", "~", "1", "1.5", "-0", "0x10", "true", "#x", "a: b", "- a", "'q'", `"d"`,
	"\t", "tab\tin", "é ü 日本", "\u2028", "trailing ", " leading", "a\n", "a", "a\nb", "a\nb\n", "a\n\n", "\n",
	"\n\n", "  indented\nnext\n", "x: 1\ny:\n  - z\n", "x: 1\ny:\n  - z", "line \nspace\n", "{a: 1}", "[1]", "%TAG",
	"---", "...", "a\r\nb\n", "\x7f",
}

func randText(r *rand.Rand) string {
	if r.Intn(3) == 0 {
		return caseTexts[r.Intn(len(caseTexts))] + caseTexts[r.Intn(len(caseTexts))]
	}
	return caseTexts[r.Intn(len(caseTexts))]
}

// randTyped returns a typed value of a case.md §7 kind; depth bounds nesting.
func randTyped(r *rand.Rand, depth int) interface{} {
	n := 7
	if depth <= 0 {
		n = 5
	}
	switch r.Intn(n) {
	case 0:
		return nil
	case 1:
		return r.Intn(2) == 0
	case 2:
		return []int{0, 1, -1, 42, math.MaxInt64, math.MinInt64, r.Int()}[r.Intn(7)]
	case 3:
		return []float64{0.5, -1.25, 2.0, 1e21, 1e-7, -0.0, 3.0e300, r.NormFloat64()}[r.Intn(8)]
	case 4:
		return randText(r)
	case 5:
		s := []interface{}{}
		for i := r.Intn(3); i > 0; i-- {
			s = append(s, randTyped(r, depth-1))
		}
		return s
	default:
		m := map[string]interface{}{}
		for i := r.Intn(3); i > 0; i-- {
			m[randText(r)] = randTyped(r, depth-1)
		}
		return m
	}
}

func randCase(r *rand.Rand, i int) *Case {
	groups := []string{"baseline", "breadth", "depth", "unsupported", "excluded", "unknown", "behavior"}
	sources := make([]string, 0, len(updateSources))
	for s := range updateSources {
		sources = append(sources, s)
	}
	sort.Strings(sources)
	c := &Case{Name: fmt.Sprintf("case-%d", i), Group: Group(groups[r.Intn(len(groups))]), Why: []string{}}
	for n := r.Intn(3); n > 0 || (c.Group == "behavior" && len(c.Why) == 0); n-- {
		c.Why = append(c.Why, fmt.Sprintf("why-%d", r.Intn(5)))
	}
	if r.Intn(3) > 0 {
		c.Env = map[string]string{}
		for n := r.Intn(4); n > 0; n-- {
			c.Env[fmt.Sprintf("DD_E%d", r.Intn(10))] = randText(r)
		}
	}
	if r.Intn(2) == 0 {
		s := randText(r)
		c.YAML = &s
	}
	if r.Intn(2) == 0 {
		s := randText(r)
		c.FleetPolicy = &s
	}
	seen := map[string]bool{}
	for n := r.Intn(3); n > 0; n-- {
		k := fmt.Sprintf("cli.k%d", r.Intn(10))
		if !seen[k] {
			seen[k] = true
			c.CLI = append(c.CLI, CLIOverride{Key: k, Value: randTyped(r, 2)})
		}
	}
	for n := r.Intn(4); n > 0; n-- {
		u := Update{Op: "set", Key: fmt.Sprintf("u.k%d", r.Intn(4)), Source: sources[r.Intn(len(sources))]}
		if r.Intn(3) == 0 {
			u.Op = "unset"
		} else {
			u.Value = randTyped(r, 2)
		}
		c.Updates = append(c.Updates, u)
	}
	seen = map[string]bool{}
	for n := 1 + r.Intn(4); n > 0; n-- {
		k := KeyEntry{Key: fmt.Sprintf("k%d.x_%d", r.Intn(10), r.Intn(3))}
		if seen[k.Key] {
			continue
		}
		seen[k.Key] = true
		if r.Intn(3) == 0 {
			perm := r.Perm(len(GetterNames))
			for _, p := range perm[:1+r.Intn(3)] {
				k.Getters = append(k.Getters, GetterNames[p])
			}
		}
		c.Keys = append(c.Keys, k)
	}
	return c
}

func roundTrip(t *testing.T, dir string, c *Case) {
	t.Helper()
	path := filepath.Join(dir, c.Name+".yaml")
	if err := WriteCaseFile(path, c); err != nil {
		t.Fatalf("write %s: %v", c.Name, err)
	}
	back, err := ParseCaseFile(path)
	if err != nil {
		data, _ := os.ReadFile(path)
		t.Fatalf("parse %s: %v\n%s", c.Name, err, data)
	}
	if !reflect.DeepEqual(back, c) {
		data, _ := os.ReadFile(path)
		t.Fatalf("%s does not round-trip:\n got %#v\nwant %#v\n%s", c.Name, back, c, data)
	}
	// reflect.DeepEqual treats -0.0 and 0.0 as equal; check float sign bits too.
	if caseSignMismatch(back, c) {
		data, _ := os.ReadFile(path)
		t.Fatalf("%s round-trips with a different float sign:\n got %#v\nwant %#v\n%s", c.Name, back, c, data)
	}
}

func TestPropertyCaseFileRoundTrip(t *testing.T) {
	dir := t.TempDir()
	for seed := int64(0); seed < 500; seed++ {
		r := rand.New(rand.NewSource(seed))
		roundTrip(t, dir, randCase(r, int(seed)))
	}
}

// TestHandWrittenCasesRoundTrip parses, writes and parses each hand-written case. The recorder's
// build container mounts them at /cases; a checkout has them beside the Go sources.
func TestHandWrittenCasesRoundTrip(t *testing.T) {
	var files []string
	for _, dir := range []string{"/cases", filepath.Join("..", "..", "cases")} {
		if files, _ = filepath.Glob(filepath.Join(dir, "*.yaml")); len(files) > 0 {
			break
		}
	}
	if len(files) == 0 {
		t.Fatal("no hand-written cases found")
	}
	dir := t.TempDir()
	for _, f := range files {
		c, err := ParseCaseFile(f)
		if err != nil {
			t.Fatal(err)
		}
		roundTrip(t, dir, c)
	}
}

func TestWriteCaseFileRejects(t *testing.T) {
	dir := t.TempDir()
	c := &Case{Name: "a", Group: "breadth", Keys: []KeyEntry{{Key: "k"}}}
	if err := WriteCaseFile(filepath.Join(dir, "a.yaml"), c); err == nil {
		t.Error("a nil Why reads back as empty, so it must be rejected")
	}
	c.Why = []string{}
	if err := WriteCaseFile(filepath.Join(dir, "b.yaml"), c); err == nil {
		t.Error("a file stem other than the name must be rejected")
	}
	c.CLI = []CLIOverride{{Key: "x", Value: math.NaN()}}
	if err := WriteCaseFile(filepath.Join(dir, "a.yaml"), c); err == nil {
		t.Error("a non-finite float must be rejected")
	}
}
