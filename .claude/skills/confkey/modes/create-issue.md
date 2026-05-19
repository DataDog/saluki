# Mode: Create Issue

Draft and file GitHub issues in `DataDog/saluki` for ConfKeys that need attention. Reads style rules
and label conventions from `../resources/issue-style.md`.

## Step 1: Parse Input

Extract key names from `<details>`. Two cases:

- **Keys provided**: proceed with those keys.
- **No keys provided**: scan `{{config_docs}}/known-configs.json` for keys that need issues —
  entries where `action` is `IMPLEMENT`, `INVESTIGATE`, or `DOCUMENT` and the `issue` field is null
  or empty. Present the list to the user and ask which keys they want to work on.

## Step 2: Duplicate Search

For each key, search for pre-existing open issues to avoid duplicates:

```bash
gh issue list --repo DataDog/saluki --state open --search "<key_name>"
```

Also search by likely title keywords derived from the key name and ledger description. Report any
potential duplicates to the user before proceeding. If a clear duplicate exists, ask whether to link
the existing issue to the ledger instead of drafting a new one (skip to Step 6).

## Step 3: Propose Grouping

If working with multiple keys, propose a grouping — which keys could share a single issue vs which
warrant separate issues. Base the suggestion on related functionality, shared codebase area, or
shared action type. Present the proposed grouping and ask the user to confirm or adjust. The user
has final say.

## Step 4: Draft Issues

For each issue (or group), draft a well-written issue body. Read `../resources/issue-style.md` for
title format, tone rules, label selection, and the `gh` command template.

For each draft include:
- **Title**: grammatical sentence ending with a period, backticks around key names
- **Body**: problem statement, what needs to happen, relevant code permalinks
- **Labels**: selected from the label reference in `../resources/issue-style.md`

Draw on the ledger entry (`description`, `reason`, `feature_state`, `action`) and any code locations
found during prior analysis. If the ledger entry lacks enough detail to write a good issue body,
flag it and ask the user to fill in the gaps.

When working with multiple issues, draft all of them before moving to review.

## Step 5: Review and Refine

Present all drafts to the user. Revise titles, body text, or labels until the user is satisfied with
each draft. Treat each issue independently; the user may approve some and continue editing others.

## Step 6: File

For each approved draft, show the final `gh` command using the template from
`../resources/issue-style.md`. Offer to run it. If the user approves, run the command and capture
the returned issue URL.

## Step 7: Update Ledger

After each issue is filed, extract the issue number from the URL and update the `issue` field in the
corresponding `known-configs.json` entries. Keep the array sorted alphabetically by `key`.

```bash
ISSUE_NUM=$(echo "$ISSUE_URL" | grep -o '[0-9]*$')
```

Record the issue as `#NNN` in the `issue` field.
