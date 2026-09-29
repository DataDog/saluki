// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package main

import (
	"errors"
	"testing"
)

func TestParseDriveFlags(t *testing.T) {
	const commit = "281d921619d52ce7b99aef40607285992c9c2e89"
	const image = "golang@sha256:e30143be198ab04cf7ba25fba83ab3a692ca584c994aad0bf131fa0eb32dd8c1"
	const digest = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
	base := []string{"--cases", "c", "--workdir", "w", "--out", "o", "--schema", "s"}
	with := func(extra ...string) []string { return append(append([]string{}, base...), extra...) }
	full := func(extra ...string) []string {
		return with(append([]string{"--agent-commit", commit, "--container-image", image, "--inputs-digest", digest}, extra...)...)
	}

	f, err := parseDriveFlags(full())
	if err != nil {
		t.Fatalf("valid flags: %v", err)
	}
	if f.agentCommit != commit || f.containerImage != image || f.inputsDigest != digest {
		t.Fatalf("parsed %+v", f)
	}

	bad := map[string][]string{
		"missing commit":       with("--container-image", image, "--inputs-digest", digest),
		"missing image":        with("--agent-commit", commit, "--inputs-digest", digest),
		"missing digest":       with("--agent-commit", commit, "--container-image", image),
		"short commit":         with("--agent-commit", commit[:39], "--container-image", image, "--inputs-digest", digest),
		"uppercase commit":     with("--agent-commit", "281D921619D52CE7B99AEF40607285992C9C2E89", "--container-image", image, "--inputs-digest", digest),
		"image by tag":         with("--agent-commit", commit, "--container-image", "golang:1.26.7", "--inputs-digest", digest),
		"digest wrong prefix":  with("--agent-commit", commit, "--container-image", image, "--inputs-digest", "md5:"+digest[7:]),
		"digest short hex":     with("--agent-commit", commit, "--container-image", image, "--inputs-digest", "sha256:abcd"),
		"digest uppercase hex": with("--agent-commit", commit, "--container-image", image, "--inputs-digest", "sha256:"+digest[7:63]+"D"),
		"stray argument":       append(full(), "extra"),
		"missing schema": {"--cases", "c", "--workdir", "w", "--out", "o", "--agent-commit", commit, "--container-image", image,
			"--inputs-digest", digest},
	}
	for name, args := range bad {
		if _, err := parseDriveFlags(args); !errors.Is(err, errUsage) {
			t.Errorf("%s: got %v, want a usage error", name, err)
		}
	}
}

func TestParseDriveFlagsRepeatedCases(t *testing.T) {
	f, err := parseDriveFlags([]string{"--cases", "c1", "--cases", "c2", "--workdir", "w", "--out", "o", "--schema", "s",
		"--agent-commit", "281d921619d52ce7b99aef40607285992c9c2e89",
		"--container-image", "golang@sha256:e30143be198ab04cf7ba25fba83ab3a692ca584c994aad0bf131fa0eb32dd8c1",
		"--inputs-digest", "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"})
	if err != nil {
		t.Fatal(err)
	}
	if len(f.casesDirs) != 2 || f.casesDirs[0] != "c1" || f.casesDirs[1] != "c2" {
		t.Fatalf("got %v", f.casesDirs)
	}
}
