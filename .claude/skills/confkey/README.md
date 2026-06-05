# confkey Skill

Workflows for analyzing individual configuration keys (ConfKeys) for parity between the Datadog
Agent and ADP's DogStatsD implementation. Classification state lives in `schema_overlay.yaml` — see
the `/config-management` skill for the full data model.

Glossary:
- **ConfKey** — a config key used by the Agent, ADP, or both.
- **FeatureState** — implementation reality: `PARITY`, `MISSING`, `DIVERGENT`, `ADP_ONLY`,
  `NOT_APPLICABLE`, `UNKNOWN`.

* * *

## Modes

### `audit <key> [key ...]`

Analyzes one or more ConfKeys via a clean-room sub-agent — static code analysis of how each key is
registered and used in both codebases. Shows you the result and proposes overlay updates (e.g.
moving a key from `ignored` to `investigate`, opening an issue).

```
/confkey audit dogstatsd_port
/confkey audit dogstatsd_buffer_size -- check if ADP default matches the agent
```

If the key is already in the overlay, shows a side-by-side comparison so you can confirm or correct
the existing classification.

* * *

### `create-issue [key ...]`

Drafts and files GitHub issues for keys that need attention. With no arguments, scans the overlay
for keys in `unsupported` with `planned: true` and no issue yet, or keys in `investigate` with no
linked issue.

```
/confkey create-issue
/confkey create-issue dogstatsd_buffer_size dogstatsd_packet_buffer_size
```

Checks for duplicates, proposes groupings for related keys, iterates on drafts with you, offers to
file with `gh`, and updates the `issue` field in the overlay.

* * *


