# Mode: Audit One

Deep analysis of a single ConfKey. Use this to onboard a key that is missing from the ledger, or to
re-examine a key already in the ledger when the recorded classification is untrusted.

## Step 1: Parse Input

Extract the key name(s) from `<details>`. Often the key is the first token and the remaining text is
free-form context from the user — hints to guide the analysis (e.g. suspected behavior, code
pointers, adjustments to the instructions). Do not treat hints as ground truth.

If no key name is identifiable in `<details>`, ask the user which key to analyze before proceeding.

## Step 2: Check Ledger Status

Look up the key in both ledger files in `{{config_docs}}`:

- Found in `known-configs-not-applicable.json` → **Dismissed.** Tell the user the key is already
  marked not-applicable. Ask whether to proceed with a fresh analysis anyway. If not, stop.
- Found in `known-configs.json` → **Known.** Note the current entry but do not let it influence the
  analysis in Step 3. The goal is a clean-room result to compare against.
- Found in neither → **New.** No prior classification exists.

## Step 3: Clean-Room Analysis

Run `../resources/analyze-features.md` as a sub-agent, passing it:
- The key name as a single-element batch
- `{{datadog-agent}}`, `{{saluki}}`
- A temp output path under `{{saluki}}/target/.temp/dogstatsd-audit/`

Do not pass the existing ledger entry to the sub-agent. Use hints from `<details>` to direct where
to look, but let the sub-agent form its own conclusions from the code.

The sub-agent returns a result using its own status terms. Map them to `FEATURE_STATE` as follows:

| Sub-agent status | FEATURE_STATE |
| --- | --- |
| `Implemented` | `PARITY` |
| `Divergent` | `DIVERGENT` |
| `Missing` | `MISSING` |
| `ADP Only` | `ADP_ONLY` |
| `Unsure` | `UNKNOWN` |

After mapping, consider whether `NOT_APPLICABLE` is more appropriate than the mapped value. This
applies when the key is architecturally outside ADP's scope (e.g. Go-GC-specific, Windows-only,
handled upstream by the core agent tagger or hostname resolver). `analyze-features.md` cannot make
this determination — it is a judgment call based on the analysis narrative.

From the result, determine:
- **`FEATURE_STATE`** (mapped above, adjusted for NOT_APPLICABLE if warranted)
- **`ACTION`**: one of `NONE`, `INVESTIGATE`, `IMPLEMENT`, `DOCUMENT`, `DOCUMENTED` — as defined in
  SKILL.md
- **description** and **reason** from the sub-agent's Description and Discussion fields

## Step 4: Present Results and Reconcile

### New key

Present the analysis: key name, `FEATURE_STATE`, `ACTION`, description, reason, and any relevant
code locations.

If `FEATURE_STATE` is `NOT_APPLICABLE`, propose adding the key to
`known-configs-not-applicable.json` instead of `known-configs.json`. Ask the user to confirm.

Otherwise propose a new entry for `known-configs.json`. Ask the user to confirm or adjust any fields
before writing.

### Known key (clean-room re-check)

Show the proposed new classification alongside the current ledger entry as a side-by-side
comparison. Highlight every field that differs.

If there are no differences, tell the user and stop (unless they want to continue).

If there are differences, propose the specific updates and ask the user to accept, modify, or reject
them.

### Dismissed key (re-examined by user request)

If the analysis confirms it is not applicable, recommend leaving it in
`known-configs-not-applicable.json`. If the analysis finds it is relevant after all, propose
removing it from `known-configs-not-applicable.json` and adding it to `known-configs.json`. Ask the
user to confirm.

## Step 5: Update Ledger

Apply the confirmed changes:

- New or updated entry in `known-configs.json`: keep array sorted alphabetically by `key`.
- New not-applicable entry in `known-configs-not-applicable.json`: keep array sorted alphabetically.
- If moving from not-applicable to known: remove from `known-configs-not-applicable.json` and add to
  `known-configs.json`.

## Step 6: Offer Doc Update

Ask the user whether to update `{{saluki}}/docs/agent-data-plane/configuration/dogstatsd.md` for
this key.

If yes:

1. Run the **Error Row Check** defined in `SKILL.md ## Shared Utilities` on the proposed
   `(feature_state, action)` pair. If it returns an Error, stop and report it.
2. Read the current doc. Find the existing row for this key in the Features table, if any.
3. Update or add the row. Preserve existing wording if the new analysis is semantically equivalent;
   only rewrite if the classification changed or the description is wrong.
4. If the analysis produced noteworthy detail, update or add a `### key_name` discussion section.
   Never remove an existing discussion section.
5. Update the `Last updated` date at the top.
