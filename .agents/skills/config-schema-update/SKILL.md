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

## Update the Agent config corpus

Regenerating the corpus is part of a schema update, not a separate task. See
`lib/datadog-agent/config-recorder/README.md` for the recorder, cases, and checks in full; this
gives only the steps and what to look for.

1. Follow the README's pin-bump steps: update the reader's lists, bump
   `REVIEWED_AT_AGENT_COMMIT`, then run `make build-agent-config-corpus`.
   `corpus_pin_matches_vendored_schema` fails until this is done. A new pin downloads Go modules
   the cache lacks; if the host's `GOPROXY` is unreachable, set a reachable one for the run (for
   example `GOPROXY=https://proxy.golang.org,direct`).
2. Review the corpus diff. A changed record usually means changed Agent behavior; summarize
   changes for the user by key and by behavior. Two diffs are expected and are not behavior
   changes:
   - Depth representatives move: each depth class records its byte-first key, so a new key that
     sorts first in its class moves that class's depth rows to the new key.
   - Batches are renamed: a section that crosses the 40-key split renames its batches.
3. Check coverage. A new modeled or unsupported key needs a breadth record; a new behavior needs
   a case (the README's "Adding a case"). A changed `startup-failed` case line, or a changed
   startup error message, means the Agent now fails differently at startup; report it.
4. Check the Agent's load-time writes. Diff `pkg/config/setup/` and `comp/core/config/setup.go`
   between the old pin and the new one. A new or changed write to a setting changes no record
   unless some case triggers it, so add one. Splitting a batch checks sources, not values: a
   generated batch that sets both a write's trigger and its target from the same source can
   record the derived value as the target's own, so give that target its own case.
5. Run the replay tests: `cargo nextest run --lib --bins -p agent-data-plane-config-system`. For
   each change in `known-results.txt`:
   - A new divergence: fix it in ADP's type or translation layer, or bless it into the file. Give
     it a divergence type, an existing one where it fits or a new `type` line. Get the user's
     approval before adding a divergence.
   - A fixed divergence: the check reports the line; bless removes it.
   - A new derived value in ADP or the Agent: add a row to `DERIVATIONS` in `derived.rs`, or a
     reason to `NOT_REPLAYED`.

   Bless with:

   ```sh
   ADP_CORPUS_REPLAY_BLESS=1 cargo nextest run --lib -p agent-data-plane-config-system \
     corpus_replay_results_equal_the_known_results_file
   ```

## Review support with the user

Group related settings; split them when support differs by pipeline or signal. Compare pinned
upstream code with ADP consumers and tests. Check existing issues and pull requests.

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
markers. Regenerate and check the build and relevant tests before reviewing the next group.

- Update `schema_overlay.yaml` and hand-maintained registry entries together.
  Follow the config-system workflows for model and translation changes.
- Reconcile changed schema types with deserialization, translation, and consumers. Do not weaken the
  schema to make old code compile.
- Check that new schema defaults do not replace runtime-derived values with placeholders.

## Verify the update

- Check that the generated model, classifier, registry, and documentation agree.
- Account for every changed setting and local edit. Confirm removals were deliberate and no review
  markers or unapproved decisions remain.
- Confirm the corpus pin matches `_version.txt`, the corpus and replay tests pass, and every
  change to `known-results.txt` is in the summary for the user.
- Summarize the revision, approved changes, issues, and validation for human review before
  committing.
