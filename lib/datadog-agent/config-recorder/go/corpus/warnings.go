// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package corpus

import (
	"log/slog"
	"strings"
)

// WarningFromRecord turns one captured log record into a Warning (record.md §7). It returns false
// for a record below slog.LevelWarn, since a record below WARN must not be recorded. A record at
// or above WARN is never dropped: its level is mapped to the nearest of the two levels record.md
// §7 allows, ERROR for slog.LevelError and above, WARN otherwise.
func WarningFromRecord(r slog.Record) (Warning, bool) {
	switch {
	case r.Level < slog.LevelWarn:
		return Warning{}, false
	case r.Level >= slog.LevelError:
		return Warning{Level: "ERROR", Message: r.Message}, true
	default:
		return Warning{Level: "WARN", Message: r.Message}, true
	}
}

// isNameByte reports whether b can be part of a key or env name, so a match next to it is only
// part of a longer name.
func isNameByte(b byte) bool {
	return b == '_' || b == '.' || (b >= '0' && b <= '9') || (b >= 'a' && b <= 'z') || (b >= 'A' && b <= 'Z')
}

// mentions reports whether s contains name with no name byte immediately before or after it.
// Every occurrence is tried, since the first may be part of a longer name.
func mentions(s, name string) bool {
	if name == "" {
		return false
	}
	for from := 0; ; {
		i := strings.Index(s[from:], name)
		if i < 0 {
			return false
		}
		start := from + i
		end := start + len(name)
		if (start == 0 || !isNameByte(s[start-1])) && (end == len(s) || !isNameByte(s[end])) {
			return true
		}
		from = start + 1
	}
}

// warningNames returns the names a warning must mention to be kept: keys lowercased, env names as
// given.
func warningNames(keys, envNames []string) []string {
	names := make([]string, 0, len(keys)+len(envNames))
	for _, k := range keys {
		names = append(names, strings.ToLower(k))
	}
	return append(names, envNames...)
}

// CaseWarningNames returns the names construction and update warnings are filtered by: every
// recorded key, update key and cli key, and every env name.
func CaseWarningNames(c *Case) []string {
	var keys, env []string
	for _, k := range c.Keys {
		keys = append(keys, k.Key)
	}
	for _, u := range c.Updates {
		keys = append(keys, u.Key)
	}
	for _, o := range c.CLI {
		keys = append(keys, o.Key)
	}
	for name := range c.Env {
		env = append(env, name)
	}
	return warningNames(keys, env)
}

// FilterWarnings keeps, in emission order, the warnings whose message mentions any of names.
func FilterWarnings(ws []Warning, names []string) []Warning {
	var out []Warning
	for _, w := range ws {
		for _, n := range names {
			if mentions(w.Message, n) {
				out = append(out, w)
				break
			}
		}
	}
	return out
}
