# DogStatsD Doc Guide

This file describes the structure and maintenance rules for
`docs/agent-data-plane/configuration/dogstatsd.md`. Read it before making any edits.

## Document Purpose

Customer-facing. Audience: operators who have enabled ADP and want to know whether their DogStatsD
configuration will work or why something is behaving unexpectedly. Limit team tracking to GitHub
issue numbers — no priorities, project status, or internal discussion.

## Section Structure

Each section follows this pattern:

1. A short introductory sentence or two explaining what the section covers.
2. A table for quick scanning.
3. Optional `### \`key_name\`` sub-sections for keys that need prose explanation.

Add a sub-section only when a one-liner is insufficient — for example, when the behavior difference
has a non-obvious cause, when the customer needs to take a specific action, or when the divergence
involves a unit or semantic change.

## Section Anchors

Each section is preceded by an HTML comment anchor that gives the skill an unambiguous location
target.

| Anchor | Section |
| --- | --- |
| `<!-- section:unsupported-in-progress -->` | Unsupported Settings -- being worked on |
| `<!-- section:unsupported-not-planned -->` | Unsupported Settings -- not planned |
| `<!-- section:behavioral-differences -->` | Behavioral Differences |
| `<!-- section:compatibility-unknown -->` | Compatibility Unknown |
| `<!-- section:adp-only -->` | ADP-Only Settings |
| `<!-- section:reference -->` | Implemented Settings |

## Common Columns

These apply to every table unless a schema below says otherwise:
- **Config Key**: backtick-quoted key name
- **Description**: from `known-configs.json` `description` field, max 50 chars

## Table Schemas

### Unsupported Settings -- being worked on

Filter: `feature_state=MISSING`, `action=IMPLEMENT`, open GitHub issue.

| Config Key | Description | Issue |
| --- | --- | --- |

- **Issue**: reference-style link, e.g. `[#1331]`

### Unsupported Settings -- not planned

Filter: `feature_state=MISSING` or `NOT_APPLICABLE`, `action=NONE`. List only keys a customer might
plausibly expect to work.

| Config Key | Description | Reason |
| --- | --- | --- |

- **Reason**: one short customer-facing phrase, e.g. "Windows only", "Go runtime specific", "handled
  by core agent"

### Behavioral Differences

Filter: `feature_state=DIVERGENT`, `action=DOCUMENT` or `DOCUMENTED`; also `action=INVESTIGATE`
where divergence is confirmed.

| Config Key | Description | Agent Behavior | ADP Behavior |
| --- | --- | --- | --- |

- **Agent Behavior** / **ADP Behavior**: one short phrase each. If ADP uses a different key name,
  note it in the sub-section, not this column.

### Compatibility Unknown

Filter: `feature_state=UNKNOWN` and `action=INVESTIGATE`, or `MISSING`/`DIVERGENT` with
`action=INVESTIGATE` where behavior is unconfirmed.

| Config Key | Description |
| --- | --- |

### ADP-Only Settings

Filter: `feature_state=ADP_ONLY`.

| Config Key | Description | Default |
| --- | --- | --- |

- **Default**: the default value, if known and useful

### Implemented Settings

Filter: `feature_state=PARITY`.

| Config Key | Description |
| --- | --- |

## Mapping: known-configs.json -> Doc Section

| feature_state | action | Section anchor |
| --- | --- | --- |
| ADP_ONLY | DOCUMENT | section:adp-only |
| ADP_ONLY | DOCUMENTED | section:adp-only |
| ADP_ONLY | IMPLEMENT | section:adp-only |
| ADP_ONLY | INVESTIGATE | section:adp-only |
| ADP_ONLY | NONE | section:adp-only |
| DIVERGENT | DOCUMENT | section:behavioral-differences |
| DIVERGENT | DOCUMENTED | section:behavioral-differences |
| DIVERGENT | IMPLEMENT | section:behavioral-differences |
| DIVERGENT | INVESTIGATE | section:compatibility-unknown |
| DIVERGENT | NONE | section:behavioral-differences |
| MISSING | DOCUMENT | Error: reclassify as DIVERGENT, NOT_APPLICABLE, or ADP_ONLY |
| MISSING | DOCUMENTED | Error: reclassify as DIVERGENT, NOT_APPLICABLE, or ADP_ONLY |
| MISSING | IMPLEMENT | section:unsupported-in-progress |
| MISSING | INVESTIGATE | section:compatibility-unknown |
| MISSING | NONE | section:unsupported-not-planned |
| NOT_APPLICABLE | DOCUMENT | section:behavioral-differences |
| NOT_APPLICABLE | DOCUMENTED | section:behavioral-differences |
| NOT_APPLICABLE | IMPLEMENT | section:unsupported-in-progress |
| NOT_APPLICABLE | INVESTIGATE | section:compatibility-unknown |
| NOT_APPLICABLE | NONE | section:unsupported-not-planned |
| PARITY | DOCUMENT | section:reference |
| PARITY | DOCUMENTED | section:reference |
| PARITY | IMPLEMENT | section:reference |
| PARITY | INVESTIGATE | section:reference |
| PARITY | NONE | section:reference |
| UNKNOWN | DOCUMENT | section:compatibility-unknown |
| UNKNOWN | DOCUMENTED | section:compatibility-unknown |
| UNKNOWN | IMPLEMENT | section:compatibility-unknown |
| UNKNOWN | INVESTIGATE | section:compatibility-unknown |
| UNKNOWN | NONE | section:compatibility-unknown |

## Link Block

Define all GitHub issue URLs as Markdown reference links at the very bottom of the doc, after all
content. Use reference-style links everywhere in the document body.

```markdown
[#1331]: https://github.com/DataDog/saluki/issues/1331
[#1332]: https://github.com/DataDog/saluki/issues/1332
```

Keep the block sorted numerically by issue number.

## Preservation Rules

1. **Table rows** are data. Add rows for new keys, update them when `feature_state` or `action`
   changes, remove rows only when a key is removed from `known-configs.json`.
2. **Sub-section prose** is human-authored narrative. Preserve it unless the key's `feature_state`
   or `action` changed in a way that makes it factually wrong. Do not rewrite to match sub-agent
   wording.
3. **Section intros and headings** are human-authored. Never modify them.
4. **Issue links** should reflect the current open/closed state. A closed issue may warrant moving a
   key from "being worked on" to "Implemented", but confirm via `gh issue view` first.
