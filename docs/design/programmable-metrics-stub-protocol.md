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
    "live_generations": 2
  }
}
```

- `config_states` lists the latest state per config ID ADP has seen (at most one per ID). `state` values are the
  specification's acknowledgment states plus `failed` (supervision exhausted). `error` is plain text, never secrets.
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
  default program. More than one config for `policy` rejects all of them (`state: rejected`).
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
- `revision` must exceed ADP's `revision_high_water`; otherwise `rejected` ("stale revision"). A payload whose
  `artifact_hash` equals the active or preparing artifact acknowledges the current state (idempotent): a redelivery at
  the high-water revision that set the mark, or the same document approved again under a newer revision. Any other
  revision at or below the mark is stale. The mark moves for every config whose transport hash, envelope, receipt, and
  `artifact_hash` check out, even if it is then rejected.
- `approved_against` is optional. When present and different from ADP's `active_plan_hash` at prepare time, ADP still
  installs but flags the review as stale in status.

## 3. Control surface (ADP privileged API, mTLS with the IPC certificate)

| Route | Body |
|---|---|
| `GET /programmable-metrics/status` | the `status` object above plus `config_states` |
| `GET /programmable-metrics/export?scope=post_enrich` | export bundle (below) |
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
- The default program is served so that a client can compose an artifact the way ADP will (the plan crate's
  `compose::compose` takes the default program, which neither the export, a region, nor `/plan`, possibly composed,
  carries) and diff it against `/plan`. Its base (the plan crate's `export::base` of the `post_enrich` scope) is the
  export's `artifact.base.program_hash` when both come from the same lowering; a client that fetches them separately
  checks that, and fetches both again when they differ (the configuration changed between the requests). It answers
  whether or not a generation is active.
- Errors are plain text: `400` for a missing `scope` or an unknown `format`, `404` for a scope the default program does
  not declare (the body names the declared ones), `500` for a scope that cannot be exported (the body says why), and
  `503` from `/plan` and `/explain` while no generation is active.

## 4. Demo server side

- Holds the durable artifact store (on disk in its run directory): current snapshot, revision counter, history.
- Next revision = `max(local counter, status.revision_high_water) + 1`.
- Records every poll's `config_states` and `status` with a receive timestamp for the acknowledgment timeline.
