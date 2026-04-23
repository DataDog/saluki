# Config Skill

Tracks configuration key parity between the Datadog Agent and ADP's DogStatsD implementation.
Maintains two ledger files and a documentation page:

- `docs/agent-data-plane/configuration/dogstatsd.md` — the doc
- `docs/agent-data-plane/configuration/dogstatsd/known-configs.json` — classified keys
- `docs/agent-data-plane/configuration/dogstatsd/known-configs-not-applicable.json` — dismissed keys

Glossary:
- **ConfKey** — a config key used by the Agent, ADP, or both.
- **FeatureState** — implementation reality: `PARITY`, `MISSING`, `DIVERGENT`, `ADP_ONLY`,
  `NOT_APPLICABLE`, `UNKNOWN`.
- **Action** — what to do: `NONE`, `IMPLEMENT`, `DOCUMENT`, `DOCUMENTED`, `INVESTIGATE`.

* * *

## Modes

### `audit-all`

Full sweep. Discovers all ConfKeys in both codebases, classifies any unreviewed ones, re-analyzes
parity for non-final keys, and updates the doc.

```
/confkey audit-all
```

Runs a multi-step workflow with user gates at each phase. Expect sub-agents and several
AskUserQuestion prompts. Use when starting fresh or when the ledger may have drifted.

* * *

### `audit-one <key>`

Analyzes a single ConfKey via a clean-room sub-agent, shows you the result, and proposes ledger and
doc updates.

```
/confkey audit-one dogstatsd_port
/confkey audit-one dogstatsd_buffer_size -- check if ADP default matches the agent
```

If the key is already in the ledger, shows a side-by-side comparison so you can confirm or correct
the existing classification.

* * *

### `create-issue [key ...]`

Drafts and files GitHub issues for keys that need attention. With no arguments, scans the ledger for
keys where `action` is `IMPLEMENT`, `INVESTIGATE`, or `DOCUMENT` and no issue is linked yet.

```
/confkey create-issue
/confkey create-issue dogstatsd_buffer_size dogstatsd_packet_buffer_size
```

Checks for duplicates, proposes groupings for related keys, iterates on drafts with you, offers to
file with `gh`, and updates the `issue` field in the ledger.

* * *

### `complete-issue <number>`

For when you've implemented a fix and are ready to close out the ledger. Reads the issue and diffs
the branch against main to determine what actually changed, then proposes ledger and doc updates.

```
/confkey complete-issue 1234
```

Optionally runs a fresh parity analysis to confirm the implementation matches expectations before
writing anything.

* * *

### `freeform <request>`

Loads all mode and resource context, then executes whatever you ask. Use when you want to do
something that doesn't fit the other modes — ad-hoc queries, bulk edits, cross-mode workflows.

```
/confkey freeform how many keys are still MISSING with no linked issue?
/confkey freeform reclassify dogstatsd_origin_detection as DIVERGENT and update the doc
```
