// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

// Command config-recorder builds the Agent's configuration from one case file,
// applies the case's updates, and writes what the Agent streamed and returned
// from its getters as canonical JSON lines.
package main

import (
	"fmt"
	"os"
)

func main() {
	fmt.Fprintln(os.Stderr, "config-recorder: not implemented")
	os.Exit(2)
}
