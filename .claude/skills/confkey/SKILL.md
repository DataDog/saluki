---
name: confkey
description: >
  Work on individual configuration keys (ConfKeys) used in agent-data-plane and in the Datadog
  Agent — analyzing parity, drafting GitHub issues, and recording completion of open issues.
  Note, this skill is currently specialized for DogStatsD configuration, but may be more general
  in the future.
argument-hint: >
  [help|audit|create-issue] <details>
  see .claude/skills/confkey/README.md
disable-model-invocation: true
allowed-tools: Read, Write, Edit, Grep, Glob, LS, Bash, Agent, Task, AskUserQuestion
---
# /confkey

## Source of Truth

The overlay file is the single source of truth for all ConfKey classification state. Read the
`/config-management` skill
(`/Users/matt.briggs/repos/confra/.claude/skills/config-management/SKILL.md`) before starting any
work here. Locate the overlay with:

```bash
find lib -name schema_overlay.yaml
```

The overlay partitions every key in `core_schema.yaml` into `inventory` or `excluded`. Within
`inventory`, each entry is tagged with a `support` field (`full`, `partial`, `none`, `unknown`).
Pipeline attribution, descriptions, and prose documentation all live there. The generated
`dogstatsd.md` documentation is output, not source — edit the overlay, not the generated file.

## Shared Setup: Path Resolution and Git Check

Resolve two repo paths. `{{saluki}}` is this repo's root. For `{{datadog-agent}}`, check
`{{saluki}}/../datadog-agent` then `~/dd/datadog-agent`. If not found, ask the user for a custom
path. If still unavailable, report it and stop.

Show a table with: each repo's resolved path, HEAD commit (message + branch), and dirty status. Use
AskUserQuestion to confirm before proceeding.

## Shared Definitions

- **ADP** (Agent Data Plane): The `agent-data-plane` binary and its components.
- **RefImpl** (Reference Implementation): The DogStatsD implementation in `datadog-agent`.
- **AdpImpl** (ADP Implementation): The DogStatsD implementation in ADP.
- **ConfKey** (Configuration Key): A configuration key used by ADP, the Agent, or both.
  - The primary Agent index is `{{datadog-agent}}/pkg/config/common_settings.go`; keys also appear
    throughout `{{datadog-agent}}/comp/dogstatsd/` and elsewhere
  - ADP keys are registered in `schema_overlay.yaml`; Rust implementation lives across
    `lib/datadog-agent/config/` and `lib/saluki-components/`

### Overlay Classification

Code analysis determines where a key belongs in the overlay. The overlay has two sections:

**`inventory`** — every key the team has reviewed and classified, tagged with `support`:
- `support: full` — ADP reads and fully supports this key; behavior matches the Agent.
- `support: partial` — ADP reads this key but behavior diverges; use `documentation` to describe the
  divergence for operators.
- `support: none` — Key is relevant to ADP's domain but not implemented.
  - `severity` (low/medium/high) — operational impact of the gap.
  - `planned: true` + `issue` — implementation is committed and tracked.
  - `planned: false` — no current plan to implement.
- `support: unknown` — Classification not yet determined. Use when there is insufficient evidence.
  Attach an `issue` when a research task has been filed.

**`excluded`** — Key is outside ADP's domain entirely (e.g. Go-GC-specific, Windows-only, handled by
the core Agent tagger or hostname resolver). Requires a short reason string. Never classify a key as
`excluded` without explicit user confirmation.

**`saluki_keys.rs`** — ADP uses a key that has no counterpart in `core_schema.yaml`. These are
ADP-only extensions and live in `config-overlay-model/src/saluki_keys.rs`, not in the overlay.

## Mode Dispatch

Extract the first word from the prompt as the mode argument. The remaining text is `<details>`.

If the mode argument is absent, unrecognized, `help`, or `--help`: read `./README.md`, print its
contents verbatim, and stop.

Otherwise dispatch to the mode file, passing `<details>` as context. For `freeform`, there is no
separate mode file — follow the **Freeform Mode** instructions below.

| Argument       | Mode file                 | Mode description                                                                             |
|----------------|---------------------------|----------------------------------------------------------------------------------------------|
| `audit`        | `./modes/audit.md`        | Analyze one or more ConfKeys against the RefImpl to determine correct overlay classification |
| `create-issue` | `./modes/create-issue.md` | Help the user write the text of a GitHub issue for one or more ConfKeys                      |

## Anti-Patterns

**Sub-agent contamination**: Never pass a key's existing overlay entry to a clean-room sub-agent.
Sub-agents running `analyze-features.md` must receive only the key name and repo paths. Passing
prior classifications biases the result and defeats the clean-room purpose.

**Skipping AskUserQuestion gates**: Do not skip user gates when previous steps look clean. The gates
exist to catch divergence between analysis output and user expectations — including cases where the
analysis itself is wrong.

**Editing generated files**: Never edit `dogstatsd.md` or the generated config registry files
directly. Fix classification in `schema_overlay.yaml` and rebuild. See `/config-management`.

**Moving keys to `excluded` autonomously**: Never classify a key as `excluded` without explicit user
confirmation. Use `support: unknown` in `inventory` as the holding pen for uncertain keys.
