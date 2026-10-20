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
  "protocol_version": 2,
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
    "stale_review": false,
    "degraded": null,
    "live_generations": 2
  }
}
```

- `config_states` lists the latest state per config ID ADP has seen (at most one per ID), each under its `product`,
  `policy`. `state` values are the specification's acknowledgment states plus `failed`
  (supervision exhausted). `error` is plain text, never secrets. On a `pending` state, `error` says what the config
  waits for (a recipient the configuration does not declare, or an output for a set of recipients that the host has
  not added yet); on an `active` state, it is what composition said beside the state, or `null`: the reasons the
  artifact runs degraded, each naming an output, or the recipients an `emit` names, and saying that their emits are
  dropped (the output was removed, or the recipients are gone). Readers treat any other text there as a note that drops
  nothing.
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
  nodes do: a parameter left out and its default are the same, and so are two sets written in another order, but a
  literal and a binding never are. The default program writes native policy settings as literals, so a later change of
  one, which a published program does not follow, creates no generation while it runs. Revoking it hands that
  generation back to the default program the same way, so a program equal to the plan running is published and revoked
  without a generation of its own.
- `status.program` (additive, detached programs): `{"runs": "default" | "published", "native_policy": "applied" |
  "not_applied", "differs": ["<node>", ...]}`, left out while no generation is active. A published program owns its
  policy settings, so native ones are not applied to it; `differs` lists the nodes of the default program, by name,
  whose parameters come from native policy settings and that the running program does not run as the default program
  writes them now (none for the default program), such as `dsd_prefix_filter` after `metric_filterlist`
  changes (version 2's step V3; before, it listed binding names).
- `status.stale_review` is `true` when the active artifact's `approved_against.active_plan_hash` (section 2) differs
  from the active plan hash it was prepared against, or, for a delivery that acknowledged the current state, from the
  active plan hash when it was delivered. It was added after the first version of this contract, as an
  additive member: readers that do not know it ignore it.
- `status.default_program_hash` is the hash of the default program's core plan (the plan crate's
  `elaborate::elaborate_default`, version 2's step V6), so it equals `status.active_plan_hash` exactly when the default
  program, or a program that elaborates to it, runs. A client refetches the export when it changes, and checks a
  default program it holds against it by elaborating it with the export's catalog. Catalog values (recipients, labels)
  can change without it, so a client that shows them also refetches periodically. Before V6 it was the program hash of
  the detached export's artifact.
- A program is whole: it replaces the default program, the whole metrics path after its sources. Version 1 of this
  protocol carried scopes (`status.scopes`, `stale_base`, one config per scope, and an artifact's `scope` and `base`);
  version 2 removed them (version 2's steps V4 and V5).
- `config_states` is bounded: ADP keeps the acknowledgment of every config of the latest snapshot, of every config
  whose artifact is still being installed, active, or kept to roll back to, and of the 16 configs most recently
  finished (`rejected`, `failed`, or `retired`) beyond those; older ones are forgotten.

Response body (`200`):

```json
{
  "protocol_version": 2,
  "products": {
    "policy": [
      { "id": "demo", "version": 7, "sha256": "<hex of payload's UTF-8 bytes>", "payload": "<envelope JSON text>" }
    ]
  }
}
```

- A complete snapshot per product, never a delta. An empty list (or a missing product) revokes: ADP installs the
  default program.
- At most one config: its artifact is the program. A snapshot of more than one config rejects all of them (`state:
  rejected`, `revision: null`: their revisions are not seen), and whatever runs keeps running. A config held unchanged
  since the previous snapshot keeps its acknowledgment. A config of the artifact in effect, delivered again under its
  revision, acknowledges that artifact's state.
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
  the whole product: a new revision must exceed every revision seen. A payload whose `artifact_hash` equals the active
  or preparing artifact acknowledges the current state (idempotent): a redelivery under that artifact's own revision,
  or the same document approved again under a newer revision. Any other revision at or below the mark is stale. The
  mark moves for every config whose transport hash, envelope, receipt, and `artifact_hash` check out, even if it is
  then rejected.
- `approved_against` is optional. When present and different from ADP's `active_plan_hash` at prepare time, ADP still
  installs but flags the review as stale in status.

## 3. Control surface (ADP privileged API, mTLS with the IPC certificate)

| Route | Body |
|---|---|
| `GET /programmable-metrics/status` | the `status` object above plus `config_states`, and `recipients` (below) |
| `GET /programmable-metrics/resources` | the resources view (below) |
| `GET /programmable-metrics/export[?form=detached\|bound]` | export bundle (below) of the default program; `detached` (the default) or `bound` |
| `GET /programmable-metrics/plan` | the active composed logical plan, resolved values included |
| `GET /programmable-metrics/default-program` | the default program ADP last lowered, resolved values included: the plan artifacts are composed into |
| `GET /programmable-metrics/explain` | the explain document (JSON) for the active plan; `?format=text` for text |

Export bundle:

```json
{
  "export_version": 2,
  "artifact": { "...": "PolicyArtifact of the default program, bindings unresolved" },
  "ir_versions": { "current": 1, "accepted": [1, 1] },
  "catalog": { "...": "the serialized plan-crate Catalog: outputs with accepts and recipients; bindings with type, liveness, class, current value; the recipient directory" },
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
  names (`{"to": ["primary", "https://..."]}`). `bound` writes the default program as
  lowered, bound to its native settings. ADP refuses an artifact that binds a `policy` binding, with a rejection
  message that names the binding, the value to write, and the facts and signals a program may bind, so `bound` is for
  tools that study lowering. The acknowledgment carries the message only: `policy_binding` names the error in the
  plan crate and the demo, and is no code on the wire. A
  reader of a bundle without `form` reads `bound`; the catalog then has no `detached_programs` member either.
- An `emit` names its output (`{"output": "<id>"}`) or recipients (`{"to": ["<name>", ...]}`), names from the
  catalog's directory on one delivery host. ADP resolves the names into the output with exactly those recipients,
  adding an implied output (`implied_<emit node>_<set digest>`, the digest's first eight hexadecimal digits) for a set
  no output delivers to. A config whose `emit` names an unknown recipient, or recipients without an
  output yet, is `pending` with the reason; one whose recipients lie on two hosts, or that the host adds no output for,
  is `rejected`. The catalog document's `detached_programs: true` says that a program binds facts and signals only.

- `catalog` is exactly the plan crate's `MemoryCatalog` document, so the plan crate reads it back. `labels` (optional,
  left out when there are none): display labels by output ID. Readers also accept a `label` member on a catalog
  output, for an output `labels` does not label; ADP does not write it.
- Each catalog binding has a `class`: `fact` (of the host, such as `host.tags`), `signal` (a state a program reacts to,
  such as `multi_region_failover.metric_mirroring.enabled`), or `policy` (a native policy setting); a document without
  one declares `policy`. The catalog's `recipients` member is the recipient directory: each name a program may address
  (`primary`, `mrf`, `cluster_agent`, each additional endpoint's URL as the forwarder resolves it, with its scheme, or
  `sha256:` and 16 hexadecimal digits of its digest for a URL status does not spell out), with the recipient
  identities it stands for, its delivery host, and its endpoint as status spells it. It never holds a credential.
  Readers ignore members of an entry they do not know, such as the `source` earlier versions wrote.
- `explain` (the route below) is version 3 since version 2's step V4: nodes carry no provenance, and the plan's
  `provenance.artifact` names the composed artifact, by revision and hash, whose nodes are the program. A
  default program's `provenance.native_components` maps a node to the native component it does the work of, for the
  cost model and the layout built at boot; a composed plan has none.
- `annotations` (optional, additive): ADP's notes for tools, one line per exported node and per binding, which an
  editor prints as comments, and `omitted`, one line naming what the export left out, in words a reader knows, which
  an editor prints at the top. They are not part of the artifact. Readers ignore members they do not know.
- `omitted` (optional, additive): the nodes of the default program a `detached` export leaves out because they do nothing as
  configured (amendment 191), by local ID, each with why: `identity` (a node that passes every record through
  unchanged with its literal parameters, such as a mapper with no profiles or a one-input `merge`), `constant_split`
  (a `split` its literal operands decide, such as an empty filterlist), or `unreachable` (a node only such a split's
  dead port reached, such as its `drop`). A program built from the export does not have them: published unmodified
  over the default program, it is a new generation that builds no kernel and makes no cut, acknowledged `active` on
  that generation. A `bound` export leaves nothing out.
- The default program has one source per metrics source the configuration enables, each named after its source's
  binding and carrying what the source produces (`dogstatsd`, `otlp`, `checks`, `liveness`, `trace_sampler`): its
  artifact covers every node after the sources, aggregation included, and reads each source as `input.<name>`. The
  catalog document carries the telemetry contract (`telemetry_contract`), against which validation warns.
- The default program is served so that a client can compose an artifact the way ADP will (the plan crate's
  `compose::compose` takes the default program, which neither the export nor `/plan`, possibly composed, carries) and
  diff it against `/plan`. A client that fetches the export and the default program separately checks that the default
  program exports as the export (the same program hash), and that it elaborates to `status.default_program_hash`, and
  fetches both again when either differs (the configuration changed between the requests). It answers whether or not
  a generation is active. `/plan` is the active core plan, every binding written as its value.
- Errors are plain text: `400` for an unknown `form` or `format`, `500` for a default program that cannot be exported
  (the body says why), and `503` from `/plan` and `/explain` while no generation is active.

Recipient incarnations, the status route's `recipients` member (stage 7; a poll's status does not carry it):

```json
[
  {
    "host": "dd_out",
    "id": "dd_out/additional/0/http://127.0.0.1:17105",
    "slot": 2,
    "incarnation": 5,
    "state": "preparing | live | draining | failed | exiting | retired",
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
  ended on its own, is not a retirement and is not listed. `draining_since_ms` is present while the incarnation drains;
  times are milliseconds since the Unix epoch.
- The recipient's delivery host counts each retirement in `plan_recipient_retirements_total{outcome}`, by what
  `last_drain` says: `deadline` when `deadline_reached`, else `discarded` when `dropped` is not 0, else `persisted`
  when `persisted` is not 0 (a close that wrote the queue to disk, such as a `persist` closure), else `drained`.

The resources view, `GET /programmable-metrics/resources`:

```json
{
  "hosts": [
    {
      "host": "dd_out",
      "encoder": "dd_metrics_encode",
      "leases": [{ "lease": 3, "output": "implied_to_team_b_true_e7b845f5", "state": "preparing | live | draining" }],
      "withdrawn": [],
      "conflicts": [],
      "client_failure": "<why the HTTP client its settings ask for could not be built>"
    }
  ],
  "outputs": [
    {
      "id": "implied_to_team_b_true_e7b845f5", "label": "Implied output of emit to_team_b_true", "host": "dd_out",
      "recipients": ["dd_out/additional/0/http://127.0.0.1:17102"]
    }
  ],
  "recipients": [
    {
      "id": "dd_out/additional/0/http://127.0.0.1:17102", "host": "dd_out", "endpoint": "http://127.0.0.1:17102",
      "credential": "native:additional/<endpoint digest>/0", "classes": "all", "delivery": "native"
    }
  ],
  "budget": { "granted": 268435456, "used": 3211264 }
}
```

- `hosts` lists every fixed delivery host with its encode host: its leases, the recipients its latest publication
  withdrew (identities the current lowering no longer declares, until the next publication), conflicts (two recipients
  sending with one key, which only recipients of a declared origin can have, so the list is empty), and, while it sends
  through an older client, why it could not build the one its settings ask for (also reported in `status.degraded`).
  `outputs` and `recipients` are the declarations of the latest lowering, native and implied, credentials by reference
  only. `budget` is the resource budget: `granted` is `null` when nothing is counted (no verified memory limit).

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

- Holds the durable artifact store (on disk in its run directory, format 3): current snapshot, revision counter,
  history. A store an earlier version wrote, with one config per scope, does not read.
- Serves at most one config, `demo`. A publish replaces it; a revoke, from the UI or the demo's API, empties the
  policy snapshot.
- Next revision = `max(local counter, status.revision_high_water) + 1`.
- Records every poll's `config_states` and `status` with a receive timestamp for the acknowledgment timeline.

## 5. The `resources` product (removed)

Stage 7 had a second product, `resources`, whose one config declared recipients and outputs apart from native
configuration. Version 2's step V2 removed it ([version 2 simplifications](programmable-metrics-v2.md)): destinations
are the configuration's endpoints, which ADP applies live, and programs reach them by name or through implied outputs.
ADP asks for `policy` only and ignores a `resources` member in an answer; the demo's stub serves `policy` only.
