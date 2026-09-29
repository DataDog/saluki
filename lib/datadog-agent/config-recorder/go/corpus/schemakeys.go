// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package corpus

import (
	"fmt"
	"sort"
	"strings"
)

// maxSchemaKeyDiffSample is how many keys from each side a mismatch report shows.
const maxSchemaKeyDiffSample = 20

// SchemaKeyDiff is how a schema's leaf keys differ from the Agent's own key set for a config
// built with no inputs (getter-map.md §1: "the schema file ... must describe exactly the Agent
// binary's key set"). Both sides are sorted and deduplicated by DiffSchemaKeys.
type SchemaKeyDiff struct {
	// OnlySchema are keys the schema has that the Agent's key set does not.
	OnlySchema []string
	// OnlyAgent are keys the Agent's key set has that the schema does not.
	OnlyAgent []string
}

// Empty reports whether the two key sets are equal.
func (d SchemaKeyDiff) Empty() bool {
	return len(d.OnlySchema) == 0 && len(d.OnlyAgent) == 0
}

// Error formats the diff as a harness failure: the count on each side, then up to
// maxSchemaKeyDiffSample keys from each side.
func (d SchemaKeyDiff) Error() string {
	var b strings.Builder
	fmt.Fprintf(&b, "schema leaves do not match the Agent's key set (%d only in the schema, %d only in the Agent)",
		len(d.OnlySchema), len(d.OnlyAgent))
	writeSide(&b, "only in the schema", d.OnlySchema)
	writeSide(&b, "only in the Agent", d.OnlyAgent)
	return b.String()
}

func writeSide(b *strings.Builder, label string, keys []string) {
	if len(keys) == 0 {
		return
	}
	shown := keys
	more := 0
	if len(shown) > maxSchemaKeyDiffSample {
		shown = shown[:maxSchemaKeyDiffSample]
		more = len(keys) - maxSchemaKeyDiffSample
	}
	fmt.Fprintf(b, "\n  %s: %s", label, strings.Join(shown, ", "))
	if more > 0 {
		fmt.Fprintf(b, " (and %d more)", more)
	}
}

// DiffSchemaKeys compares a schema's leaf keys to the Agent's own key set (AllKeysLowercased of a
// config built with no inputs). Neither input needs to be sorted or deduplicated.
func DiffSchemaKeys(schemaLeaves, agentKeys []string) SchemaKeyDiff {
	schema := make(map[string]bool, len(schemaLeaves))
	for _, k := range schemaLeaves {
		schema[k] = true
	}
	agent := make(map[string]bool, len(agentKeys))
	for _, k := range agentKeys {
		agent[k] = true
	}
	var d SchemaKeyDiff
	for k := range schema {
		if !agent[k] {
			d.OnlySchema = append(d.OnlySchema, k)
		}
	}
	for k := range agent {
		if !schema[k] {
			d.OnlyAgent = append(d.OnlyAgent, k)
		}
	}
	sort.Strings(d.OnlySchema)
	sort.Strings(d.OnlyAgent)
	return d
}
