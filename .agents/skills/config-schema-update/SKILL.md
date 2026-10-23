---
name: config-schema-update
description: Update the vendored configuration schema
disable-model-invocation: true
user-invocable: true
---
Read `docs/development/configuration-system.md` in the Saluki checkout if you have not already.

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

Present the keys, changes, effect on users, evidence, and proposal. Ask one question per group.
Wait for approval before finalizing the classification.

Use the overlay's existing meanings and exclusion-note format, described under "Support
classifications" in `docs/development/configuration-system.md`.

Get approval before filing an issue, then verify it and link it from the overlay.

## Apply the decisions

Apply one approved decision at a time. Replace its provisional choices and remove its review
markers. Regenerate and check the build and relevant tests before reviewing the next group.

- Update `schema_overlay.yaml` and hand-maintained registry entries together.
  Follow the workflows in `docs/development/configuration-system.md` for model and translation
  changes.
- Reconcile changed schema types with deserialization, translation, and consumers. Do not weaken the
  schema to make old code compile.
- Check that new schema defaults do not replace runtime-derived values with placeholders.

## Verify the update

- Check that the generated model, classifier, registry, and documentation agree.
- Account for every changed setting and local edit. Confirm removals were deliberate and no review
  markers or unapproved decisions remain.
- Summarize the revision, approved changes, issues, and validation for human review before committing.
