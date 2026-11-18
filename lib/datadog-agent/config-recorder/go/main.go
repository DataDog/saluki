// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

// Command config-recorder builds the Agent's configuration from one case file,
// and writes what the Agent streamed and returned from its getters.
//
// Subcommands:
//
//	run-case  runs one case in this process and writes an internal result file
//	drive     runs a baseline and every case in one or more directories, each in its own process
//	generate  writes the generated case files
//	probe     reports getter selection over every schema leaf
package main

import (
	"errors"
	"fmt"
	"os"
)

// errUsage marks a command-line error; it exits 2.
var errUsage = errors.New("usage")

func main() {
	if len(os.Args) < 2 {
		fmt.Fprintln(os.Stderr, "usage: config-recorder run-case|drive|generate|probe [flags]")
		os.Exit(2)
	}
	var err error
	switch os.Args[1] {
	case "run-case":
		err = runCaseMain(os.Args[2:])
	case "drive":
		err = driveMain(os.Args[2:])
	case "generate":
		err = generateMain(os.Args[2:])
	case "probe":
		err = probeMain(os.Args[2:])
	default:
		err = fmt.Errorf("%w: unknown subcommand %q", errUsage, os.Args[1])
	}
	if err != nil {
		fmt.Fprintln(os.Stderr, "config-recorder:", err)
		if errors.Is(err, errUsage) {
			os.Exit(2)
		}
		os.Exit(1)
	}
}
