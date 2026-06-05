# Mode: Audit

Deep analysis of one or more ConfKeys against the RefImpl codebase. Use this to examine a key that
is missing from the overlay, to re-examine a key whose classification is untrusted, or to surface
work items (e.g. move a key from `ignored` to `investigate` and open an issue).

## Step 1: Parse Input

Extract the key name(s) from `<details>`. Often the key is the first token and the remaining text is
free-form context from the user — hints to guide the analysis (e.g. suspected behavior, code
pointers, adjustments to the instructions). Do not treat hints as ground truth.

If no key name is identifiable in `<details>`, ask the user which key(s) to analyze before
proceeding. Multiple keys are processed independently and results are presented together at Step 4.

## Step 2: Check Overlay Status

Read `schema_overlay.yaml` (locate with `find lib -name schema_overlay.yaml`). Find the key:

- Found in `ignored` → **Dismissed.** Tell the user the key is in `ignored`. Ask whether to proceed
  with a fresh analysis anyway. If not, stop.
- Found in `investigate`, `supported`, or `unsupported` → **Known.** Note the current entry but do
  not let it influence the analysis in Step 3. The goal is a clean-room result to compare against.
- Found in neither → **New.** No prior classification exists; it may also be absent from
  `core_schema.yaml` entirely (ADP-only key).

## Step 3: Clean-Room Analysis

Run `../resources/analyze-features.md` as a sub-agent, passing it:
- The key name as a single-element batch
- `{{datadog-agent}}`, `{{saluki}}`
- A temp output path under `{{saluki}}/target/.temp/dogstatsd-audit/`

Do not pass the existing overlay entry to the sub-agent. Use hints from `<details>` to direct where
to look, but let the sub-agent form its own conclusions from the code.

The sub-agent returns a `ProposedSection` for each key plus section-specific fields (`SupportLevel`,
`Severity`, `Description`, `Discussion`). These map directly to overlay sections — no translation
needed. Treat the sub-agent's `ProposedSection` as a proposal, not a final decision:

- `ignored` always requires explicit user confirmation before writing.
- `saluki_keys.rs` means the key has no Agent counterpart; flag it to the user before touching
  `saluki_keys.rs`.
- `investigate` is appropriate when the sub-agent could not determine scope or implementation
  status; confirm with the user before accepting.

Refer to `config-overlay-model/src/lib.rs` for required fields per section.

## Step 4: Present Results and Reconcile

### New key

Present the analysis: key name, `ProposedSection`, proposed fields, and relevant code locations.
Propose the specific overlay entry (or `saluki_keys.rs` addition if applicable). Ask the user to
confirm or adjust any fields before writing.

### Known key (clean-room re-check)

Show the proposed overlay entry alongside the current one as a side-by-side comparison. Highlight
every field that differs. If there are no differences, tell the user and stop (unless they want to
continue). Otherwise ask the user to accept, modify, or reject the proposed changes.

### Dismissed key (re-examined by user request)

If the analysis confirms out-of-scope, recommend leaving it in `ignored`. If the analysis finds it
is relevant after all, propose moving it to `investigate` as the default holding pen. Ask the user
to confirm.

## Step 5: Update Overlay

Apply the confirmed changes to `schema_overlay.yaml`. Follow `/config-management` rules:

- Section order: `supported`, `unsupported`, optional `investigate`, `ignored`
- Keys within each section are alphabetical
- Required fields differ per section; `config-overlay-model/src/lib.rs` is authoritative

Build to verify: `cargo build -p datadog-agent-config-testsupport`. Read any panic message — it
names the violated rule and the offending key.

## Step 6: Inspect Generated Output

After a successful build, spot-check the regenerated outputs:
- Config registry annotations (`config-testsupport/.../config_registry/`)
- Documentation (`docs/agent-data-plane/configuration/`)

Confirm the diff looks correct. Flag anything unexpected to the user.
