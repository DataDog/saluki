# Mode: Complete Issue

A developer has been working on a GitHub issue and is preparing to open or amend a PR. This mode
helps them update the ledger and documentation to reflect the work done.

## Step 1: Parse Input

Extract the issue number from `<details>`. If no issue number is present, ask for it before
proceeding. The remaining text is optional context.

## Step 2: Read the Issue

Fetch the issue body:

```bash
gh issue view <number> --repo DataDog/saluki
```

Treat the issue body as informative background — it describes the original intent but may not
reflect decisions made during implementation. Do not use it as the source of truth for what changed.

## Step 3: Read the Diff

Diff the current branch against main to see what was actually implemented:

```bash
git diff main
```

This is the source of truth. Use it together with the issue body to understand:
- Which ConfKeys were touched
- What behavior was added, changed, or deliberately left out
- Any decisions that diverged from the original issue description

## Step 4: Identify Affected ConfKeys

Find which ledger entries relate to this work:

- Check `known-configs.json` for entries whose `issue` field references this issue number
- Cross-reference key names and code paths mentioned in the issue body and diff
- If the diff touches files that implement config behavior not yet in the ledger, note those too

## Step 5: Offer Implementation Audit

Ask the developer whether they want to audit their work against RefImpl using
`../resources/analyze-features.md` for each affected key.

- If **yes**: run the sub-agent for each affected key (or as a batch), using the same clean-room
  approach as `audit-one`. Use the results to inform the proposed classifications in Step 6.
- If **no**: proceed to Step 6 using the diff and issue body alone to determine proposed state.

## Step 6: Propose Ledger and Doc Updates

Before generating the proposed workup, run the **Error Row Check** defined in
`SKILL.md ## Shared Utilities` on each proposed `(feature_state, action)` pair. Resolve any Error
rows before presenting the review to the user. This avoids wasting a review session on changes that
cannot be written.

Based on the diff, issue body, and any audit results, determine the correct final state for each
affected key. Do not assume `PARITY` + `NONE` — the resolution may be any valid combination:

- `PARITY` + `NONE` — fully implemented as specified
- `DIVERGENT` + `DOCUMENT` — intentional difference, needs documenting
- `NOT_APPLICABLE` + `NONE` — determined during implementation to be out of scope
- `MISSING` + `IMPLEMENT` — partially addressed; remaining work still tracked
- Any other valid `(FEATURE_STATE, ACTION)` pair from the definitions in SKILL.md

Present all affected keys at once in a single workup. For each key show:
- Current ledger entry (before)
- Proposed ledger entry (after): updated `feature_state`, `action`, `reason`, and any other changed
  fields
- Proposed doc change: updated Features table row and any Discussion section additions or edits

Ask the developer to review, confirm, or correct any of the proposed values before writing. Enter an
interactive back-and-forth until they are satisfied.

## Step 7: Apply Confirmed Changes

Write all confirmed changes:

- Update entries in `{{config_docs}}/known-configs.json`. Keep array sorted alphabetically by `key`.
- Retain the `issue` field after work is complete. It preserves the historical link back to the
  GitHub issue that drove the change. Do not clear it to `null`.
- If any key is reclassified as not-applicable: move it from `known-configs.json` to
  `known-configs-not-applicable.json`. Keep both arrays sorted alphabetically.
- Update `{{saluki}}/docs/agent-data-plane/configuration/dogstatsd.md`:
  - Update or add Features table rows. Preserve existing wording when semantically equivalent.
  - Update or add `### key_name` discussion sections where the analysis produced noteworthy detail.
    Do not remove an existing discussion section without asking.
  - Update the `Last updated` date at the top.
