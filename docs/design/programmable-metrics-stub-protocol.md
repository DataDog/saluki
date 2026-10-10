# Artifact channel stub protocol and ADP control surface (contract between ADP and the demo server)

Normative for both sides: ADP's stage 5 stub worker (`bin/agent-data-plane/src/programmable_metrics/channel_stub.rs`)
and the demo server (`bin/programmable-metrics-demo`). All JSON; unknown fields are ignored by readers (additive
evolution); `protocol_version` bumps only on removals or renames.

## 1. Poll (ADP -> stub server)

ADP polls `POST {stub_url}/v1/artifacts/poll` every `poll_interval` (default 1 s): the poll path is appended to the
path of the stub URL, less a trailing `/`. `stub_url` must be an `http://` URL of a loopback address (`127.0.0.1`,
`[::1]`, `localhost`) without a query; ADP refuses other URLs at startup with an actionable error.

Request body:

```json
{
  "protocol_version": 1,
  "client": { "id": "<random per ADP process>", "version": "<ADP version>" },
  "products": ["policy"],
  "config_states": [
    {
      "product": "policy",
      "id": "demo",
      "version": 7,
      "revision": 7,
      "artifact_hash": "sha256:<hex>",
      "state": "pending | prepared | active | retired | rejected | failed",
      "generation": 12,
      "cut_at": null,
      "error": null
    }
  ],
  "status": {
    "default_program_hash": "sha256:<hex>",
    "revision_high_water": 7,
    "active_generation": 12,
    "active_plan_hash": "sha256:<hex>",
    "active_artifact": { "id": "demo", "revision": 7, "artifact_hash": "sha256:<hex>" },
    "stale_base": false,
    "stale_review": false,
    "degraded": null,
    "live_generations": 2,
    "scopes": {
      "dogstatsd_pre_aggregation": {
        "default_program_hash": "sha256:<hex>",
        "active_artifact": { "id": "demo-dogstatsd_pre_aggregation", "revision": 6, "artifact_hash": "sha256:<hex>" },
        "stale_base": false
      },
      "post_enrich": {
        "default_program_hash": "sha256:<hex>",
        "active_artifact": { "id": "demo", "revision": 7, "artifact_hash": "sha256:<hex>" },
        "stale_base": false
      }
    }
  }
}
```

- `config_states` lists the latest state per config ID ADP has seen (at most one per ID). `state` values are the
  specification's acknowledgment states plus `failed` (supervision exhausted). `error` is plain text, never secrets.
- `prepared` also means a cut installation waiting for its cut: an artifact that changes a state owner's time
  contract (such as an aggregation's window), or adds or removes one, installs at a bucket boundary rather than at once.
  While the cut is armed, `cut_at` is the admission time it takes effect at, in whole milliseconds since the Unix epoch;
  it is `null` otherwise, and once the cut is finalized. The config becomes `active` once the cut can no longer be
  cancelled: when the first batch is stamped with its generation, which may be before `cut_at`, or when a cancellation
  could not take the admission points' locks in time, since a batch may have been stamped meanwhile (the generation may
  then be `active` before any batch carries it).
- A revoked artifact stays `active` until a generation without it replaces its own. When that replacement waits for a
  cut in flight, which can take the cut's whole wait, its `error` is `revoked; active until replaced` while it stays
  `active`.
- `status.active_artifact` is `null` when the default program runs. Otherwise it is the latest accepted delivery of
  the artifact the active generation serves: a payload that acknowledges the current state (section 2) under a newer
  revision, or in another config, becomes the active artifact without a new generation. `degraded` is `null` or a
  short reason.
- `status.stale_review` is `true` when the active artifact's `approved_against.active_plan_hash` (section 2) differs
  from the active plan hash it was prepared against, or, for a delivery that acknowledged the current state, from the
  active plan hash when it was delivered. It was added after the first version of this contract, as an
  additive member: readers that do not know it ignore it.
- `status.default_program_hash` equals `artifact.base.program_hash` of the current export bundle (section 3) for the
  `post_enrich` scope, so a client refetches the export when its artifact changes: its nodes, edges, or the
  fingerprints of the outputs it emits to. Catalog values (binding values, recipients, labels) can change without it,
  so a client that shows them also refetches periodically.
- The top-level `default_program_hash`, `active_artifact`, and `stale_base` describe the policy scope, `post_enrich`,
  as in the first version of this contract; `stale_review` is `true` when any active artifact's review is stale.
  `scopes` (additive) describes each replaceable scope of the default program by name: the base program hash of its
  export, the artifact active for it (`null` while the default program's region runs there), and whether that
  artifact's base is stale. ADP reports every scope's base, so it always writes `scopes`; a host that reports only the
  policy scope's base writes it while an artifact is active in another scope, and otherwise leaves it out, the first
  version's form.
- `config_states` is bounded: ADP keeps the acknowledgment of every config of the latest snapshot, of every config
  whose artifact is still being installed, active, or kept to roll back to, and of the 16 configs most recently
  finished (`rejected`, `failed`, or `retired`) beyond those; older ones are forgotten.

Response body (`200`):

```json
{
  "protocol_version": 1,
  "products": {
    "policy": [
      { "id": "demo", "version": 7, "sha256": "<hex of payload's UTF-8 bytes>", "payload": "<envelope JSON text>" }
    ]
  }
}
```

- A complete snapshot per product, never a delta. An empty list (or a missing product) revokes: ADP installs the
  default program.
- At most one config per scope: each artifact replaces one scope of the default program (its `scope` member), and ADP
  composes the artifacts of every scope together. More than one config for one scope rejects all of them for that
  scope (`state: rejected`, `revision: null`: their revisions are not seen); the other scopes' configs are handled as
  usual. A config held unchanged since the previous snapshot keeps its acknowledgment, so a snapshot that changes one
  scope's config leaves the other scopes' artifacts as they are; one rejected for what other configs of the previous
  snapshot held (another config for its scope, or its revision) is handled anew. A config of the artifact in effect
  for its scope, delivered again under its revision, acknowledges that artifact's state.
- A snapshot that holds no config for a scope whose artifact is in effect revokes that scope's artifact (as long as
  every config in the snapshot could be read), even if the config that delivered it now delivers another scope's: ADP
  installs the default program's region there, and keeps the other scopes' artifacts.
- New artifacts of several scopes in one snapshot install together. When the plan they compose to is rejected, ADP
  checks each alone with the artifacts in effect for the other scopes: one rejected alone is `rejected` with its own
  error, and the others install. Artifacts that each install alone but not together are all `rejected`.
- `payload` is a JSON **string** containing the envelope, so `sha256` covers exact bytes. ADP verifies it and reports
  `rejected` ("transport hash mismatch") on mismatch.
- A transport failure (connection error, non-200, unparsable body) keeps ADP's current feed; it never revokes. ADP
  retries on the next interval.
- `products` is required. An answer without it is unreadable, a transport failure, and never revokes; an empty
  `products` object, or a missing or empty product list, revokes.
- ADP reads only the products it asks for. Another product is ignored whatever its entries look like, so a stub can
  serve products a given ADP does not know.
- An answer that states another `protocol_version`, whose `policy` member is not a list of configs (`null` included),
  or that is larger than 8 MiB is unreadable: a transport failure, which keeps the current feed.
- ADP sends the first poll at startup and then one every `poll_interval`, measured from when each poll was due; a poll
  that takes long delays the next one. A poll whose answer, body included, does not arrive in full within 5 seconds is
  a transport failure, and the next poll retries.

## 2. Envelope (inside `payload`)

```json
{
  "revision": 7,
  "artifact_hash": "sha256:<hex>",
  "approved_against": { "active_plan_hash": "sha256:<hex>" },
  "artifact": { "...": "a PolicyArtifact document (saluki-metric-plan IR)" }
}
```

- `artifact_hash` is the plan crate's hash of the artifact document exactly as received (`ReceivedArtifact`). ADP
  recomputes it; a mismatch is `rejected`.
- `revision` must exceed ADP's `revision_high_water`; otherwise `rejected` ("stale revision"). There is one mark for
  the whole product: a new revision for any scope must exceed every revision seen, whatever scope saw it, and the
  configs of one snapshot are ordered against the mark as the snapshot found it. A payload whose `artifact_hash` equals
  the active or preparing artifact of its scope acknowledges the current state (idempotent): a redelivery under that
  artifact's own revision, whatever the mark (another scope's newer artifact may have moved it since), or the same
  document approved again under a newer revision. Any other revision at or below the mark is stale. Two or more
  configs of one snapshot under one revision above the mark are all `rejected` (`revision` reported, but not seen):
  none is newer than the others. Each is handled anew in the next snapshot, even held unchanged. The mark moves for
  every other config whose transport hash, envelope, receipt, and `artifact_hash` check out, even if it is then
  rejected.
- `approved_against` is optional. When present and different from ADP's `active_plan_hash` at prepare time, ADP still
  installs but flags the review as stale in status.

## 3. Control surface (ADP privileged API, mTLS with the IPC certificate)

| Route | Body |
|---|---|
| `GET /programmable-metrics/status` | the `status` object above plus `config_states` |
| `GET /programmable-metrics/export?scope=<scope>` | export bundle (below) of `post_enrich` or `dogstatsd_pre_aggregation` |
| `GET /programmable-metrics/plan` | the active composed logical plan, resolved values included |
| `GET /programmable-metrics/default-program` | the default program ADP last lowered, resolved values included: the plan artifacts are composed into |
| `GET /programmable-metrics/explain` | the explain document (JSON) for the active plan; `?format=text` for text |

Export bundle:

```json
{
  "export_version": 1,
  "scope": "post_enrich",
  "artifact": { "...": "PolicyArtifact of the default program's scope, bindings unresolved, base filled in" },
  "ir_versions": { "current": 1, "accepted": [1, 1] },
  "catalog": { "...": "the serialized plan-crate Catalog: scope inputs with port types, outputs with accepts, fingerprint, recipients; bindings with type, liveness, current value" },
  "labels": { "<output id>": "<display label>" }
}
```

- `catalog` is exactly the plan crate's `MemoryCatalog` document, so the plan crate reads it back. `labels` (optional,
  left out when there are none): display labels by output ID. Readers also accept a `label` member on a catalog
  output, for an output `labels` does not label; ADP does not write it.
- A scope with exits, such as `dogstatsd_pre_aggregation` (input `metrics`, exits `aggregated` and `passthrough`), is
  exported up to its exits: the artifact's edges into an exit's port are written as entering `exit.<name>`, and its
  base records no output when its records leave through exits rather than `emit`s. An artifact reaches the exits the
  same way; an exit is only ever an edge's target.
- The default program is served so that a client can compose an artifact the way ADP will (the plan crate's
  `compose::compose_all` takes the default program, which neither the export, a region, nor `/plan`, possibly composed,
  carries) and diff it against `/plan`. ADP composes an artifact with the artifacts in effect for the other scopes, so
  a client composes it with those `status.scopes` (or, for `post_enrich`, the top-level `active_artifact`) names
  active, under their revisions. Its base (the plan crate's `export::base` of a scope) is that scope's export's
  `artifact.base.program_hash` when both come from the same lowering; a client that fetches them separately checks
  that, and fetches both again when they differ (the configuration changed between the requests). It answers whether
  or not a generation is active.
- Errors are plain text: `400` for a missing `scope` or an unknown `format`, `404` for a scope the default program does
  not declare (the body names the declared ones), `500` for a scope that cannot be exported (the body says why), and
  `503` from `/plan` and `/explain` while no generation is active.

## 4. Demo server side

- Holds the durable artifact store (on disk in its run directory): current snapshot, revision counter, history.
- Serves one config per scope: `demo` for `post_enrich`, `demo-<scope>` for any other scope. A publish replaces the
  config of its artifact's scope; a revoke removes one scope's config, or empties the snapshot.
- Next revision = `max(local counter, status.revision_high_water) + 1`, one counter for every scope.
- Records every poll's `config_states` and `status` with a receive timestamp for the acknowledgment timeline.
