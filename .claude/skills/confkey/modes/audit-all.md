# Mode: Full Audit

End-to-end discovery, classification, and documentation update across all DogStatsD ConfKeys. Use
this when starting fresh or when the ledger may have drifted from the codebase.

You may store temporary files in `{{tmp}}`=`{{saluki}}/target/.temp/dogstatsd-audit`. Delete {{tmp}}
if exists. Create {{tmp}}.

## Step 1: Gather Background Knowledge

Build enough understanding of both codebases to recognize config keys and trace their behavior. The
entry points below are good starting points, but the code evolves — follow references, search
broadly, and improvise as needed.

Known RefImpl entry points:
- `{{documentation}}/content/en/extend/dogstatsd/_index.md` -- feature overview, metric types
- `{{datadog-agent}}/pkg/config/setup/common_settings.go` -- primary config key registry
- `{{datadog-agent}}/comp/dogstatsd/` -- DogStatsD runtime reads and server logic

Known AdpImpl entry points:
- `{{saluki}}/bin/agent-data-plane/src/config.rs` -- top-level ADP config struct
- `{{saluki}}/bin/agent-data-plane/src/cli/run.rs` -- topology construction, config loading pipeline

Gate: use AskUserQuestion to briefly summarize your understanding of the audit goal and the two
implementations, and ask the user to confirm or correct before proceeding.

## Step 2: Discover

**Collect ALL ConfKeys across the entire codebase, not just DogStatsD-related ones.** DogStatsD keys
can't always be identified by name alone -- filtering happens in a later phase.

Create a sub-agent for each task. Store output in `{{tmp}}`.

- Find all ConfKeys in {{datadog-agent}} by running `../resources/find-refimpl-configs.md`

- Find all ConfKeys in {{saluki}} by running `../resources/find-adpimpl-configs.md`

- AskUserQuestion - give the user the output filenames and ask the user if they look OK before
  proceeding.

Combine the files. For keys found in multiple locations, prefer the most authoritative source:

- RefImpl: `common_settings.go` > `pkg/config/` > `cmd/agent/dist/datadog.yaml` > docs
- AdpImpl: config structs > call sites

### Definition: ConfKey csv

A ConfKey csv file looks like this:

```csv
"dogstatsd_tag_cardinality","{{datadog-agent}}/pkg/config/setup/common_settings.go:536"
"system_probe_config.internal_profiling.enabled","{{datadog-agent}}/pkg/config/setup/system_probe.go:109"
```

### Building all-conf-keys.json

Cross-reference the discovered keys against both ledger files. Write to
`{{tmp}}/all-conf-keys.json`. Schema: `../resources/all-conf-keys.schema.json`.

- If the key is in `known-configs-not-applicable.json` -> **exclude** entirely.
- If the key is in `known-configs.json` -> include with `"Status": "known"`.
- If the key is in neither -> include with `"Status": "unreviewed"`.

Each entry includes a `"Status"` field: `"known"` or `"unreviewed"`.

```json
[
	{
		"ConfKey": "histogram_aggregates",
		"Status": "known",
		"RefImpl": null,
		"AdpImpl": "lib/saluki-components/src/transforms/aggregate/config.rs:79"
	},
	{
		"ConfKey": "dogstatsd_workers_count",
		"Status": "known",
		"RefImpl": "pkg/config/setup/common_settings.go:1596",
		"AdpImpl": null
	},
	{
		"ConfKey": "some_new_key",
		"Status": "unreviewed",
		"RefImpl": "pkg/config/setup/common_settings.go:400",
		"AdpImpl": null
	}
]
```

## Step 3: Classify Unreviewed Keys

If `all-conf-keys.json` contains no `"unreviewed"` keys, skip this section.

The goal is to classify each unreviewed key as relevant or irrelevant to DogStatsD behavior. This
requires code analysis -- name prefixes alone are not sufficient. A key like `forwarder_num_workers`
has no `dogstatsd` prefix but directly affects how DogStatsD metrics are forwarded.

### Phase 1: Batch Triage

Filter `all-conf-keys.json` to only `"Status": "unreviewed"` entries. Split them into batches of
~30-50 keys. For each batch, create a sub-agent with the following instructions:

> For each key in this batch, determine whether it could plausibly affect DogStatsD behavior. Read
> the code at the listed source location(s) and trace how the key is used. A key is relevant to
> DogStatsD if it influences any of:
> 
> - Metric reception (listeners, ports, sockets, buffers, protocols)
> - Metric parsing or decoding (DogStatsD wire format, sample rates, timestamps)
> - Metric aggregation, enrichment, or tagging (context resolution, tag cardinality, host tags)
> - Metric forwarding or serialization (forwarder, endpoints, payloads, compression, retry)
> - Origin detection or container enrichment
> - General infrastructure that DogStatsD depends on (API keys, proxy, TLS, secrets, logging that
>   would affect DogStatsD components)
> 
> Respond with one CSV line per key: `"key_name","true/false","reasoning (20-70 chars)"`
> 
> Where `true` means RELEVANT (it does or could affect DogStatsD), and `false` means NOT RELEVANT.
> 
> When uncertain, err on the side of `true` (relevant). It is much worse to miss a relevant key than
> to include an irrelevant one.

Give each sub-agent access to both `{{datadog-agent}}` and `{{saluki}}` so it can read usage sites.

### Phase 2: Assemble and Review

Collect all sub-agent CSV outputs and concatenate into `{{tmp}}/new-key-recommendations.csv`:

```csv
"api_key","true","shared infra: DogStatsD forwarder needs this"
"security_agent.enabled","false","security agent only, no DogStatsD path"
"dogstatsd_port","true","directly configures DogStatsD listener"
"network_config.enable_http_monitoring","false","system probe network monitoring only"
```

Use AskUserQuestion: report how many keys were analyzed, how many recommended-relevant vs
recommended-irrelevant, and ask the user to review the file before proceeding.

### Phase 3: Update Ledgers and all-conf-keys.json

After the user approves (they may have edited the recommendations file):

1. For each key recommended as **not applicable**: append the key string to
   `{{config_docs}}/known-configs-not-applicable.json`, keeping the array sorted alphabetically.

2. For each key recommended as **relevant**: append a new entry to
   `{{config_docs}}/known-configs.json` with `feature_state: "UNKNOWN"` and `action: "INVESTIGATE"`.
   Fill `description` and `reason` from the recommendation. Set `issue` and `adp_key` to `null`.
   Keep the array sorted alphabetically by `key`.

3. Update `{{tmp}}/all-conf-keys.json`: change newly classified keys from `"Status": "unreviewed"`
   to `"known"`, and remove entries for keys added to `known-configs-not-applicable.json`.

After this step, `all-conf-keys.json` should contain only `"Status": "known"` entries -- the
relevant keys that downstream phases will analyze.

## Step 4: Analyze Feature Parity

For each relevant key in `all-conf-keys.json`, analyze both codebases to determine FeatureState.

**Final-state exclusion**: Before building batches, filter out keys that are already in a settled
state. These are skipped unless the user explicitly requests otherwise in `<details>`. A key is in a
final state if:
- `feature_state == PARITY` AND `action == NONE` — confirmed implemented, nothing pending
- `feature_state == ADP_ONLY` AND `action == NONE` — confirmed no parity needed
- `action == DOCUMENTED` — documentation work is complete

Log how many keys are excluded vs. included so the user can see the analysis scope.

### Phase 1: Dispatch Analysis Agents

Split the non-final keys into batches of 10-15. For each batch, create a sub-agent using
`../resources/analyze-features.md`. Each sub-agent performs clean room analysis -- it independently
searches both codebases. Give it the batch of ConfKey names plus paths to `{{datadog-agent}}`,
`{{saluki}}` and an output path consisting of {{outdir}}/{{outfile}}.

### Phase 2: Compile Results

Collect JSON outputs into `{{tmp}}/feature-analysis.json` (single array of all analyzed features).
Schema: `../resources/feature-analysis.schema.json`.

AskUserQuestion: report summary counts (Implemented, Missing, Divergent, ADP Only) and ask user to
review `feature-analysis.json` before updating documentation.

### Phase 3: Update docs/agent-data-plane/configuration/dogstatsd.md

Before making any edits, run the **Error Row Check** defined in `SKILL.md ## Shared Utilities` on
every key in `feature-analysis.json`. Do not produce or modify the doc until all Error rows are
resolved.

Read the current file at `{{saluki}}/docs/agent-data-plane/configuration/dogstatsd.md`. Apply
analysis with these rules:

**General preservation rule -- applies to Features table rows AND Discussion sections:**
- If existing content is semantically equivalent to the new analysis, keep the existing text
  unchanged. Don't rewrite to match sub-agent wording.
- Only update if the analysis is substantively different (status changed, description wrong, etc.).
- Add new entries for keys/discussions not yet present.
- Never remove existing rows or discussion sections.

**Features table:**
- Columns: `Config Key | Description | Status | Notes`
- Description and Notes: max 50 characters each.

**Discussion section:**
- Sub-agents emit `### key_name` markdown for noteworthy features only. Apply the preservation rule
  above.

**Status Legend:**
- Only add UNSURE to the legend if a sub-agent actually used it in the results.

**Other sections (intro, Action Items, etc.):** Do not modify.

Update the `Last updated` date at the top.

## Step 5: Audit Summary

After updating the doc, compile and present a summary of changes made during this audit run:

- **New keys classified**: count added to `known-configs.json` and
  `known-configs-not-applicable.json` during Step 3 (were `"unreviewed"`, now classified)
- **Feature state changes**: keys whose `feature_state` changed during Step 4 analysis — list each
  one with old → new values
- **Action changes**: keys whose `action` changed — list each one with old → new values
- **Newly resolved**: keys that moved to `PARITY + NONE` since last run (potential wins to
  highlight)

Use AskUserQuestion to present this summary and give the user a chance to review before continuing.

## Step 6: Surface Outstanding Work

Scan `{{config_docs}}/known-configs.json` for keys with unresolved action items. Present two tables:

**Keys needing issues** (`action` is `IMPLEMENT`, `INVESTIGATE`, or `DOCUMENT` and `issue` is null):

| Key | feature_state | action | description |
| --- | --- | --- | --- |

**Keys with open issues** (`action` is `IMPLEMENT` or `INVESTIGATE` and `issue` is not null):

| Key | feature_state | action | issue |
| --- | --- | --- | --- |

If the "needs issues" table is non-empty, use AskUserQuestion to ask the user whether they want to
run `create-issue` mode for any of those keys. If yes, dispatch to `./modes/create-issue.md` with
the selected key names as `<details>`.

## Completion Checklist

Before reporting the audit as complete, verify every item:

- [ ] No `"unreviewed"` entries remain in `{{tmp}}/all-conf-keys.json`
- [ ] `known-configs.json` passes Ledger Validation (SKILL.md `## Shared Utilities`)
- [ ] `known-configs-not-applicable.json` is valid JSON and sorted alphabetically
- [ ] `dogstatsd.md` `Last updated` date reflects today's date
- [ ] No Error row violations remain (all doc sections consistent with the mapping table)
- [ ] Audit summary presented to user and acknowledged (Step 5)
