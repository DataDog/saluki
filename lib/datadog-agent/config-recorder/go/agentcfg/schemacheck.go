// Unless explicitly stated otherwise all files in this repository are licensed
// under the Apache License Version 2.0.
// This product includes software developed at Datadog (https://www.datadoghq.com/).
// Copyright 2026-present Datadog, Inc.

package agentcfg

import (
	"strings"

	"github.com/DataDog/datadog-agent/pkg/config/model"

	"github.com/DataDog/datadog-agent/cmd/config-recorder/schema"
)

// CheckSchemaKeys checks that the schema's leaves are exactly the Agent's own key set for the
// config cfg (getter-map.md §1). Call it right after the first snapshot and before any getter
// runs (record.md §4.1): AllKeysLowercased is not itself a getter and must not join unknown keys
// to the key set, but a getter called first would, making a later mismatch ambiguous.
//
// AllKeysLowercased lowercases every key (e.g. a schema key named with an acronym like `GUI_host`
// comes back as `gui_host`), so the schema side is lowercased the same way before comparing.
func CheckSchemaKeys(s schema.Schema, cfg model.Reader) error {
	leaves := s.Leaves()
	lowered := make([]string, len(leaves))
	for i, k := range leaves {
		lowered[i] = strings.ToLower(k)
	}
	diff := DiffKeys(lowered, cfg.AllKeysLowercased())
	if !diff.Empty() {
		return diff
	}
	return nil
}
