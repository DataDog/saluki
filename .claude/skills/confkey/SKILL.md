---
name: confkey
description: >
  Audit configuration keys (ConfKeys) that are used in agent-data-plane and in the Datadog Agent
  looking for differences that should be either documented or fixed. Maintains files at:
  - docs/agent-data-plane/configuration/dogstatsd.md
  - docs/agent-data-plane/configuration/dogstatsd/
  Available in multiple modes. See .claude/skills/confkey/README.md
  Note, this skill is currently specialized for DogStatsD configuration, but may be more general
  in the future.
argument-hint: >
  [help|audit-all|audit-one|create-issue|complete-issue|freeform] <details>
  see .claude/skills/confkey/README.md
disable-model-invocation: true
allowed-tools: Read, Write, Edit, Grep, Glob, LS, Bash, Agent, Task, AskUserQuestion
---
# /confkey

## Shared Setup: Path Resolution and Git Check

Resolve three repo paths. `{{saluki}}` is this repo's root. For `{{datadog-agent}}` and
`{{documentation}}`, check `{{saluki}}/../<name>` then `~/dd/<name>`. If not found, ask the user for
a custom path. If still unavailable, report which is missing and stop.

Show a table with: each repo's resolved path, HEAD commit (message + branch), and dirty status. Use
AskUserQuestion to confirm before proceeding.

`{{config_docs}}` = `{{saluki}}/docs/agent-data-plane/configuration/dogstatsd` -- the directory that
holds the data files maintained by this skill. The documentation page lives one level up at
`{{saluki}}/docs/agent-data-plane/configuration/dogstatsd.md`.

## Shared Definitions

- **ADP** (Agent Data Plane): The `agent-data-plane` binary and its components.
- **RefImpl** (Reference Implementation): The DogStatsD implementation in `datadog-agent`.
- **AdpImpl** (ADP Implementation): The DogStatsD implementation in ADP.
- **ConfKey** (Configuration Key): A configuration key used by ADP, the Agent, or both.
  - The primary Agent index is `{{datadog-agent}}/pkg/config/common_settings.go`, but keys also
    appear throughout `{{datadog-agent}}/comp/dogstatsd/` and elsewhere
  - ADP has no central registry. DogStatsD keys live primarily in
    `{{saluki}}/lib/saluki-components/src/sources/dogstatsd/mod.rs` (serde struct); broader ADP keys
    are in `{{saluki}}/bin/agent-data-plane/src/config.rs`

### FeatureState

Two orthogonal enums describe every ConfKey.

**`FEATURE_STATE`** -- implementation reality, machine-derivable from code analysis:

- **`PARITY`**: Both RefImpl and AdpImpl have it; behavior and effective defaults match.
- **`DIVERGENT`**: Both have it; behavior or defaults differ (intentionally or not).
- **`MISSING`**: RefImpl has it; AdpImpl does not.
- **`ADP_ONLY`**: AdpImpl has it; RefImpl does not.
- **`NOT_APPLICABLE`**: RefImpl has it but it is architecturally outside ADP's scope (e.g.
  Go-GC-specific, handled by the core agent tagger or hostname resolver).
- **`UNKNOWN`**: Insufficient data to determine; needs investigation.

**`ACTION`** -- human decision about what to do; stable across re-runs until a human changes it:

- **`NONE`**: Nothing to do; acceptable as-is.
- **`INVESTIGATE`**: Research needed before deciding. When investigation concludes, update to the
  resulting action (`NONE`, `DOCUMENT`, or `IMPLEMENT`) and record the conclusion in `reason`.
- **`IMPLEMENT`**: Code work needed. Should have a GitHub issue. Completion is detected by the skill
  on re-run (feature_state updates to PARITY).
- **`DOCUMENT`**: Documentation work needed. No code change required.
- **`DOCUMENTED`**: Terminal state for `DOCUMENT`. The documentation has been written.

Common combinations:
- `PARITY` + `NONE` -- nominal case
- `ADP_ONLY` + `NONE` -- ADP extension, no parity needed
- `NOT_APPLICABLE` + `NONE` -- outside scope
- `DIVERGENT` + `DOCUMENT` -- intentional known difference
- `MISSING` + `IMPLEMENT` -- tracked gap
- `MISSING` or `UNKNOWN` + `INVESTIGATE` -- needs research

### Persistent Ledgers

Two files in `{{config_docs}}` track classification state across runs:

**`known-configs.json`** -- full classification records for all DogStatsD-relevant keys. Schema is
defined in `./resources/known-configs.schema.json`. Each entry has `key`, `feature_state`, `action`,
`description`, `reason`, `issue`, and `adp_key`. A key present here is **known**.

**`known-configs-not-applicable.json`** -- flat JSON array of key name strings for keys confirmed as
outside ADP's scope. Used to skip re-classifying already-dismissed keys on future runs. A key
present here is **not applicable** and should be excluded from all downstream analysis.

A key present in neither file is **unreviewed** and needs classification.

## Mode Dispatch

Extract the first word from the prompt as the mode argument. The remaining text is `<details>`.

If the mode argument is absent, unrecognized, `help`, or `--help`: read `./README.md`, print its
contents verbatim, and stop.

Otherwise dispatch to the mode file, passing `<details>` as context. For `freeform`, there is no
separate mode file — follow the **Freeform Mode** instructions below.

| Argument | Mode file | Mode description |
| --- | --- | --- |
| `audit-all` | `./modes/audit-all.md` | Deep analysis of both codebases casting a wide net for ConfKeys |
| `audit-one` | `./modes/audit-one.md` | Deep analysis of a single, given ConfKey |
| `create-issue` | `./modes/create-issue.md` | Help the user write the text of a GitHub issue for one or more ConfKeys |
| `complete-issue` | `./modes/complete-issue.md` | Help the user update the documentation when finishing work on an open issue |
| `freeform` | (see **Freeform Mode** below) | Bring full skill context online and execute user-directed work |

## Freeform Mode

`freeform` has no dedicated mode file. The goal is to bring the agent's full comprehension of the
ConfKey system online so the user can direct arbitrary work without being constrained to a
pre-defined workflow.

**Context loading sequence — complete all reads before taking any action:**

1. Read all mode files: `./modes/audit-all.md`, `./modes/audit-one.md`, `./modes/create-issue.md`,
   `./modes/complete-issue.md`
2. Read all resource files: `./resources/analyze-features.md`,
   `./resources/find-refimpl-configs.md`, `./resources/find-adpimpl-configs.md`,
   `./resources/dogstatsd-doc-guide.md`, `./resources/issue-style.md`,
   `./resources/known-configs.schema.json`
3. Read both ledger files in `{{config_docs}}`:
   - `known-configs.json` (full contents)
   - `known-configs-not-applicable.json` (full contents)
4. Read `{{saluki}}/docs/agent-data-plane/configuration/dogstatsd.md`

With this context loaded, execute the user's request from `<details>`. Draw freely on any
combination of instructions from the mode files, spawn sub-agents as described, and apply the shared
utilities below. The user is directing the workflow — your job is to bring complete comprehension of
the ConfKey system to bear on whatever they are asking.

## Shared Utilities

### Error Row Check

Before updating `{{saluki}}/docs/agent-data-plane/configuration/dogstatsd.md`, check each key's
`(feature_state, action)` pair against the mapping table in `./resources/dogstatsd-doc-guide.md`. If
any pair maps to an **Error** row, stop immediately — do not modify the doc. List the offending keys
and use AskUserQuestion to ask the user whether to reclassify them with your assistance or manually.
Proceed only when all Error rows are resolved.

### Ledger Validation

After writing to any ledger file, validate it by running this Python script with the actual path
substituted for `PATH`:

```python
import json, sys

VALID_STATES = {"PARITY", "DIVERGENT", "MISSING", "ADP_ONLY", "UNKNOWN"}
VALID_ACTIONS = {"NONE", "INVESTIGATE", "IMPLEMENT", "DOCUMENT", "DOCUMENTED"}

with open("PATH") as f:
    data = json.load(f)

errors = []
for i, entry in enumerate(data):
    k = entry.get("key", f"entry[{i}]")
    for field in ("key", "feature_state", "action", "description", "reason"):
        if field not in entry:
            errors.append(f"{k}: missing required field '{field}'")
    if entry.get("feature_state") not in VALID_STATES:
        errors.append(f"{k}: invalid feature_state '{entry.get('feature_state')}'")
    if entry.get("action") not in VALID_ACTIONS:
        errors.append(f"{k}: invalid action '{entry.get('action')}'")
    if len(entry.get("description", "")) > 50:
        errors.append(f"{k}: description too long ({len(entry['description'])} chars, max 50)")
    issue = entry.get("issue")
    if issue is not None and not (isinstance(issue, str) and issue.startswith("#") and issue[1:].isdigit()):
        errors.append(f"{k}: invalid issue format '{issue}' (expected #NNN or null)")

keys = [e["key"] for e in data]
if keys != sorted(keys):
    errors.append("Array not sorted alphabetically by key")

if errors:
    for e in errors:
        print(f"ERROR: {e}", file=sys.stderr)
    sys.exit(1)

print(f"OK: {len(data)} entries, all valid")
```

Run as: `python3 validate.py` after writing the script to a temp file, or inline via
`python3 -c "$(cat)"`. If the script exits non-zero, fix the reported errors before proceeding. The
not-applicable ledger (`known-configs-not-applicable.json`) only needs `json.load` + sort-order
checks, since it is a flat array of strings.

## Anti-Patterns

**Sub-agent contamination**: Never pass a key's existing ledger entry to a clean-room sub-agent.
Sub-agents running `analyze-features.md` must receive only the key name and repo paths. Passing
prior classifications biases the result and defeats the clean-room purpose.

**Skipping AskUserQuestion gates**: Do not skip user gates when previous steps look clean. The gates
exist to catch divergence between analysis output and user expectations — including cases where the
analysis itself is wrong.

**Misrouting NOT_APPLICABLE**: Never write a `known-configs.json` entry with
`feature_state: NOT_APPLICABLE`. NOT_APPLICABLE keys live exclusively in
`known-configs-not-applicable.json`. The schema enforces this; a Ledger Validation run will catch
the mistake.

**Proceeding past an Error row**: If the Error Row Check finds a violation, stop completely. Do not
partially update the doc and handle the rest later. The check must pass in full before any doc edits
are written.

**Re-analyzing final-state keys in a full audit**: In `audit-all`, keys in a final state are already
settled — re-analyzing them wastes sub-agent context. Skip them unless the user explicitly requests
otherwise. Final states: `PARITY + NONE`, `ADP_ONLY + NONE`, `action = DOCUMENTED`.
