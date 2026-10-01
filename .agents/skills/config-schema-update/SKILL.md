---
name: config-schema-update
description: Update the vendored configuration schema
disable-model-invocation: true
user-invocable: true
---
Read `.agents/skills/config-system/SKILL.md` in the Saluki checkout if you have not already.

# /config-schema-update

Update the Datadog Agent schema in `lib/datadog-agent/config/schema/core/`, the overlay, and
downstream code. Adapt to the user's environment and preferences.

## Prepare the update

The schema comes from https://github.com/DataDog/datadog-agent, under `pkg/config/schema/yaml/`.
Prefer an existing checkout.

Before replacing the vendored copy, record keys changed by hand since the last update.
Ask the user how to handle local changes that upstream has not adopted.

## Compare the schemas

Default to the latest schema from datadog-agent `main`. If unsure, check with the user.

Compare the old upstream schema, the previous Saluki copy, and the new upstream schema. Review
setting leaves and their attributes, not only file diffs or key counts.

Separate these cases:

- New Datadog keys that need an ADP support decision.
- Saluki-only keys now defined upstream. These need to move to the Datadog configuration path.
- Local schema additions now adopted upstream. Usually only reporting is needed.
- Removed or renamed keys. Decide whether to retire their consumers or preserve as saluki-only.
- Changed types, defaults, environment bindings, constraints, or documentation. Trace their effects
  on parsing, translation, and runtime behavior, even when the key name is unchanged.

Record exact keys, changes, evidence, proposals, and decisions for review across sessions.
Proposals are not approvals.

## Update the vendored schema

Resolve the upstream revision to a commit SHA. Replace the snapshot with files from that commit,
not an edited working directory. Record the SHA in the vendored `_version.txt`.

Use your judgement to get the build and relevant tests passing. Use valid overlay entries, not a
placeholder classification. Mark provisional choices with `TODO: review` comments in hand-edited
sources and record them for user review. Do not weaken validation or tests.

## Review support with the user

Group related settings; split them when support differs by pipeline or signal. Compare pinned
upstream code with ADP consumers and tests. Check existing issues and pull requests.

Review the Agent's configuration loading and reading code in the same pass: diff
`pkg/config/setup/`, `comp/core/config/setup.go`, and the model, getter and environment code the
corpus lists name, between the old pin and the new one. A new or changed load-time write, getter
result, or environment binding is an upstream behavior change that no re-recording can reveal
unless a case triggers it, so bring it into the same discussion and name the case that will
trigger it. Splitting a batch checks sources, not values: a generated batch that sets both a
write's trigger and its target from the same source can record the derived value as the target's
own, so give that target its own case.

Present the keys, changes, effect on users, evidence, and proposal. Ask one question per group.
Wait for approval before finalizing the classification.

Use the overlay's existing meanings:

- `full`: ADP reads the key and matches the Agent's behavior.
- `partial`: ADP reads the key but differs in some cases. Describe those differences.
- `none`: ADP does not support the key. Choose severity based on the effect on users and state
  whether support is planned. Link an issue when work should be tracked.
- `unknown`: compatibility has not been established. Record what still needs investigation.
- `excluded`: ADP silently ignores the key and omits it from compatibility documentation. Use this
  for features ADP does not currently support, not to hide gaps that need tracking.

An investigation toward intended support is `planned: true`. A documentation-only issue does not
mean support is planned. Get approval before filing an issue, then verify it and link it from the
overlay.

Date exclusion notes and name the feature: `YYYY-MM-DD: ADP does not currently support <feature>.`
Avoid generic reasons such as "Core Agent-only" or claims that support can never be added.

## Apply the decisions

Apply one approved decision at a time. Replace its provisional choices and remove its review
markers. Run `make build-schema-overlay` and check the build and relevant tests before reviewing
the next group. An optional diagnostic corpus recording can investigate a suspected behavior
change mid-review; the recorded corpus is refreshed once, below, after the groups are applied.

- Update `schema_overlay.yaml` and hand-maintained registry entries together.
  Follow the config-system workflows for model and translation changes.
- Reconcile changed schema types with deserialization, translation, and consumers. Do not weaken the
  schema to make old code compile.
- Check that new schema defaults do not replace runtime-derived values with placeholders.

## Record the corpus

Record once, after the decisions are applied and the generated code has settled. Recording between
classification steps goes stale when a key moves between modeled, unsupported, and excluded. See
`lib/datadog-agent/config-recorder/README.md` for the recorder, cases, and checks in full; these
are the steps and what to look for.

1. Follow the README's pin-bump steps: update the reader's lists of sources, getters and groups
   against `pkg/config/model/types.go` at the new pin, and bump `REVIEWED_AT_AGENT_COMMIT`, once
   per pin. A new pin downloads Go modules the cache lacks; if the host's `GOPROXY` is unreachable,
   set a reachable one for the run (for example `GOPROXY=https://proxy.golang.org,direct`).
2. Run `make build-agent-config-corpus`. A pin bump can break the recorder's compilation or a
   getter API it calls; repair it and show the repair in the summary. Do not add compatibility
   layers for breakage that has not happened.
3. Review the corpus diff. A changed record usually means changed Agent behavior; summarize the
   changes for the user by key and by behavior. Not behavior changes:
   - A section crossing the 40-key split renames its batches and redistributes its keys; every
     check's origin is stable across those renames and splits.
   - A depth row changes only when the pinned representative's class is actually affected by the
     schema change, not because a new key sorts earlier. A pin that no longer matches the schema
     fails generation with what to update.
4. Check coverage. A new modeled or unsupported key needs a breadth record; new upstream behavior
   found in the support review needs the triggering case named there (the README's "Adding a
   case"). A changed `startup-failed` case line, or a changed startup error message, means the
   Agent now fails differently at startup; report it. The pin-match and inputs-digest checks must
   pass.
5. Run the replay tests: `cargo nextest run --lib -p agent-data-plane-config-system
   corpus_replay`. A failing check is a difference between ADP and the recorded Agent result.
   Expectations name checks by their stable origin, not by batch name or update position, so
   inserting an update of another key leaves existing identities unchanged. Updates of the same
   key are distinguished by their occurrence. Decide each difference:
   - Intentional ADP behavior: state the exact expected value and the reason in the typed
     expectation beside the replay code.
   - Known bug: record the precise current result, the desired result (normally the recorded
     Agent result), and the reason. A fix changes the current result, so the expectation is
     edited in the same change that fixes the production code.

   Classifying a difference does not need a second approval round; open support questions still go
   through the review above. Never regenerate the corpus to make an ADP-only fix pass.

## Verify the update

- Check that the generated model, classifier, registry, and documentation agree.
- Account for every changed setting and local edit. Confirm removals were deliberate and no review
  markers or unapproved decisions remain.
- Confirm the corpus pin matches `_version.txt`, the corpus checks and replay tests pass, and
  every expectation edit and behavior difference is in the summary for the user.
- Summarize the revision, approved changes, issues, and validation for human review before
  committing.
