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
  "products": ["policy", "resources"],
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
      "metrics": {
        "default_program_hash": "sha256:<hex>",
        "active_artifact": { "id": "demo", "revision": 7, "artifact_hash": "sha256:<hex>" },
        "stale_base": false
      }
    },
    "resources": {
      "revision_high_water": 2,
      "active_artifact": { "id": "demo-resources", "revision": 2, "artifact_hash": "sha256:<hex>" }
    }
  }
}
```

- `config_states` lists the latest state per config ID ADP has seen (at most one per ID), each under its `product`
  (`policy` or `resources`, section 5). `state` values are the specification's acknowledgment states plus `failed`
  (supervision exhausted). `error` is plain text, never secrets. On a `pending` state, `error` says what the config
  waits for (an output its base records that the host does not declare yet, or a credential that is not configured);
  on an `active` state, it is what composition said beside the state, or `null`: the reasons the artifact runs
  degraded, each naming an output and saying that its emits are dropped (the output was removed, or now delivers
  elsewhere). Readers treat any other text there as a note that drops nothing.
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
- A payload whose artifact composes to a plan that does what the running plan does **adopts** the running generation
  (detached programs, phase 2): its config is acknowledged `active` with the generation ID already running, no
  generation is created, and `active_artifact` and `program` name it from then on. Plans are compared by what their
  nodes do, with every binding written as its value, so a detached program's literals adopt the generation of the
  default program that binds them, and a later change of a native policy setting creates no generation. Revoking it
  hands that generation back to the default program the same way, so an unmodified export is published and revoked
  without a generation of its own.
- `status.program` (additive, detached programs): `{"runs": "default" | "published", "native_policy": "applied" |
  "not_applied", "differs": ["<binding>", ...]}`, left out while no generation is active. A published program owns its
  policy settings, so native ones are not applied to it; `differs` lists the native policy settings, by binding, whose
  current value the running program does not run as the default program would (none for the default program).
- `status.stale_review` is `true` when the active artifact's `approved_against.active_plan_hash` (section 2) differs
  from the active plan hash it was prepared against, or, for a delivery that acknowledged the current state, from the
  active plan hash when it was delivered. It was added after the first version of this contract, as an
  additive member: readers that do not know it ignore it.
- `status.default_program_hash` equals `artifact.base.program_hash` of the current export bundle (section 3) for the
  `metrics` scope, so a client refetches the export when its artifact changes: its nodes, edges, or the
  fingerprints of the outputs it emits to. Catalog values (binding values, recipients, labels) can change without it,
  so a client that shows them also refetches periodically.
- The top-level `default_program_hash`, `active_artifact`, and `stale_base` describe the policy scope, `metrics`, the
  default program's one scope: the whole metrics path after its sources (until the one-scope change, the policy scope
  was `post_enrich`, the region after host tags, beside `dogstatsd_pre_aggregation`; see the [status
  document](programmable-metrics-status.md#the-demo)). `stale_review` is `true` when any active artifact's review is
  stale. `scopes` (additive) describes each replaceable scope of the default program by name: the base program hash of
  its export, the artifact active for it (`null` while the default program's region runs there), and whether that
  artifact's base is stale. ADP reports every scope's base, so it always writes `scopes`, today with the one entry
  `metrics`; a host that reports only the policy scope's base writes it while an artifact is active in another scope,
  and otherwise leaves it out, the first version's form. The protocol keeps several scopes, which the plan crate
  supports, though ADP declares one.
- `config_states` is bounded: ADP keeps the acknowledgment of every config of the latest snapshot, of every config
  whose artifact is still being installed, active, or kept to roll back to, and of the 16 configs most recently
  finished (`rejected`, `failed`, or `retired`) beyond those; older ones are forgotten.
- `status.resources` (additive, stage 7) describes the `resources` product, which has its own revisions: its
  `revision_high_water`, and the `active_artifact` whose declarations the active generation carries (`null` when it
  carries none). It is left out until ADP has seen a resources config, so a poll of a host that uses only policy
  artifacts is unchanged.

Response body (`200`):

```json
{
  "protocol_version": 1,
  "products": {
    "policy": [
      { "id": "demo", "version": 7, "sha256": "<hex of payload's UTF-8 bytes>", "payload": "<envelope JSON text>" }
    ],
    "resources": [
      { "id": "demo-resources", "version": 2, "sha256": "<hex>", "payload": "<envelope JSON text>" }
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
  or that is larger than 8 MiB is unreadable: a transport failure, which keeps the current feed. A `resources` member
  that is not a list of configs makes only that product unreadable for the poll: its current snapshot stays in effect,
  and the `policy` snapshot is read as usual.
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
| `GET /programmable-metrics/status` | the `status` object above plus `config_states`, and `recipients` (below) |
| `GET /programmable-metrics/resources` | the resources view (below) |
| `GET /programmable-metrics/export?scope=<scope>[&form=detached\|bound]` | export bundle (below) of a scope of the default program: `metrics`, its one scope; `detached` (the default) or `bound` |
| `GET /programmable-metrics/plan` | the active composed logical plan, resolved values included |
| `GET /programmable-metrics/default-program` | the default program ADP last lowered, resolved values included: the plan artifacts are composed into |
| `GET /programmable-metrics/explain` | the explain document (JSON) for the active plan; `?format=text` for text |

Export bundle:

```json
{
  "export_version": 1,
  "scope": "metrics",
  "artifact": { "...": "PolicyArtifact of the default program's scope, bindings unresolved, base filled in" },
  "ir_versions": { "current": 1, "accepted": [1, 1] },
  "catalog": { "...": "the serialized plan-crate Catalog: scope inputs with port types, outputs with accepts, fingerprint, recipients; bindings with type, liveness, class, current value; the recipient directory" },
  "labels": { "<output id>": "<display label>" },
  "annotations": {
    "nodes": { "<node id>": "<note>" },
    "bindings": { "<binding>": "<class>: <what it is>" },
    "omitted": "Not shown, because they do nothing as configured: ..."
  },
  "form": "detached",
  "omitted": { "<node id>": "identity | constant_split | unreachable" }
}
```

- `form` (additive, detached programs): `detached`, the default, writes every parameter bound to a `policy` binding as
  its current value, keeps `fact` and `signal` bindings bound, folds `flag` and `is_empty` over literals, and writes
  each `emit` to a native output whose recipients a set of directory names stands for exactly as an `emit` to those
  names (`{"to": ["primary", "https://..."]}`); its base is the bound export's. `bound` writes the default program as
  lowered, bound to its native settings. ADP refuses an artifact that binds a `policy` binding, with a rejection
  message that names the binding, the value to write, and the facts and signals a program may bind, so `bound` is for
  tools that study lowering. The acknowledgment carries the message only: `policy_binding` names the error in the
  plan crate and the demo, and is no code on the wire. A
  reader of a bundle without `form` reads `bound`; the catalog then has no `detached_programs` member either.
- An `emit` names its output (`{"output": "<id>"}`) or recipients (`{"to": ["<name>", ...]}`), names from the
  catalog's directory on one delivery host. ADP resolves the names into the output with exactly those recipients,
  adding an implied output (`implied_<emit node>_<set digest>`, the digest's first eight hexadecimal digits) for a set
  no output delivers to, so an artifact records no
  fingerprint for an `emit` to recipients. A config whose `emit` names an unknown recipient, or recipients without an
  output yet, is `pending` with the reason; one whose recipients lie on two hosts, or that the host adds no output for,
  is `rejected`. The catalog document's `detached_programs: true` says that a program binds facts and signals only.

- `catalog` is exactly the plan crate's `MemoryCatalog` document, so the plan crate reads it back. `labels` (optional,
  left out when there are none): display labels by output ID. Readers also accept a `label` member on a catalog
  output, for an output `labels` does not label; ADP does not write it.
- Each catalog binding has a `class`: `fact` (of the host, such as `host.tags`), `signal` (a state a program reacts to,
  such as `multi_region_failover.metric_mirroring.enabled`), or `policy` (a native policy setting); a document without
  one declares `policy`. The catalog's `recipients` member is the recipient directory: each name a program may address
  (`primary`, `mrf`, `cluster_agent`, each additional endpoint's URL as the forwarder resolves it, with its scheme, or
  `sha256:` and 16 hexadecimal digits of its digest for a URL status does not spell out, and a resources artifact's
  names),
  with the recipient identities it stands for, its delivery host, its endpoint as status spells it, and its `source`
  (`native`, `config`, or `resources`). It never holds a credential. Readers ignore members of an entry they do not
  know, and leave out an entry from a source they do not know.
- `explain` (the route below) is version 2 since detached programs, phase 2: nodes carry no provenance, and the plan's
  `provenance.artifacts` names each composed artifact with the `scope` it replaces, whose nodes are its program. A
  default program's `provenance.native_components` maps a node to the native component it does the work of, for the
  cost model and the layout built at boot; a composed plan has none.
- `annotations` (optional, additive): ADP's notes for tools, one line per exported node and per binding, which an
  editor prints as comments, and `omitted`, one line naming what the export left out, in words a reader knows, which
  an editor prints at the top. They are not part of the artifact. Readers ignore members they do not know.
- `omitted` (optional, additive): the nodes of the scope a `detached` export leaves out because they do nothing as
  configured (amendment 191), by local ID, each with why: `identity` (a node that passes every record through
  unchanged with its literal parameters, such as a mapper with no profiles or a one-input `merge`), `constant_split`
  (a `split` its literal operands decide, such as an empty filterlist), or `unreachable` (a node only such a split's
  dead port reached, such as its `drop`). A program built from the export does not have them: published unmodified
  over the default program, it is a new generation that builds no kernel and makes no cut, acknowledged `active` on
  that generation. A `bound` export leaves nothing out.
- The `metrics` scope has one input per metrics source the configuration enables, each named after its source's binding
  and carrying what the source produces (`dogstatsd`, `otlp`, `checks`, `liveness`, `trace_sampler`), and no exits: its
  artifact covers every node after the sources, aggregation included. The catalog document's `single_aggregation:
  true` says that a program has at most one aggregation, which validation then checks (`single_aggregation`). A scope
  with exits, which the plan crate supports and ADP no longer declares (`dogstatsd_pre_aggregation` had input
  `metrics` and exits `aggregated` and `passthrough`), is exported up to its exits: the artifact's edges into an exit's
  port are written as entering `exit.<name>`, and its base records no output when its records leave through exits
  rather than `emit`s. An artifact reaches the exits the same way; an exit is only ever an edge's target.
- The default program is served so that a client can compose an artifact the way ADP will (the plan crate's
  `compose::compose_all` takes the default program, which neither the export, a region, nor `/plan`, possibly composed,
  carries) and diff it against `/plan`. ADP composes an artifact with the artifacts in effect for the other scopes, so
  a client composes it with those `status.scopes` (or, for the policy scope, the top-level `active_artifact`) names
  active, under their revisions. Its base (the plan crate's `export::base` of a scope) is that scope's export's
  `artifact.base.program_hash` when both come from the same lowering; a client that fetches them separately checks
  that, and fetches both again when they differ (the configuration changed between the requests). It answers whether
  or not a generation is active.
- Errors are plain text: `400` for a missing `scope` or an unknown `format`, `404` for a scope the default program does
  not declare (the body names the declared ones), `500` for a scope that cannot be exported (the body says why), and
  `503` from `/plan` and `/explain` while no generation is active.

Recipient incarnations, the status route's `recipients` member (stage 7; a poll's status does not carry it):

```json
[
  {
    "host": "dd_out",
    "id": "dd_out/declared/team_d/http://127.0.0.1:17105",
    "slot": 2,
    "incarnation": 5,
    "state": "preparing | live | draining | failed | exiting | retired",
    "provenance": "native",
    "draining_since_ms": 1791740787505,
    "last_drain": {
      "delivered": 12,
      "persisted": 0,
      "dropped": 3,
      "deadline_reached": true,
      "retired_ms": 1791740817505
    }
  }
]
```

- First the incarnations the installer holds, by host and slot, then the latest 16 that retired after a release, oldest
  first, with state `retired` and `last_drain`, what the drain did. An incarnation an abort dropped, or a loop that
  ended on its own, is not a retirement and is not listed. `provenance` is `"native"` or `{"declared": {"revision": 2,
  "hash": "sha256:<hex>"}}`, the resources revision that declared the incarnation; it is left out when unknown.
  `draining_since_ms` is present while the incarnation drains; times are milliseconds since the Unix epoch.
- The recipient's delivery host counts each retirement in `plan_recipient_retirements_total{outcome}`, by what
  `last_drain` says: `deadline` when `deadline_reached`, else `discarded` when `dropped` is not 0, else `persisted`
  when `persisted` is not 0 (a close that wrote the queue to disk, such as a `persist` closure), else `drained`.

The resources view, `GET /programmable-metrics/resources`:

```json
{
  "declared": { "config": "demo-resources", "state": "active", "revision": 2, "error": null, "installed_revision": 2 },
  "missing_credentials": [],
  "hosts": [
    {
      "host": "dd_out",
      "encoder": "dd_metrics_encode",
      "declared_recipients": true,
      "leases": [{ "lease": 3, "output": "resources_team_d", "state": "preparing | live | draining" }],
      "withdrawn": [],
      "conflicts": [{ "declared": "<recipient id>", "duplicates": "<recipient id>" }],
      "client_failure": "<why the HTTP client its settings ask for could not be built>"
    }
  ],
  "outputs": [
    {
      "id": "resources_team_d", "label": "Team D", "host": "dd_out",
      "fingerprint": "sha256:<hex>",
      "recipients": ["dd_out/declared/team_d/http://127.0.0.1:17105"],
      "provenance": { "declared": { "revision": 2, "hash": "sha256:<hex>" } }
    }
  ],
  "recipients": [
    {
      "id": "dd_out/declared/team_d/http://127.0.0.1:17105", "host": "dd_out", "endpoint": "http://127.0.0.1:17105",
      "credential": "credentials/team_d", "classes": "metrics_only",
      "delivery": { "bounded": { "queue_bytes": 4194304, "spill": "never", "on_retire": "attempt" } },
      "provenance": { "declared": { "revision": 2, "hash": "sha256:<hex>" } }
    }
  ],
  "budget": { "granted": 268435456, "used": 3211264 }
}
```

- `declared` is the resources config's acknowledgment, with the revision lowering declares resources from
  (`installed_revision`), or `null` before a resources config was delivered; `missing_credentials` lists, by reference,
  the credentials that held artifacts use and the configuration does not provide.
- `hosts` lists every fixed delivery host with its encode host: its leases, the recipients its latest publication
  withdrew (identities the current lowering no longer declares, until the next publication), conflicts (two recipients
  sending with one key), and, while it sends through an older client, why it could not build the one its settings ask
  for (also reported in `status.degraded`). `outputs` and `recipients` are the declarations of the latest lowering,
  native and declared, credentials by reference only. `budget` is the resource budget: `granted` is `null` when nothing
  is counted (no verified memory limit).

Lanes (stage 9). The status route has **no `lanes` member**, and a poll's status carries none: lane counts are fixed
at boot and are runtime layout, never plan content. A client reads the layout from the explain document's physical
sections (`GET /programmable-metrics/explain`, its `physical` member) and per-lane activity from ADP's telemetry. The
physical sections' lane members are additive within `explain`'s versioning rule, each left out at its default, so a
document of a frame on one lane reads as before:

```json
{
  "layout": { "dogstatsd_lanes": 2, "state_lanes": 2, "post_lanes": 3, "pipelining": false },
  "fixed": [
    { "node": "dsd_in", "admission_points": [
      { "id": 0, "lanes": 2, "port": "metrics", "to": "dsd_pre_aggregation", "routing": "fixed", "feeds_state": true }
    ] }
  ],
  "slots": [
    { "name": "dsd_pre_aggregation", "lanes": 2, "exits": [
      { "name": "dsd_agg", "routing": "keyed", "progress_points": [0, 1],
        "key_groups": [{ "lane": 0, "first": 0, "last": 127 }, { "lane": 1, "first": 128, "last": 255 }] },
      { "name": "metrics_enrich", "routing": "affinity" }
    ] },
    { "name": "dsd_agg", "lanes": 2, "exits": [{ "name": "metrics_enrich", "routing": "spread" }],
      "isolation": { "output_point": 6, "output_points": [6, 7], "upstream": [0, 1] } }
  ],
  "edges": [{ "...": "...", "routing": "keyed" }],
  "segments": [{ "...": "...", "lanes": 2 }]
}
```

- `layout` is present whenever the frame runs a slot on more than one lane or was laid out without R1's pipelining
  (any `lanes.mode: auto`); `pipelining` is `false` whenever lanes are on.
- `lanes` (default 1) is a point group's or a slot's lane count; a point with lanes has consecutive IDs from `id`.
- `routing` (default `one`) is how an admission point, an exit, or an edge reaches the lanes of the slot it feeds:
  `fixed` (lane `i` to lane `i`), `keyed` (by context key through 256 key groups; `key_groups` lists each target lane's
  contiguous range), `affinity` (lane `i` to lane `i mod M`), or `spread` (each buffer to the first lane with room).
- `output_points` lists the admission point IDs of each state lane's time-driven output, for an isolation slot on
  several lanes (`output_point` stays lane 0's).

Per-lane telemetry, each series tagged `component_id` (the slot) and `lane` (0 to 63): `plan_lane_events_total`,
`plan_lane_batches_total`, `plan_lane_busy_nanos_total`, `plan_lane_stamp_wait_nanos_total`, and
`plan_lane_input_depth` for every lane; `plan_lane_routed_buffers_total`, `plan_lane_routed_events_total`, and
`plan_lane_progress_total` for the lanes of a slot with keyed exits; and for state lanes `plan_lane_contexts`,
`plan_lane_watermark_lag_seconds`, the histograms `plan_lane_flush_seconds` and `plan_lane_flush_blocked_seconds`,
`plan_lane_crossings_total{kind: cut | reset}` (per-lane crossing state), `plan_lane_crossing_overlap_contexts`,
`plan_lane_tick_divergence_total`, `plan_lane_tick_straggler_total`, `plan_lane_reset_overflow_total`, and
`plan_lane_misrouted_total`, which must stay 0. The demo's Lanes panel and its check steps S12 to S14 read exactly these
(8.8 of the stages 7 and 9 plan).

## 4. Demo server side

- Holds the durable artifact store (on disk in its run directory): current snapshot, revision counter, history.
- Serves one config per scope: `demo` for the policy scope, `metrics`, and `demo-<scope>` for any other scope, which
  ADP refuses. A publish replaces the config of its artifact's scope; a revoke, from the UI or the demo's API, empties
  the policy snapshot.
- Next revision = `max(local counter, status.revision_high_water) + 1`, one counter for every scope.
- Serves the `resources` product's one config as `demo-resources`, with a revision counter of its own, next revision
  `max(local counter, status.resources.revision_high_water) + 1`.
- Records every poll's `config_states` and `status` with a receive timestamp for the acknowledgment timeline.

## 5. The `resources` product (stage 7)

A resources artifact declares recipients (endpoints of a delivery host, with credentials the operator configured) and
outputs that deliver to them, apart from native configuration. Policy artifacts then route to those outputs by ID.

- The product holds **one config** (the demo's ID is `demo-resources`). A snapshot with more than one config refuses
  them all; an empty snapshot, or a missing `resources` member, revokes every declared resource.
- The envelope is the policy product's (section 2), without `approved_against`, with a resources document as `artifact`.
  `artifact_hash` is the plan crate's hash of the document exactly as received.
- Revisions follow the policy product's rules on a **mark of their own** (`status.resources.revision_high_water`): a
  revision must exceed every authentic resources revision seen; a redelivery (same revision and hash) changes nothing;
  the same document under a newer revision is approved again.
- The document:

  ```json
  {
    "resources_version": 1,
    "recipients": [
      {
        "name": "team_d",
        "host": "dd_out",
        "endpoint": "http://127.0.0.1:17105",
        "credential": "credentials/team_d",
        "protocol": { "v3_series": false, "v3_sketches": false },
        "delivery": { "queue_bytes": 4194304, "spill": "never", "on_retire": "attempt" }
      }
    ],
    "outputs": [{ "name": "team_d", "recipients": ["team_d"], "accepts": "any", "label": "Team D" }]
  }
  ```

  Unknown members are rejected. A recipient's `host` must accept declared recipients (`dd_out` only), its credential is
  `credentials/<name>`, an entry of `experimental.programmable_metrics.resources.credentials`, and it receives metrics
  only. Its endpoint is an `http` or `https` URL without user information, query, or fragment, which ADP puts in
  canonical form (lowercase scheme and host, default port and trailing slash removed); its identity is
  `<host>/declared/<credential name>/<canonical endpoint>`, the endpoint written as a digest when long. An output's ID
  is `resources_<name>`; it names at least one recipient, all on one host. At most 16 recipients and 16 outputs.
  `delivery` is optional: `queue_bytes` bounds the recipient's in-memory retry queue (at least 1 byte; ADP caps it at
  the host's native bound), `spill` (`native` or `never`) says whether it may spill to disk as the host's storage
  settings allow, and `on_retire` (`native`, `attempt`, `persist`, `discard`) says what its queued work becomes when it
  is removed.
- Acknowledgments: `rejected` (the document is not valid, its revision is stale, or a recipient it adds failed to
  prepare; a corrected artifact is published as a new revision), `pending` with `error` naming the credentials it waits
  for, `pending` while the generation lowered with it is not active yet, `active` once it is, `retired` once a newer
  revision or a revocation replaced it and the last generation lowered with it retired.
- A newer revision that is refused, rejected, or pending leaves the one in use in place; declared resources are removed
  only by a revocation, or when the revision in use itself becomes pending (a credential it uses was removed).
- A policy artifact whose base records a declared output the host does not declare yet is `pending` ("waiting for output
  `resources_<name>`") and installs once a resources artifact declares it. The artifact in effect whose declared output
  is removed (the resources revoked) stays `active`, degraded: its emits to the output are dropped and counted as its
  policy loss, and they return when the output is declared again.
- A removed recipient is withdrawn at the resource transaction that follows the change: payloads routed to it from then
  on are dropped and counted (`plan_recipient_withdrawn_payloads_total`); what it already holds drains for at most 30
  seconds, then its `on_retire` closure applies.
