# Programmable metric processing: detached programs

The design that follows the owner's decision that programs replace native policy
([direction](programmable-metrics-next.md#direction-programs-replace-native-policy)). Its goal: simplify as much as we
can, so that the demo and the default program read well, with less magic. Phase 1 (parts A, C, F, G1, and H) is
committed (`cfd4a28072`): see [phase 1 as built](#105-phase-1-as-built). Phase 2 (parts B, D, and E, with the owner's
later decisions: no node provenance, adoption, and a standalone configuration reload for the demo) is committed
(`7f9d67a033`): see [phase 2 as built](#106-phase-2-as-built). Phase 3, part G2, the demo, is built, not committed, with
the owner's decisions for it: see [the demo as built](#85-the-demo-as-built). The detached export then learned to
leave out what does nothing as configured, built, not committed: see [what does nothing, as
built](#107-what-does-nothing-as-built). Amendments 170 to 191 are in [the stages 7 and 9
plan](programmable-metrics-stages-7-and-9.md#10-proposed-spec-amendments).

## Contents

- [1. In one minute](#1-in-one-minute)
- [2. What a program binds](#2-what-a-program-binds)
- [3. Step 1: the detached export](#3-step-1-the-detached-export)
- [4. Step 2: recipient names](#4-step-2-recipient-names)
- [5. Step 2: emits addressed to recipients](#5-step-2-emits-addressed-to-recipients)
- [6. Step 3: facts and signals only](#6-step-3-facts-and-signals-only)
- [7. Step 4: one aggregation is the aggregation](#7-step-4-one-aggregation-is-the-aggregation)
- [8. The editor and the demo](#8-the-editor-and-the-demo), and [the demo as built](#85-the-demo-as-built)
- [9. What this lets us remove](#9-what-this-lets-us-remove)
- [10. Implementation plan](#10-implementation-plan), [phase 1 as built](#105-phase-1-as-built),
  [phase 2 as built](#106-phase-2-as-built), and [what does nothing, as built](#107-what-does-nothing-as-built)
- [11. Assumptions](#11-assumptions)
- [12. Open questions for the owner](#12-open-questions-for-the-owner)

## 1. In one minute

- **Every binding gets a class**: fact, signal, or policy. Policy bindings stay only in the default program, which
  lowering writes from native configuration as today. Host tags and their deadline become fact bindings; autoscaling
  failover gains a signal.
- **The export is detached.** `GET /programmable-metrics/export` writes every policy binding as its current literal
  value and keeps facts and signals bound. Composition treats a literal equal to a binding's value as the same
  operation, so publishing an unmodified export still changes nothing. Status says which program runs and whether native
  policy settings apply.
- **Recipients have names** (`primary`, `mrf`, `cluster_agent`, a name derived from each additional endpoint's host,
  and the name a declaration gives), and **programs emit to recipients**: `emit to [team_b]`. ADP matches each distinct
  recipient set to an output, reusing a native one when the set is equal, and otherwise adds one through stage 7's
  resource transaction. One encoder runs per distinct set, as native routing groups do.
- **The demo declares teams B and C as recipients in configuration**, not as native additional endpoints with endpoint
  routing, so the demo shows a program routing. The equivalence of native routing stays proven by tests and fixtures.
- **Published programs bind only facts and signals** (`policy_binding`, gated by the catalog like
  `single_aggregation`).
- **A program's one aggregation corresponds to `dsd_agg`** whatever its node is called, so its state carries over, and
  its telemetry keeps `component_id:dsd_agg`.

## 2. What a program binds

### 2.1 Every binding, classified

The registry is `bin/agent-data-plane/src/programmable_metrics/bindings.rs`, `BINDINGS`; each entry gains a class,
which the catalog carries (amendment 171). **Fact**: something true of the host or its resources that the program does
not choose. **Signal**: a live operational state the program reacts to. **Policy**: what to do with metrics, which a
published program owns.

| Binding | Liveness | Class | Why |
|---|---|---|---|
| `dogstatsd.mapper.profiles` | startup | policy | Rules that rename metrics and add tags: a program in itself. |
| `dogstatsd.mapper.cache_size`, `dogstatsd.mapper.string_interner_size_bytes` | startup | policy | Tuning of one node. Memory is bounded for artifact nodes anyway (`ARTIFACT_MEMORY`, 64 MiB). |
| `dogstatsd.prefix_filter.metric_namespace`, `.metric_namespace_blocklist` | startup | policy | Name rewriting and its exclusions. |
| `dogstatsd.metric_filter.values`, `.match_prefix` | live | policy | The metric filterlist. |
| `dogstatsd.post_aggregate_filter.values` | live, derived | policy | Derived from the filterlist and the histogram statistics; becomes one literal list (see [3.1](#31-where-detaching-happens)). |
| `dogstatsd.tag_filterlist`, `dogstatsd.tag_value_allowlist` | live, startup | policy | The tag filterlist and value rules. |
| `dogstatsd.aggregation.aggregator_tag_filter_cache_capacity` | startup | policy | Tuning of one node. |
| `dogstatsd.aggregation.window_duration_seconds`, `.flush_interval`, `.counter_expiry_seconds`, `.flush_open_windows` | startup | policy | Aggregation parameters, named by the owner as policy. |
| `dogstatsd.aggregation.context_limit` | startup | policy | The node's own limit. The host's **state budget**, also derived from `dogstatsd_context_limit` (`AdpCatalog::state_budget`), stays a host bound that no program raises; validation warns beyond it (`state_budget`). |
| `metrics_encoding.histogram.aggregates`, `.percentiles`, `.copy_to_distribution`, `.copy_to_distribution_prefix` | startup | policy | The statistics aggregation computes. The `metrics_encoding` prefix is the native key's; only aggregation and the derived filter read them. |
| `metrics_endpoint_routing.group.{key}.names`, `.prefixes` | live | policy | Routing allowlists, keyed by `routing_group_key`. Lowering-only: the detached export writes the lists, and step 2 writes the emit as recipients. |
| `multi_region_failover.enabled` | live | fact | Whether the failover recipient is configured: resource availability, not a choice. Redundant once an emit names `mrf`; kept so the export stays faithful ([9](#9-what-this-lets-us-remove)). |
| `multi_region_failover.metric_mirroring.enabled` | live | signal | The Datadog key `multi_region_failover.failover_metrics`: turned on during a regional failover, read live. This is the failover signal the owner named. |
| `multi_region_failover.metric_mirroring.allowlist` | live | policy | The mirroring list. |
| `autoscaling_failover.metrics` | live | policy | Which metrics the Cluster Agent receives: a filter list like the mirroring list. It is a deployment's choice, not a state the program observes. |
| `autoscaling_failover.enabled` (new) | live | signal | Whether autoscaling failover is on, the other signal the owner named. See [2.2](#22-new-bindings). |
| `host.tags` (new) | boot | fact | The host's tags, fetched from the Core Agent at boot. Today a literal in `metrics_enrich_host_tags`. |
| `host.tags_deadline` (new) | boot | fact | The process start plus `expected_tags_duration`: an absolute instant, meaningless as a literal in a program kept across restarts or sent to many hosts. `expected_tags_duration` describes when the Agent's intake adds host tags itself, a fact of the deployment. |

The hostname is not a binding: `host_enrichment` takes no parameters, since native host enrichment changes only events
and service checks. A program that needs it would bind a `host.name` fact; none does yet.

### 2.2 New bindings

- **`host.tags` and `host.tags_deadline`** (amendment 172). `host_tags`'s `tags` and `deadline` become bindable
  (`ParamType::Bindable`). Lowering binds them, resolved from `HostFacts`; their liveness is `Startup`. Fact names
  live under `host.`, outside the rule that binding names are typed-model paths.
- **`autoscaling_failover.enabled`** (amendment 180), live. Lowering adds `flag(config("autoscaling_failover.enabled"))`
  to `af_metrics_gateway`'s predicate. The branch exists only while autoscaling failover is requested, so the flag is
  always true in the default program; the detached export carries it, so a published program stops sending to the
  Cluster Agent when autoscaling failover turns off. For that to work without degrading the program, the
  `cluster_agent` recipient becomes available whenever forwarding to the Cluster Agent is configured (endpoint at boot,
  `cluster_agent.enabled`, a token), whether or not the branch is requested (today `cluster_agent_forwarder` also
  requires `is_branch_requested`). An available recipient that no output names runs no loop, so this costs nothing.

### 2.3 Where the native compatibility machinery goes

| Machinery | Where it goes |
|---|---|
| Policy bindings and their liveness (`Startup` or `Live`) | Lowering only: the default program keeps them; artifacts cannot bind them (step 3). |
| Hash-keyed routing-group bindings (`metrics_endpoint_routing.group.<16 hex>.*`, `BindingKey::RoutingGroup`) | Lowering only, never shown: the export writes the lists, and the emits as recipients. |
| Outputs shaped by native routing (`metrics_routing_encode_<n>`, `Selector::Group`, the main output sending to every endpoint without a routing policy) | Lowering only: artifacts emit to recipients; composition maps a recipient set to such an output only when the sets are equal. |
| Native authority bookkeeping (`compose.rs` `guards_authority`, `Feeds`) | Removed in phase 2, with node provenance ([open question 2](#12-open-questions-for-the-owner)). |
| Version 1 fingerprints and legacy recipient IDs | Removed ([9](#9-what-this-lets-us-remove)). |
| Base staleness (`stale_base`) | Replaced in status by which program runs and which native settings differ ([3.3](#33-status)); removed later. |

## 3. Step 1: the detached export

### 3.1 Where detaching happens

In the plan crate's export (`lib/saluki-metric-plan/src/export.rs`), as a form of the bundle (amendment 173):

- `export(plan, scope, catalog, labels, ExportForm::Detached)` writes every parameter bound to a binding whose catalog
  class is `policy` as the binding's resolved value, which the default program carries, and keeps fact and signal
  bindings bound and unresolved, as today. `ExportForm::Bound` is today's export, kept for tools and tests that study
  lowering.
- **Literal folding.** After writing literals, the detached export folds `is_empty(<literal>)` and `flag(<literal>)` to
  constants and simplifies the junctions they sit in (`x and true` is `x`, `x or true` is `true`, and so on). Nothing
  else is folded: `intake_name in []` stays, so an author can add names to an empty filterlist where it is. Without
  folding, the failover gateway would read `is_empty([]) or name in []`, with the mirroring list written twice, and an
  edit to one copy would silently disagree with the other.
- The bundle gains `form` (`detached` or `bound`). ADP's route serves `detached` by default and `bound` for
  `?form=bound`. `status.default_program_hash` is the detached export's base hash, so a client refetches when a native
  policy value changes, which is what an editor wants.
- The derived `dogstatsd.post_aggregate_filter.values` becomes one literal list: the filterlist entries that name a
  histogram statistic's series. Editing `dsd_agg`'s statistics does not update it; the editor's note on the node says
  so.

**Composition compares values, not their sources** (amendment 174). `same_operation` (`compose.rs`) compares a default
node and an artifact node after resolving the default node's bindings to their values and folding literals in both, as
the export does. So an unmodified detached export composes to the default program, with its hash (S0 holds), and its
nodes keep native provenance. When a native setting later changes, the default program is lowered again and the
artifact in effect composed again; the node's literal now differs, so it takes the artifact's provenance and keeps the
artifact's value: the program owns it. The rule is general (an operation is what it does with the values it has), so
it is not machinery for native compatibility.

### 3.2 How ADP knows an active program is detached

After step 3 every published program is detached, since none may bind a policy setting. Until then, ADP classifies the
artifact in effect: **detached** when no node binds a policy binding, else **bound**, naming the policy bindings it
binds. A bound artifact keeps following native values as today; the status says so.

### 3.3 Status

The poll's status and the status route gain `program` (additive, amendment 175):

```json
"program": {
  "runs": "published",
  "native_policy": "not_applied",
  "differs": ["dogstatsd.metric_filter.values"],
  "bound": []
}
```

- `runs` is `default` (no artifact: native policy settings apply, `native_policy` is `applied`) or `published`.
- `native_policy` is `applied`, `not_applied` (a detached program), or `partly_applied` (a bound one, listing its
  bindings in `bound`).
- `differs` lists the policy bindings whose current native value differs from what the published program does: for
  each default-program node bound to one, the node is absent from the artifact or its value differs. It is what an
  operator needs after changing a native setting while a program runs.

### 3.4 Native configuration changes while a program runs

They still reach the default program: the installer lowers it on every configuration change, the export follows it,
and **Load default program** shows the new values. They reach the published program only through facts, signals,
resources (recipients, credentials, delivery settings), and implied outputs, which composition resolves again. They do
not change its policy. An operator sees it three ways: `program.differs` in status, an `info` log line per change
("`metric_filterlist` changed; the published program (revision 7) owns its policy, so the change applies to the default
program only; revoke the program, or publish one with the new value, to apply it"), and in the demo a banner over the
editor. Standalone mode, which the demo runs, never applies configuration updates, so ADP's live-change tests cover
this, not the demo.

## 4. Step 2: recipient names

### 4.1 The directory

Lowering, the compatibility layer, names every recipient a program may address, and the catalog carries the
**directory** (amendment 176): name, recipient ID, delivery host, the endpoint as status spells it (`spelled_endpoint`,
never a credential), and its source (`native`, `config`, or `resources`). The export bundle carries it, and the editor
shows it. Recipient IDs stay positional and internal (`<host>/<route>/<key index>/<endpoint>`,
`resources.rs` `recipient_id`): retry queues, slots, and leases keep them. Names resolve at composition, as bindings do,
so the composed plan holds output IDs only, and runtime pays nothing for names.

| Recipient | Name |
|---|---|
| The main output's primary endpoint, or the metrics override that replaces it (Observability Pipelines Worker, Vector) | `primary` |
| The Multi-Region Failover endpoint (`mrf_dd_out`) | `mrf` |
| The Cluster Agent (`cluster_agent_out`) | `cluster_agent` |
| An additional endpoint's API key | derived from its URL: see below |
| A declared recipient, in configuration ([4.2](#42-recipients-declared-in-configuration)) or a resources artifact | the name its declaration gives |

> [!NOTE]
> As built, on the owner's decision (2026-10-14): an additional endpoint is named by its URL as the forwarder resolves
> it, with its scheme, which names every API key's recipient of that endpoint under every spelling of it, and there is
> no derived name and no collision rule. The rest of this section's naming rule for additional endpoints is
> superseded; see [C as built](#105-phase-1-as-built).

**Additional endpoints** are named from the host of the URL: `ep_`, then the host lowercased with every character
outside `[a-z0-9]` written as `_` (repeats collapsed), then `_<port>` for an explicit port other than the scheme's
default, then `_key<i>` for every API key after the first (`i` counting from 1). So `https://app.datadoghq.eu` with two
keys is `ep_app_datadoghq_eu` and `ep_app_datadoghq_eu_key1`. A name depends only on its own endpoint and key position,
so adding or removing another endpoint never renames it. Two spellings of one host and port, which native configuration
treats as two recipients, collide; each then gets `_` and the first 8 hexadecimal digits of the SHA-256 of its URL. A
name longer than an identifier allows (63 bytes) keeps its first 54 bytes and that digest. As with recipient IDs,
removing an earlier key renumbers the later ones.

**Reserved names.** `primary`, `mrf`, `cluster_agent`, and names starting `ep_` belong to lowering. A declaration that
uses one is refused, naming the reserved name. Two declarations of one name (configuration and a resources artifact)
refuse the resources artifact's revision, since configuration is the operator's own.

An optional label key for native endpoints (operator-chosen names for `additional_endpoints`) is left out: derived
names are stable, and the demo stops using additional endpoints. It can be added to the directory later without
changing programs.

### 4.2 Recipients declared in configuration

`experimental.programmable_metrics.resources.recipients` (Saluki-only, amendment 177) declares recipients as a resources
artifact does, with the same schema and validation (`resources::validate`) and credentials from
`experimental.programmable_metrics.resources.credentials`. It is live, as the endpoint keys are since stage 7. Lowering
merges it with the accepted resources artifact; identities follow the declared rule
(`<host>/declared/<credential name>/<canonical endpoint>`), and the source is `config`. This is the owner's "declared in
config": the operator's path to destinations that only programs route to, with stage 8 skipped.

### 4.3 The demo's teams: recommendation

**Declare teams B and C in configuration, and drop `additional_endpoints` and `experimental.metrics_endpoint_routing`
from the demo.** Reasons:

- **Honesty.** Today the demo's default program routes to B and C through native routing groups, so the most visible
  routing in the demo is native routing. With declared recipients, B and C receive only what a program emits to them,
  and the demo shows programs routing.
- **Readability.** The demo's default program loses two hash-keyed routing gateways and two positional outputs:
  its policy is one emit to `primary` and the failover gateway.
- **Native additional endpoints cannot stand in.** An additional endpoint without a routing policy receives everything
  (natively it is dual-shipping), so B and C would need endpoint routing again.
- **Equivalence loses nothing.** The equivalence of native routing is proven by the lowering matrix
  (`lowering/matrix.rs`), the differential tests against frozen native code (`lowering/legacy_tests.rs`), the plan
  crate's fixtures with `metrics_routing_filter_<n>`, and the live-change tests of the test topology, none of which use
  the demo.

The alternative, seeding the resources product with B and C at launch, needs no ADP change but ties B and C to team D's
lifecycle scenarios (S8 to S11 publish and revoke the whole resources set) and leaves them undeclared until the first
poll. Team D stays in the resources product, as the destination that arrives later.

## 5. Step 2: emits addressed to recipients

### 5.1 Syntax and IR

```text
node to_team_b = split(name starts_with "demo.checkout.") {
  true  -> emit to [team_b]
  false -> drop
}
```

- **IR** (amendment 178): `emit` takes exactly one of `output` (an output ID, as today) or `to` (a non-empty list of
  recipient names, identifiers). The syntax prints `emit to [a, b]` for `to`, and `emit <output>` for `output`.
- **One host per emit.** Every recipient of one `emit` must be on one delivery host, since an output is one encoder
  stream on one host. Otherwise composition refuses: "`emit to [primary, mrf]` names recipients of two delivery hosts
  (`primary` on `dd_out`, `mrf` on `mrf_dd_out`); emit to each from its own branch."
- **Output-addressed emits stay** for outputs without recipients, today only `dsd_client_telemetry`, the tap that the
  Agent's DogStatsD client telemetry needs, and for the default program, which lowering keeps writing as today.

### 5.2 From recipient sets to outputs

> [!NOTE]
> As built (phase 3, amendment 189): when some recipients an `emit` of the artifact in effect names are gone, it keeps
> delivering to those that remain, and counts what the others would have received as dropped; only when none remain is
> the `emit` a counted drop. See [the demo as built](#85-the-demo-as-built).

Composition resolves `to` before comparing nodes, as it resolves bindings:

1. Each name resolves through the directory to a recipient ID. A name the directory does not have makes a new artifact
   **wait** (`pending`, "waiting for recipient `team_z`"), as an undeclared output does today, so a route can be
   published before its resources (S10). For the artifact in effect, the `emit` becomes a counted drop with the
   artifact's provenance (degraded), as for a removed output.
2. The sorted set of IDs on its host is matched to an output with exactly that set: a native output first (the main
   output, a routing group's, the failover outputs), then an **implied output**. Matching uses the set alone, never
   the order or spelling of the names.
3. The node's `to` becomes `output: <id>`, and `same_operation` runs as usual. So the detached export's
   `emit to [primary]` composes to the default program's `emit dd_metrics_encode` when the main output's set is
   `{primary}`, and S0 still changes nothing.

**Implied outputs** (amendment 179). The installer core computes an artifact's **demand**, the recipient sets its emits
need (`compose::demanded_outputs(artifact, catalog)`, pure), for the artifact installing, the one in effect, and those
kept to roll back to. When demand names a set no output serves, the core returns `Action::InstallDefaultProgram`, and
ADP lowers with the demand, as it lowers with the accepted resources (`Lowering::lower_after(current, declared, demand,
previous)`). Lowering adds one output per set no output serves yet, which the resource transaction prepares (a lease on
the host's encode host, and the recipients), before the artifact composes. An implied output retires, its lease
draining, once no artifact in demand needs it, through the existing lease release.

- **ID and telemetry.** `to_` and the sorted names joined by `__` (`to_team_b`, `to_team_b__team_c`); when that exceeds
  63 bytes, `to_<first name>_and_<n>_<8 hex of the set's digest>`. Encoder telemetry reports under the ID, as for
  declared outputs.
- **Encoder settings from the host's template.** On `dd_out`, the main output's settings with the protocol variants its
  recipients accept, as declared outputs get today (`resources.rs` `declare`), generalized from declared recipients to
  any recipient on the host; on `mrf_dd_out`, the failover endpoint override; on `cluster_agent_out`, V2 series only,
  accepting counters, rates, gauges, and sets. Validation's port-type check then refuses distributions sent to
  `cluster_agent`, as it does for `af_metrics_encode`.
- **Fingerprint.** The declared rule, over host, accepts, and the recipient set (`Selector::Declared`). Artifacts record
  no fingerprint for a `to` emit: the names are the reference, and a renamed or re-declared recipient is followed by
  name, which is what declaring resources apart from programs is for.
- **Budget.** Implied and declared outputs share `MAX_DECLARED_OUTPUTS` (16). Demand beyond it refuses the artifact,
  naming the limit.

**Encoders are shared.** One output, so one encode, per distinct recipient set, however many branches emit to it, as
native routing groups share an encoder per endpoint set. A record two branches send to one set is a duplicate
contribution, refused as today. Nothing changes per metric: at 0.3 cores the cost that matters is encode and
compression, which this neither adds nor splits.

### 5.3 The default program's outputs, equivalence, and telemetry

Lowering is unchanged: the default program emits to `dd_metrics_encode`, `metrics_routing_encode_<n>`,
`mrf_metrics_encode`, and `af_metrics_encode`, with their native selectors, encoders, and telemetry, so the equivalence
oracle, the fixtures, and the native series hold. Only the detached export changes how emits print, as the recipient
sets those outputs have now. When native configuration later changes a native output's set (an endpoint added to the
main output), a published program that emits to the old set gets an implied output for it: its encoder telemetry moves
from `dd_metrics_encode` to `to_primary`, and the added endpoint receives nothing from that program. That is the
decision working as intended, and status shows it: the directory lists the endpoint, and no output of the published
program names it.

## 6. Step 3: facts and signals only

**Rule** (amendment 181): when the catalog declares `detached_programs` (a `MemoryCatalog` member and a `Catalog`
method, as `single_aggregation` is), composition refuses an artifact any node of which binds a binding whose class is
`policy`, kept nodes included (the rule is about what the author wrote, not about provenance). ADP's catalog declares
it; the plan crate's fixtures do not, so its tests of bound artifacts keep running.

- **Allowed:** fact and signal bindings; literals; recipient names; output IDs of outputs without recipients.
- **Message** (`ComposeError::PolicyBinding`, at the node): "`dsd_prefix_filter_blocklist` binds
  `dogstatsd.metric_filter.values`, a native policy setting. A published program owns its policy: write the value
  itself (now `[]`; **Load default program** writes current values). Only facts and signals can be bound: `host.tags`,
  `host.tags_deadline`, `multi_region_failover.enabled`, `multi_region_failover.metric_mirroring.enabled`,
  `autoscaling_failover.enabled`." The editor places it at the binding's span.
- **Order.** It lands after step 1, so that the export the editor loads already passes.

## 7. Step 4: one aggregation is the aggregation

(Version 2's step V10 removed this adoption: a renamed aggregation is a new lineage, at a cut; amendment 199.)

**Today.** Composition's `node_ids` (`compose.rs`) gives an artifact node its plan ID: the reserved ID when its local ID
is a member of the scope's region, else `policy/<local>`. The installer's diff (`install/diff.rs`) keys a state
owner's lineage by `LineageKey { node, contract }` (`compile/program.rs`), and `bind_lineages` retains state only for an
equal key. So a from-scratch program's `node agg = aggregate(...)` becomes `policy/agg`, a new lineage: a cut that
starts aggregation over, and series that report under another `component_id` than `dsd_agg`, which the Core Agent's
`aggregator.*` remaps expect.

**Change** (amendment 182), in `node_ids` only: when the catalog declares `single_aggregation`, the artifact has exactly
one state owner, and the region has exactly one, whose reserved ID no artifact node takes, the artifact's state owner
gets that reserved ID (`dsd_agg`). Everything after is unchanged: `same_operation` and the feed check decide its
provenance, the lineage key decides retain or cut from the lineage parameters (a new width is a cut, as today), and the
correspondence maps `dsd_agg` back to `agg`, so the diff and the editor speak of `agg`. With two state owners,
`single_aggregation` already refuses. The isolation slot, its series, and the `aggregator.*` remaps keep `dsd_agg`.

The other telemetry contract a from-scratch program can break is the DogStatsD client telemetry the Agent reads: a
program without `emit dsd_client_telemetry` stops it. The editor's note on that emit says so; nothing in the substrate
enforces it.

## 8. The editor and the demo

### 8.1 Less magic in what a user reads

- **Annotations instead of opaque names.** The export bundle gains `annotations`, for tools only: one line per
  default-program node from lowering ("Metric filterlist, from `metric_filterlist`"), per binding (its class and
  meaning), and per recipient (endpoint, host, source). The editor prints a node's note as a `#` line
  before it, and the directory as a `# Recipients:` block before the policy, listing recipients no branch addresses too.
  Comments are not part of the artifact.
- **`base` hidden**, as fingerprints are: `PrintOptions::elide_base` leaves out the `base` line, and parsing takes a
  base from the caller when the text has none (`ParseOptions::base`), so diagnostics keep their lines. The editor
  passes the current export's base. A detached program's base only reports staleness.
- **Readable values**: literals instead of `config("...")`, recipient names instead of `metrics_routing_encode_<n>`, and
  folded constants ([3.1](#31-where-detaching-happens)). The header names what runs (**the default program**, or
  **published revision 3**), whether native policy settings apply, and how many differ.

### 8.2 The demo's initial program

> [!NOTE]
> As built later, the detached export leaves out what does nothing as configured: here the mapper, the namespace
> prefix, the metric filterlist, the tag filterlist, the post-aggregation filter, the one-input merge, and host
> enrichment, which a comment at the top names. See [what does nothing, as built](#107-what-does-nothing-as-built).

ADP boots with no artifact, so the editor loads the default program, detached. Abbreviated (`…`):

```text
policy metrics {
  # Sources: where each source's metrics enter
  input.dogstatsd -> dsd_enrich

  # DogStatsD processing
  # DogStatsD mapper: no profiles configured
  node dsd_enrich = dogstatsd_mapper(
    profiles: [],
    cache_size: 1000,
    string_interner_bytes: …,
  ) {
    -> dsd_prefix_filter
  }

  # Namespace prefix, from statsd_metric_namespace and its blocklist
  node dsd_prefix_filter = namespace_prefix(
    namespace: "",
    exclusions: ["datadog.agent", "datadog.dogstatsd", …],
  ) {
    -> dsd_prefix_filter_blocklist
  }

  # Metric filterlist, from metric_filterlist: empty, so it drops nothing
  node dsd_prefix_filter_blocklist = split(intake_name in []) {
    true  -> drop
    false -> dsd_no_agg_split
  }

  # Timestamped points skip aggregation; the window must equal dsd_agg's
  node dsd_no_agg_split = timestamp_split(window: 1s) {
    default     -> dsd_tag_filterlist
    passthrough -> dsd_post_agg_filter_in.1
  }

  # Tag filterlist, from metric_tag_filterlist: empty
  node dsd_tag_filterlist = tag_projection(
    rules: [],
    value_rules: [],
    cache_capacity: …,
  ) {
    -> dsd_agg
  }

  # Aggregation: 1 s buckets, counters never expire
  node dsd_agg = aggregate(
    window: 1s,
    flush_interval: 1s,
    context_limit: …,
    counter_expiry: 0s,
    flush_open_windows: false,
    histogram_aggregates: ["max", "median", "avg", "count"],
    histogram_percentiles: [95],
    copy_to_distribution: false,
    copy_to_distribution_prefix: "",
  ) {
    -> dsd_post_agg_filter_in.0
  }

  # After aggregation
  node dsd_post_agg_filter_in = merge(inputs: 2) {
    -> dsd_post_agg_filter
  }

  # Filterlist entries naming histogram statistics: empty
  node dsd_post_agg_filter = split(
    kind in [counter, rate, gauge, set] and intake_name in []
  ) {
    true  -> drop
    false -> dsd_post_agg_filter_fanout
  }

  # DogStatsD client telemetry for the Agent; everything on to enrichment
  node dsd_post_agg_filter_fanout = tee {
    -> emit dsd_client_telemetry
    -> metrics_enrich_in.0
  }

  # Enrichment
  node metrics_enrich_in = merge(inputs: 1) {
    -> metrics_enrich
  }

  # Host enrichment: leaves metrics unchanged
  node metrics_enrich = host_enrichment {
    -> metrics_policy
  }

  # Recipients:
  #   primary  http://127.0.0.1:17101  (dd_url)
  #   mrf      http://127.0.0.1:17104  (multi_region_failover.dd_url)
  #   team_b   http://127.0.0.1:17102  (configuration; not addressed)
  #   team_c   http://127.0.0.1:17103  (configuration; not addressed)
  # Policy: one branch per destination; route here
  node metrics_policy = tee {
    -> emit to [primary]
    -> mrf_metrics_gateway
  }

  # Multi-Region Failover: mirrors every metric while failover is on (a signal)
  node mrf_metrics_gateway = split(
    flag(config("multi_region_failover.enabled"))
    and flag(config("multi_region_failover.metric_mirroring.enabled"))
  ) {
    true  -> emit to [mrf]
    false -> drop
  }
}
```

The policy, today four branches with two hash-keyed gateways, is two lines and a gateway. What is still there for
native reasons is labelled: the one-input merge and the client telemetry tap (IDs and telemetry that native components
fix), and `host_enrichment`.

### 8.3 A visible change in about 30 seconds

> [!NOTE]
> As built (phase 3), teams B and C are ordinary additional endpoints, which the default program dual-ships to, so the
> recipe narrows team B rather than adding it: see [the demo as built](#85-the-demo-as-built).

1. The editor shows the preceding program; the header reads "Running: the default program"; the team B and team C
   columns are empty, labelled `declared, not addressed`.
2. Add a branch to the policy `tee` and its gateway:

   ```text
   node metrics_policy = tee {
     -> emit to [primary]
     -> to_team_b
     -> mrf_metrics_gateway
   }

   node to_team_b = split(name starts_with "demo.checkout.") {
     true  -> emit to [team_b]
     false -> drop
   }
   ```

3. **Compile.** The diff lists the added nodes, the `tee`'s new port, and one new output, `to_team_b` (`team_b`,
   `http://127.0.0.1:17102`). **Publish** and approve.
4. Within a few seconds `demo.checkout.*` appears in team B's column, still in primary's; the header reads "Running:
   published revision 1 (native policy settings not applied)".

Other edits: drop `demo.debug.*` with a split ahead of the `tee` (today's recipe); make team B's route exclusive by
moving its split ahead of the `tee`; mirror only checkout with `and name in ["demo.checkout.requests"]` on the failover
gateway; rename `dsd_agg` to `agg`, or write the program from scratch, and see the state kept (no cut).

### 8.4 Scenarios and the headless check

> [!NOTE]
> As built (phase 3), without resources artifacts and with destinations added and removed live: see [the demo as
> built](#85-the-demo-as-built).

| Step | Change |
|---|---|
| S0 | Unchanged in what it proves, on the detached export: `active`, no new generation, an empty diff; `/export?form=detached` and `/default-program` hash as expected. |
| S1 | Unchanged (`drop_debug`). |
| S2 | New: route `demo.checkout.*` to `team_b` (the recipe): an implied output `to_team_b` appears, its lease in the encode host `dd_metrics_encode`. |
| S3 | Today's S2 and S3 on the new route: widen it with `or tag("team") == "platform"`, then make it exclusive. |
| S4 | The failover gateway gains `and name in ["demo.checkout.requests"]` (was a replaced predicate over a binding). |
| S5 | S5a, S5c to S5e unchanged; S5b becomes `emit to [primary, mrf]` (two hosts); new S5f binds `config("dogstatsd.metric_filter.values")` (`policy_binding`, placed at the binding). |
| S6 | Unchanged: revoke, and team B's column empties. |
| S8 to S11 | Team D's resources declare only a recipient (no `outputs` member); S8b emits `to [team_d]`; S10 waits for recipient `team_d`. |
| S12 to S14 | Unchanged; S14 routes `to [team_d]`. |
| S15 | New, second launch: publish the default program with `dsd_agg` renamed `agg`; no cut (`plan_cuts_total` unchanged), canaries balanced, `component_id:dsd_agg` series continue. |

The launcher's configuration loses `additional_endpoints` and `experimental.metrics_endpoint_routing`, and declares
`team_b` and `team_c` under `experimental.programmable_metrics.resources.recipients` with credentials `team_b` and
`team_c`. The demo's export fixture (`fixtures/export-metrics.json`) is regenerated from the new configuration.

### 8.5 The demo as built

Phase 3 built part G2 with the owner's decisions of 2026-10-14, which replace [4.3](#43-the-demos-teams-recommendation)
and parts of [8.3](#83-a-visible-change-in-about-30-seconds) and [8.4](#84-scenarios-and-the-headless-check):

- **Destinations are ordinary `additional_endpoints`.** The launcher writes the primary endpoint, teams B and C as
  additional endpoints, and the failover endpoint, with no `experimental.metrics_endpoint_routing` and no resources
  credentials, and turns `experimental.standalone_config_reload` on (250 ms). The default program dual-ships: its main
  `emit` is `emit to [primary, "http://127.0.0.1:17102", "http://127.0.0.1:17103"]`, the URLs as ADP's directory
  names the endpoints (scheme kept, no root path). The demo keeps a label per URL, which ADP never sees, and prints it
  beside the URL in the editor's `# Recipients:` comments (display only: the artifact and its hash are unchanged).
- **Add destination and Remove.** There is no reserved team D. **Add destination** starts a recording intake on a port
  the system picks, with an API key of its own and a label (`Destination D`, `E`, and so on), and rewrites the run's
  `datadog.yaml` atomically with it under `additional_endpoints`; ADP reads it within about a second. **Remove** takes
  an additional endpoint out of the file: ADP drains its recipient, the intake answers until ADP lists the recipient
  retired (at most 60 s), and the column goes. The demo's server, `destinations.rs`, owns the intakes and the live part
  of the configuration (the destinations, `metric_filterlist`, and `multi_region_failover.failover_metrics`, which the
  **Native configuration** card of the UI sets).
- **Detached semantics, visible.** Under the default program a new destination receives everything; under a published
  program its column reads "declared, not addressed" (ADP's directory names it, and no `emit` of the active plan
  reaches it) until the program routes to it. The header names what runs from the status's `program` member, and a
  native policy change under a published program shows in `program.differs`.
- **The editor and implied outputs.** A program that emits to a recipient set no output delivers to yet cannot be
  composed in the editor, which composes against ADP's current default program: the editor reports ADP's waiting
  reason as a warning ("Waiting for an output to ..."), not an error, with no plan diff, and says that ADP adds the
  output. Once ADP runs the program, compiling it again shows the plan ADP runs. An unknown recipient is a warning too:
  ADP holds the program pending.
- **No resources artifacts in the demo.** The Resources tab, its routes, and the team D template are gone; the stub
  still serves the resources product, which ADP polls, and the plan crate's and ADP's resources code and tests stay.
- **The scenarios** are those of [8.4](#84-scenarios-and-the-headless-check) without resources, and with destinations
  and native changes: S0 adopt; S1 drop debug; S2 the recipe (implied outputs); S3a and S3b widen and make exclusive;
  S4a to S4c the failover gateway narrowed, then the failover signal on and off in the file; S5a to S5f the guardrails
  (duplicate, two hosts, second aggregation, aggregation after enrichment, policy binding, unknown recipient, the last
  `pending`); S6 revoke; S8a and S8b a destination added and removed under the default program; S9a to S9c the recipe
  again, a destination added under it (declared, not addressed), then routed to; S10 that destination removed while
  routed to (the program stays `active`, degraded, with policy loss at the route); S11a to S11c the filterlist set
  under the program (no new generation, `program.differs`), revoked (the default program applies it), and cleared;
  the second launch's S12 and S13 as before, and S14a and S14b the aggregation renamed (adopted; since amendment 191 a generation with no cut), then renamed with a
  routing change (a generation, no cut). The status document's [demo section](programmable-metrics-status.md#the-demo)
  has the table and the run times.
- **After the review (amendments 189 and 190).**
  - **An `emit` delivers to the recipients that remain** (amendment 189, decided on the owner's behalf as least
    surprising). Before, removing team C under the recipe made the whole `emit to [primary, "...17103"]` a counted
    drop: the primary endpoint went empty. Now the `emit` of the artifact in effect keeps delivering to the recipients
    that remain, through the output with exactly them, and composition makes it a two-port `tee` to an `emit` of
    those (`policy/<emit>_delivered`) and a counted drop (`policy/<emit>_missing`), which counts as policy loss what
    the gone recipients would have received; the acknowledgment names them (`the emits to ["..."] are dropped until
    they are available again: the host has no recipient ...; the emit still delivers to [primary]`). Only when none
    remain is the `emit` a counted drop. A new artifact still waits for every name. `demanded_outputs` asks for the
    set that remains, and ADP lowers twice when that changes: once with the demand as the old directory resolves it,
    and, if the new directory resolves it otherwise, once more before proposing, so the output for the remaining set
    exists in the same generation that removes the recipient, and nothing the primary endpoint receives is lost (an
    ADP delivery test removes an endpoint under such a program and counts every batch at the primary endpoint; it fails
    without the second lowering). Only the policy region changes. The headless check's S10b does the same live.
  - **The editor validates programs that need implied outputs.** It composes against stand-ins, `adp_will_add_<n>`,
    one per recipient set on the primary's host that no output delivers to exactly (from `demanded_outputs`), copies
    of the host's output with those recipients, so it reports ADP's findings and a provisional diff before publishing,
    marked as such, with a note per set ("ADP will add an output for Primary and Team C"). A program ADP will reject,
    such as the recipe with a duplicate branch, now shows the error. The plan hash differs from ADP's, whose outputs
    have ADP's own settings; compiling again once ADP runs the program shows the exact plan.
  - **Recompiling the program ADP runs** composes it in effect, as ADP does, so a degraded program reads degraded, not
    pending.
  - **A refused newer delivery stays its config's acknowledgment** (amendment 190), whether refused at composition or
    rejected at preparation, until the installation before it changes state.
  - The editor's recipient comment lists every recipient with the demo's label, in aligned columns, primary and
    failover first; an unused native output that a program replaces with implied outputs is a note, not a warning; a
    note beside the editor names destinations ADP declares that the published program does not address.
  - A removed destination's intake stops only after a status ADP sent two reload intervals after the file was
    written; a new destination never takes a port an earlier one had in the run; the intake listens before the file
    names it.
  - Found and not fixed: the duplicate analysis still reports a duplicate contribution for copies that reach, through
    two branches of a `tee`, two outputs with no recipient in common, when a third output on the other side of a
    `split` shares a recipient with each (amendment 187 covers outputs reached from one place only). The demo's own
    programs do not meet it.
- **Found on the way.**
  - A program that asks for implied outputs and is then rejected (a duplicate contribution, say) was never
    acknowledged `rejected`: its demand gone, ADP lowered again, the artifact in effect before it moved to a new
    generation, and the installer core reported that change as the config's acknowledgment in place of the rejection.
    A composition refusal followed by any configuration change lost its acknowledgment the same way. The core now
    keeps a newer delivery's refusal, rejection, or failure as the acknowledgment while the installation before it only
    moves to another generation, and still reports a change of that installation's state (`refresh_acknowledgments`,
    with two lifecycle cases in `install/artifact_tests.rs`).
  - A step that replaces a program with implied outputs, or revokes one, makes ADP lower twice (once at once, and once
    when the old demand retires), so the plan ADP reports at the acknowledgment can be the first of the two. The check
    compiles the program again and compares with the plan ADP runs then.
  - Revoking a published program whose only change is a split the signals decide as the default program's does (the
    failover gateway narrowed, with failover off) adopts the running generation: plans are compared with their
    bindings written as values. The check accepts adoption or a new generation on a revoke.
  - Other endpoints than the primary one now receive canaries (dual shipping), so the check's rule changed: from no
    canary elsewhere, to never more than were sent.

## 9. What this lets us remove

| Simplification | When | Risk |
|---|---|---|
| Version 1 fingerprints and legacy recipient IDs: `legacy_fingerprint`, `legacy_recipient_id`, `Approval::upgraded`, `ScopeComposition::legacy_outputs`, the version 1 path of `check_output` | Now: nothing is released, and only old demo run directories hold such artifacts | Low: an old run directory's artifact is refused as rebound ("Rebase the artifact on the current export"), or runs degraded in effect, until republished. |
| Declared outputs in resources artifacts (`outputs`, `resources_<name>`) | After step 2 and the demo moves | Low: a resources format bump; recipient emits cover every use. |
| `multi_region_failover.enabled` in the failover gateway | Any time | Low: one fixture hash per configuration with failover. |
| Output fingerprints in artifacts, `ReboundOutput`, renumbering detection (`renumbered_routing_groups`, amendment 129) | Once authored emits are all addressed to recipients (after step 3) | Low: only output-addressed emits need them, and only the default program and the client telemetry tap have those. |
| `stale_base` in status and `base.program_hash` | After step 1's `program` member has a release behind it | Low to moderate: a status member removed from the stub protocol. |
| **Policy bindings in the default program**: lowering writes policy values as literals, labelled with their native setting; bindings remain only for facts and signals. Removes the detached export's conversion and 3.1's comparison of values, keyed bindings (`BindingKey`, `Resolve::Keyed`), and most of the catalog's bindings | After step 3; the next simplification to take | Moderate: every default-program fixture, export, and explain golden changes; the lowering matrix's binding checks are rewritten; R3 may then fold a split whose literal is constant, so a live change may change which nodes a compiled program keeps (harmless in a fixed frame, but the "structure never depends on configuration values" rule is restated). |
| Native authority bookkeeping (`guards_authority`, `Feeds`): every node of a published program has the program's provenance | Waits on [open question 2](#12-open-questions-for-the-owner) | Moderate: loss attribution, panic containment, and the artifact limits then cover the whole program; S0 would make a generation. |
| Role and group selectors for native outputs (`Selector::Role`, `Selector::Group`) | Waits: they keep a native output's lease when an endpoint joins its role | Moderate: an endpoint added live would replace the main output's lease. |
| Native lowering itself | Waits on N1 and on replacing native | High: the compatibility front end is what proves equivalence. |

## 10. Implementation plan

### 10.1 Parts

| Part | What | Main files | Depends on |
|---|---|---|---|
| **A** Binding classes and facts | `BindingClass` in the plan crate (`CatalogBinding.class`, the catalog document's `class`, `policy` when absent); classes in `BINDINGS`; `host.tags`, `host.tags_deadline` (bindable `host_tags` parameters); `autoscaling_failover.enabled` and its flag; `cluster_agent` availability apart from the branch request | `validate/catalog.rs`, `operators/enrich.rs`, `bindings.rs`, `lowering/mod.rs`, `resources.rs`, `catalog.rs` | none |
| **B** Detached export and status | `ExportForm`, literal folding, `same_operation` on values, the route's `form`, `status.program`, the log line | `export.rs`, `predicate/canonical.rs`, `compose.rs`, `api.rs`, `installer.rs`, the stub protocol | A's interface |
| **C** Recipient directory | Names, reserved names, the catalog's and the export's directory; configuration-declared recipients (C2) | `resources.rs`, `catalog.rs`, `validate/catalog.rs`, `resources/mod.rs`, configuration schema and overlay | none (C2 needs the config-system skill) |
| **D** Recipient emits | `emit`'s `to`; syntax `emit to [...]`; composition's resolution; demand, implied outputs, retirement; the detached export prints recipient sets | `operators/routing.rs`, syntax `parser.rs`, `printer.rs`, `compose.rs`, `install/core.rs`, `install/resources.rs`, `lowering/mod.rs`, `resources.rs` | C's interface |
| **E** Facts and signals only | `detached_programs` in the catalog, `ComposeError::PolicyBinding` | `compose.rs`, `validate/catalog.rs`, `catalog.rs` | A; lands after B |
| **F** One aggregation | `node_ids` under `single_aggregation` | `compose.rs` | none |
| **G** Editor and demo | G1 annotations and `elide_base` (from the start); G2 the demo's configuration, scenarios, and check (after B to F) | demo `compile.rs`, `export.rs`, `ui/`, `launcher/config.rs`, `scenario/`; syntax `lib.rs`; the annotations in lowering | G1: none; G2: all |
| **H** Removals | Version 1 fingerprints | `resources.rs`, `compose.rs`, `install/core/artifacts.rs` | none |

**Order.** First wave, in parallel, each in its own Git work tree: A, C, F, G1, H. Second: B (on A), D (on C). Third: E
(on A, after B). Last: G2 on everything. B and D both touch `compose.rs` (B the comparison, D the resolution step before
it), so D rebases on B, or they agree the order of the two passes first: resolve names, resolve or compare values, then
`same_operation`.

### 10.2 Interfaces to agree first

```rust
// saluki-metric-plan::validate
pub enum BindingClass { Fact, Signal, Policy }
pub struct CatalogBinding { /* today's members */ pub class: BindingClass }
pub struct CatalogRecipient { pub id: RecipientId, pub host: HostId, pub endpoint: String, pub source: RecipientSource }
pub trait Catalog {
    // ...
    fn recipient(&self, name: &RecipientName) -> Option<&CatalogRecipient>;
    fn output_for(&self, host: &HostId, recipients: &[RecipientId]) -> Option<OutputId>;
    fn detached_programs(&self) -> bool { false }
}

// saluki-metric-plan::export
pub enum ExportForm { Bound, Detached }

// saluki-metric-plan::compose
pub fn demanded_outputs(artifact: &PolicyArtifact, catalog: &dyn Catalog) -> Result<Vec<Demand>, ComposeError>;
pub struct Demand { pub host: HostId, pub recipients: Vec<RecipientId> }   // sorted, deduplicated

// agent-data-plane lowering: `lower_after` gains the demand.
// fn lower_after(&self, current, declared, demand: &[Demand], previous) -> Result<Lowered, GenericError>
```

`RecipientName` is an identifier type in `ir/id.rs`. The status `program` member and the export's `form`,
`annotations`, and `recipients` are additive in the stub protocol.

As built in phase 1, `CatalogRecipient` holds `recipients: Vec<RecipientId>` rather than one `id`, since a URL names
every key's recipient of an endpoint, and `RecipientName` is an opaque reference (printable ASCII) rather than an
identifier, since a URL is a name. `Catalog::reserves_recipient_name` was added for the reserved names.
`output_for`, `detached_programs`, `ExportForm`, `Demand`, and `demanded_outputs` are left to the parts that use them.

**Composition pass order** (agreed for B and D). `compose_scope` (`compose.rs`) runs, per artifact:

1. Inline blobs, once, into every node's parameters (phase 1 moved this ahead of ID assignment).
2. **E (facts and signals only):** refuse a node, kept or not, that binds a `policy` binding (`policy_binding`). It
   judges what the author wrote, so it runs before anything is resolved.
3. Assign plan IDs (`node_ids`), with F's rule for the one aggregation.
4. **D (resolve names):** each `emit` with `to` resolves through the directory to its recipient IDs, the set must lie on
   one delivery host, and `Catalog::output_for` gives the output with exactly that set; the node's parameters become
   `{"output": <id>}`, marked as addressed by name so that step 8 checks no fingerprint for it. An unknown name holds a
   new artifact pending, and drops the emit of the artifact in effect.
5. **B (compare values):** `same_operation` compares the default node, its bindings resolved to their values, with the
   artifact node, and folds `flag` and `is_empty` over literals in both.
6. Provenance: kept, modified, or added, with the feed and authority checks as today.
7. Resolve the bindings of artifact-provenance nodes from the catalog (facts and signals only, after E).
8. `check_output` on output-addressed emits: fingerprints, pending, rebound, and drops, as today.

Then the plan is spliced and validated once. D's step 4 comes before B's step 5 so that `emit to [primary]` compares
equal to `emit dd_metrics_encode`; both come after step 3 because comparison is keyed by plan ID.

### 10.3 Tests and exit evidence

| Part | Tests | Exit |
|---|---|---|
| A | Every `BINDINGS` entry has a class (a test lists them, so a new binding must choose); `host.tags` resolves from `HostFacts`; the AF gateway's flag; `cluster_agent` available without the branch, with no recipient loop until an output names it | Equivalence, the lowering matrix, and the legacy differential tests pass with the new hashes |
| B | Detached export of every fixture: no policy binding left, facts and signals bound; folding preserves the interpreter's result (a property test over random predicates and literals); the unmodified detached export composes to the default program's hash; a native change after publishing makes the node take the artifact's value; `program.differs` | S0 on the detached export |
| C | Name derivation (ports, keys, collisions, length); reserved names refused; configuration and resources declarations merged and colliding; the directory never holds a credential | The demo's configuration lowers with `team_b` and `team_c` |
| D | Parse and print `emit to`; two hosts refused; set matching ignores order; an implied output prepared, reused by a second branch, and retired with its lease drained; pending on an unknown name; the 16-output budget; a continuous-input test across adding an implied output, with no loss or duplication | The recipe's edit end to end in the test topology |
| E | A bound policy setting refused, kept nodes included; facts and signals accepted; the fixtures' catalog unaffected | S5f |
| F | A renamed aggregation retains (no cut) with equal lineage parameters and cuts with a new width; correspondence maps back; two state owners still refused | S15 |
| G | The editor's printed program pinned as a golden, every line at most 80 characters; base restored from the export; annotations present | `make demo-programmable-metrics-check` passes both launches |
| H | An artifact recording a version 1 fingerprint is refused as rebound, and runs degraded in effect | Plan crate and ADP tests pass |

### 10.4 Hashes and fixtures that change

- **A:** `default-program.json` and `default-program-no-aggregation-pipeline-off.json` (host tags and the AF flag),
  their exports in `fixtures/export/`, the explain golden files in `fixtures/explain/`, ADP's lowering fixture
  `lowering/fixtures/every-branch.json`, and the checked-in JSON schemas (`host_tags`'s bindable parameters, the
  catalog's `class`). Hashes are computed with Python, as before.
- **B:** a new golden, `fixtures/export/default-program-metrics-detached.json`; no default program changes.
- **C, D, E:** the catalog document (`recipients`, `detached_programs`) and the `emit` schema; the syntax crate's
  grammar tests; no default program changes, since lowering keeps output-addressed emits.
- **F:** none. **G:** the demo's export fixture and the editor's golden source. **H:** tests only.

### 10.5 Phase 1 as built

Phase 1 built parts A, C (without C2), F, G1, and H, in the main checkout, not committed.

- **A (binding classes and facts).** `BindingClass` (`validate/catalog.rs`) on `CatalogBinding.class`, written always
  and read as `policy` when absent; `MemoryCatalog::with_classified_binding`. Every `BINDINGS` entry
  (`bindings.rs`) has `class` and `about` (one sentence naming the native setting), listed by the test
  `every_binding_is_a_fact_a_signal_or_a_policy_setting`. `host.tags` and `host.tags_deadline` resolve through a new
  `Resolve::Fact` from `HostFacts`, which `Snapshots` now carries; `host_tags`'s parameters are bindable and lowering
  binds them. `autoscaling_failover.enabled` (signal, live) sits in `af_metrics_gateway`'s predicate **after** the kind
  test, not first, since R7's static order ranks a kind test (cost 2) before a flag (3); writing it first would make R7
  rewrite the default program. The `cluster_agent` recipient's availability apart from the branch is a directory entry
  only: the plan declares the recipient only while an output names it, as before, because a declared native recipient
  runs a loop whether or not an output names it. D adds the output, and with it the recipient.
- **C (the directory).** `NativeResources::directory` (`resources.rs`), carried by `AdpCatalog` and its document
  (`MemoryCatalog`'s `recipients`), so the export bundle has it. Names: `primary` (the main output's primary endpoint,
  or the metrics override), `mrf`, `cluster_agent`, each additional endpoint by its URL as the forwarder resolves and
  sends to it (`additional_endpoint_name`, one function, over `saluki-components`'s new `resolved_endpoint`): with its
  scheme, `https` for an endpoint configured without one, and without a root path, so the spellings of one endpoint
  share one name and its recipients; a URL that is not plain printable ASCII, has `@`, or is too long is named
  `sha256:` and 16 hexadecimal digits, as status spells it. A native name for an additional endpoint therefore always
  has a scheme, never equals `primary`, `mrf`, `cluster_agent`, or a declared name (an identifier), and never spans two
  hosts: lowering fails, as a bug, rather than merge two hosts or two sources under one name. Resources validation
  refuses a name the catalog reserves (`primary`, `mrf`, `cluster_agent`; ADP's catalog and the demo's Resources
  check both reserve them) or one the directory gives a recipient declared other than by a resources artifact, which
  is the hook C2 needs whichever way the owner decides it. Readers of the export keep only the members of a directory
  entry they know and leave out an entry from a source they do not know; the demo leaves out any entry that does not
  read, so a newer directory costs it only comments.
- **F (one aggregation).** `the_aggregation` and `node_ids` (`compose.rs`), as designed. Tested in
  `install::unified_scope_tests`: renamed and unmodified composes to the default program's hash with no cut; renamed
  and widened is a cut; without `single_aggregation`, `policy/agg`; with two, neither is mapped.
- **G1 (annotations and the base).** The export bundle's `annotations` (`export.rs`, `Annotations { nodes, bindings }`);
  lowering writes node notes (`lowering/mod.rs` `ANNOTATIONS`, routing filters with their endpoints), and the installer
  adds them and each binding's class and meaning to every export. Recipient notes are not annotations: the editor
  prints the directory itself, so nothing is said twice. The syntax crate gained `PrintOptions::elide_base` and
  `parse_with(source, &ParseOptions { base })`. The demo's editor prints without the base, prints each node's note
  above it and the directory above the policy `tee` (`compile.rs` `annotate`, which replaced the hard-coded section
  comments), and parses with the export's program hash. `Parsed::base_given` says whether the base came from the
  caller: formatting hides only such a base, and keeps one the source states. The hidden base is the program hash of
  the export current when the source is compiled, so a source the editor printed never warns about a stale base; a
  stale base shows only when the author writes one.
- **Installer core fixes**, found by the reviewed property test
  `property_test_the_high_water_mark_never_decreases_and_only_accepted_plans_run` (both before phase 1): a config held
  unchanged beside a damaged twin of its ID in the previous snapshot is read anew, since configs are tracked by ID,
  so the twin's refusal no longer hides it from the rule of one config per scope; and a kept (last-known-good)
  generation that supervision fails takes its documents out of effect under every revision, as the active one's
  failure does, so a newer revision of the same document in flight is dropped rather than installed. Each has a
  regression test in `install/artifact_tests.rs` and a seed in the test's regressions file.
- **H (removals).** `CatalogOutput::legacy_fingerprint`, `NativeOutput::legacy_fingerprint`, `legacy_recipient_id`,
  `Approval::upgraded`, `ScopeComposition::legacy_outputs`, `OutputCheck::Legacy`, and the installer's `upgraded`
  memory. Simpler than designed: there is no dedicated refusal message. An artifact recording a version 1 fingerprint
  is an artifact whose output delivers elsewhere: a new one is refused with `ReboundOutput` ("Rebase the artifact on
  the current export"), and one in effect runs degraded until republished.

Notes for phase 2 (taken up in [phase 2 as built](#106-phase-2-as-built)):

- **D:** implied output IDs `to_<a>__<b>` are not identifiers (no double underscores), and URL names cannot be part
  of an ID at all: derive implied output IDs from the set's digest, or from a short label. The syntax needs a quoted
  form for URL names, as in `emit to ["http://127.0.0.1:17102"]`.

### 10.6 Phase 2 as built

Phase 2 built parts B, D, and E, and the owner's decisions made after this design: nodes have no provenance, a
published program equal to the running plan adopts its generation, and standalone mode can reload its configuration
file for the demo. It is in the main checkout, not committed. Amendments 183 to 188 record the changes to the model.

**Ownership removed** (owner's decision; amendment 183). `Node.provenance` and `ir::Provenance` are gone; what a plan
runs is a property of the plan. `PlanProvenance.artifacts` lists each composed artifact with the scope it replaces
(`ComposedArtifact { scope, revision, hash }`), and the program of an artifact is every node of that scope's region
(`export::program_nodes`, over `export::scope_members`), whether its author changed a node or kept it.

- **What replaced each use.** The artifact limits, the `state_budget` warning, and placement's attribution of missing
  structure apply to the program's nodes. Since a program holds every node of its scope, the limits rise to what the
  default program has there (amendment 188, below). `trusted_host` is a property of the operator,
  `OperatorDef::authorable` (`false` for `source` and `opaque_host`), checked on the program's nodes: a scope that holds
  a host component cannot be replaced at all, even by its unmodified export (the plan crate's opaque-gateway fixtures,
  never ADP's). A panic in a generation counts against the budget of the program it runs, in every region
  (`PanicBudget::for_origin`; the driver's `with_artifact_nodes` and the per-node coverage are gone). Loss is attributed
  to its node ID, and the program is the plan's provenance, which explain, now version 2, prints with its scope.
- **Removed:** the rules `loss_authority`, `provenance_consistency`, and `provenance_namespace`; composition's
  `guards_authority` and its `Feeds` and `Stream` analysis, so a node fed other records than natively is still the
  default program's node when it does what that node does; the `provenance` aspect of plan diffs; and node provenance
  in `explain`.
- **Native components, a deviation.** R1's cost model prices native components, and ADP sizes a region's input by the
  native components it replaces; both read, at boot, which native component each node stands for, which node
  provenance carried. That is now `PlanProvenance.native_components` (a node lowering lists no component for is a
  component of its own), which lowering writes, composition does not carry, and the hash leaves out, as all
  plan-level provenance. It decides no ownership, loss, or reuse. The alternative, a host input to `Frame::compute`,
  would have kept it out of the IR at the cost of every frame test.
- **Native telemetry identity** still follows the reserved node ID and the operator (`telemetry_profiles.rs`), and
  R5a and R6 now keep native filters whole by their reserved IDs, which is where native series report. The lowering
  matrix test checks every lowered node's identity against its component.
- **Hashes.** Node provenance was hashed, so every plan hash, fixture, and explain golden changed once: the plan crate's
  `default-program.json` and `default-program-no-aggregation-pipeline-off.json` (provenance moved to
  `native_components`), ADP's `every-branch.json`, the explain golden files (regenerated with
  `SALUKI_METRIC_PLAN_UPDATE_EXPLAIN`), the JSON schemas (`SALUKI_METRIC_PLAN_UPDATE_SCHEMA`), and one hand-computed
  hash in `ir/plan.rs`. Export hashes did not change: exports never carried provenance.

**Precise updates and adoption** (owner's decision; amendment 184). Reuse is decided by operators, parameters, and
edges only, compared by what they do: `export::operation_params` writes every binding as its value and folds literal
predicates, and `export::operation_hash` hashes a plan with every node in that form, encoded by its operator when it
decodes, so a set in another order is the same set. The core decides "unchanged" by the operation hash (`does`, beside
`plan_hash`, which status keeps), kernels are keyed by the operation form (`KernelKey`), and the plan diff compares it.
A published program whose composition does what the plan running does **adopts** its generation: the core records the
new origin (`InstallerCore::adopt`, from `serve_unchanged`), replaces the plan it holds, and its hash, with the
composition, which names the program in its provenance, and asks the host for
`Action::Adopt { generation, origin }`; ADP sends every region `RegionControl::Adopt`, which gives the generation the
program's panic budget. Nothing is built, cut, or drained. The default program takes a generation back the same way
when the program is revoked. In the protocol, the config is acknowledged `active` on the **unchanged** generation ID,
and status names the program (`active_artifact`, the active generation's origin, `program.runs: published`). A new
generation ID with no physical change was rejected as more surprising: a generation ID names a physical installation
(programs, kernels, leases, lineages), and every series and acknowledgment keyed by it would move for nothing. One
consequence: an adopted generation can roll back, so a later artifact keeps it as last-known-good. The installer
tests pin it: `install::unified_scope_tests::an_unmodified_export_of_the_one_scope_adopts_the_running_generation` (no
preparation, so no kernel built; a reordered document adopts again as its revision; the revocation hands it back), and
beside it a routing edit that rebuilds only the policy region's slots, a width edit that is a cut, and a renamed
aggregation that keeps its state with no cut, fed otherwise or not.

- **An adopted generation that fails** (found in review). If an artifact's generation exhausts its panic budget and the
  revocation is processed before the region's report, the default program adopts it with a zero budget and the report
  then fails a default-program generation, which cannot roll back: every metric would be dropped until the next
  configuration change. The core marks a generation the default program adopted from an artifact
  (`adopted_from_artifact`) and, when such a generation fails, asks for the default program, which gets a new
  generation, since a candidate equal to a failed one is never unchanged. A default-program generation that was never
  an artifact's still asks for nothing: under its zero budget a panic fails the region, as a native component does.
  This was preferred to refusing the adoption of a generation with charged panics, which would need the regions'
  ledgers in the core and still race with a report in flight. Test:
  `a_generation_the_default_program_adopted_after_its_artifacts_budget_ran_out_is_replaced`.
- **Revoking a program with implied outputs** makes two default-program generations: one at once, while the program's
  artifact is still held and its implied outputs declared, and one when its demand is gone and lowering drops them.
  The second differs only in its output declarations, so it is a commit that reuses every kernel; avoiding it would
  need the core to tell an unused output declaration from a change, which it does not.

**B: the detached export.** `export::export_as(plan, scope, catalog, labels, ExportForm::Detached)` writes policy
bindings as their resolved values, keeps facts and signals bound, folds literal `flag` and `is_empty`
(`Predicate::fold_literals`, with the junctions they decide), and addresses native outputs to their recipients (see D).
The bundle has `form`. ADP's route serves `detached` by default and `bound` for `?form=bound`. Deviations:

- **The base is the bound export's.** The detached artifact records the bound export's base, so composing it is no
  stale base, and `status.default_program_hash` is unchanged. A client therefore refetches on structural changes only;
  `program.differs` says when native values changed.
- **`program` in status** is `{runs, native_policy, differs}` (`channel::ProgramStatus`): `runs` is `default` or
  `published`, from the active generation's origin; `native_policy` is `applied` or `not_applied`. There is no
  `partly_applied` and no `bound` list: with E in place, ADP never runs a program that binds a policy setting.
  `differs` (`export::policy_differences`) lists each policy binding of a default-program node whose value at the
  binding's place in the running node differs, or whose node or place the program does not have; a node kept
  unmodified lists nothing. ADP logs one `info` line per native policy value that changes while a published program
  runs.
- **Comparison by value** (`same_operation`): parameters as written, or else both sides in their operation form. A
  node written as the default program writes it is the default program's, bindings and all. A node that matches only
  by value, such as a literal equal to a binding's value, is unmodified but composes as the artifact writes it (fixed
  in review: it was the default program's node, still bound, so a native change made a generation that did nothing
  new, and another when the value returned). So a detached program never follows native policy: a native policy change
  recomposes it to a plan that does what the running one does, and no generation is made. ADP's
  `a_native_policy_change_under_a_published_detached_program_changes_nothing_and_neither_does_its_return` pins it, and
  the edited-program test keeps generation 2 through the change. `policy_differences` skips a node that does what the
  default program's does.

**D: emits addressed to recipients.** `emit` takes exactly one of `output` and `to` (`operators::EMIT_TO`, a list of
recipient names); the syntax writes `emit to [primary, "https://127.0.0.1:17102"]`, a name bare when it is an
identifier and quoted otherwise. Validation refuses a plan's `emit` with `to` (`declared_output`): composition resolves
it.

- **Composition** resolves each `to` through the directory to its recipients, which must lie on one host, and
  `Catalog::output_for` gives the output with exactly those recipients, which the `emit` then names; it records no
  fingerprint. A new artifact waits for an unknown name (`ComposeError::UnknownRecipient`) or for an output for its
  recipients (`PendingRecipients`), and is refused for recipients of two hosts (`RecipientsOnTwoHosts`) or a set the
  host does not add an output for (`NoOutputFor`, from `Catalog::implied_output_refused`). The artifact in effect turns
  such an `emit` into a counted drop and runs degraded (`DropTarget::Recipients`).
- **Demand and implied outputs.** `compose::demanded_outputs` returns each distinct set an artifact's `emit`s address,
  with the first `emit` that addresses it. ADP computes the demand of every program the core holds
  (`InstallerCore::held_policy_artifacts`: in effect, installing, active, kept, waiting) and lowers again whenever it
  changes, after every installer event, so a waiting program's implied output is added and one nobody asks for retires.
  Lowering (`resources::imply`, through `Demanded`) adds one output per set no output serves: on `dd_out`, with the
  encoder native configuration builds for an endpoint routing group of those endpoints (or the main output without the
  others), selector `Selector::Recipients` (new), and native provenance. Implied and declared outputs together number at
  most `MAX_DECLARED_OUTPUTS`, counted over the demand of every program the core holds, which is what exists together:
  a replacement's outputs are built before it runs and the running program's retire after it, so a replacement that
  asks for nine new sets beside a running program's nine is refused. The refusal says so.
- **The implied output ID rule:** `implied_`, the local ID of the first `emit`, in node ID order, that addresses the
  set, and `_` with the first eight hexadecimal digits of the SHA-256 digest of the set's recipient IDs (each followed
  by a newline), the `emit`'s part cut to fit the 63 bytes of an identifier, and the whole numbered `_2`, `_3`, and so
  on after a collision; an implied output keeps the ID it had in the lowering before while any program asks for its set,
  so renaming an `emit` later renames nothing. The digest makes the ID the set's (fixed in review): an `emit` whose
  recipients change asks for another set, which gets another ID, so telemetry under one ID always means one set. No
  recipient name is ever part of an ID. For example, the branch `to_team_b`'s inline `emit to [...]` on its `true` port
  is the node `to_team_b_true`, whose output is `implied_to_team_b_true_e7b845f5`.
- **The detached export addresses native outputs to their recipients** wherever a set of directory names stands for
  exactly an output's recipients and `output_for` gives that output back, so composing it maps each `emit` to its
  native output and the plan hash is unchanged: `emit to [primary, "https://..."]` for a main output that dual-ships,
  `emit to ["https://..."]` for a routing group, `emit to [mrf]`, `emit to [cluster_agent]`. The client telemetry tap,
  which has no recipients, stays `emit dsd_client_telemetry`.
- **Simpler than designed:** implied outputs are added on `dd_out` only, for native recipients. A set on another host
  (only the Cluster Agent's could lack an output, while autoscaling failover is off) or with a recipient a resources
  artifact declares is refused, with the reason; a resources artifact's own outputs still serve its recipients. The
  `cluster_agent` recipient therefore still runs only while autoscaling failover requests its branch.
- **Found on the way:** the duplicate analysis grouped outputs that share a recipient transitively, so copies to two
  implied outputs with no recipient in common, beside the main output that shares one with each, were refused. It now
  compares only paths whose outputs share a recipient (amendment 187).

**Program limits** (found in review; amendment 188). A program is every node of its scope, so the artifact limits
measured every node, and an unmodified export of a native configuration beyond a limit, such as a 128 MiB mapper
interner (`artifact_memory`) or a routing tee with a port for each of 40 routing groups (`tee_fan_out`), could not be
published at all. Composition now validates against the default program (`Validator::with_baseline`), and each limit
rises to the default program's own measure in the program's scope, where that is more: node count, predicate leaves,
and declared memory for the program, fan-out by copying node, and work per input by entry node. A program may keep
what native configuration sized the host for, but not grow past it, and is held to the limits wherever the default
program is within them, so the limits keep their meaning for what an author adds. Measuring growth over the default
program instead would have let an author add up to the limit beyond any native size; exempting unmodified nodes would
have let a modified node keep its native size only by luck of correspondence. The validation report keeps the
baselines, so a rewritten plan is checked against the same limits. ADP's export tests pin both cases.

**E: facts and signals only.** `Catalog::detached_programs` (a `MemoryCatalog` member, `detached_programs`) makes
composition refuse a node, kept or not, that binds a policy binding, before anything is resolved:
`ComposeError::PolicyBinding { node, binding, value, allowed }`, whose message gives the value to write and the facts
and signals the host declares (`Catalog::facts_and_signals`). ADP's catalog declares it; the plan crate's fixtures do
not. The demo places the error at the binding's text.

**Composition pass order, as built:** inline blobs; refuse policy bindings (E); assign plan IDs, with the one
aggregation (F); resolve `to` into outputs (D); compare by value (B); keep, modify, or add each node, resolving the
bindings of the nodes not kept; check the fingerprints of output-addressed `emit`s.

**Standalone configuration reload** (owner's decision; amendment 185). Saluki-only
`experimental.standalone_config_reload.enabled` (default `false`) and `.interval` (default `1s`), both read at startup:
standalone mode then re-reads its configuration file every interval and applies a change through the path connected
mode's updates use (the file and environment merged again, environment precedence as at startup, translated and
validated whole, live views woken), rejecting an unreadable or invalid file with a warning and keeping the last
configuration it accepted. Restart-only keys stay restart-only, since their consumers read the configuration once. It
exists only for the demo. A change is taken once two consecutive reads agree, so a file caught while it is written is
never applied; the file is read off the async runtime, an interval under 100 milliseconds is raised to it with a
warning, and connected mode warns once that the setting does not apply there. The demo launcher writes the file
atomically. ADP tests: an endpoint added to the file becomes a live recipient, one removed drains and retires, an
invalid file is reported and changes nothing (`standalone_reload_tests.rs`); the configuration system's tests add a
half-written file, a removed key returning to its default, a deleted and recreated file, and `BeforeFile` precedence,
and prove each rejected file was read.

**Phase 3**, the demo, is [built](#85-the-demo-as-built): S15 became the second launch's S14.

### 10.7 What does nothing, as built

The owner's request of 2026-10-15: the detached default program showed native features that do nothing as
configured (an empty filterlist, a namespace prefix with no namespace, a mapper with no profiles, a tag filterlist
with no rules, a one-input merge, host enrichment), so that the program a user read was not only what happens. The
detached export now leaves them out (amendment 191). The default program, which ADP runs with no artifact, is
unchanged: its parameters are live bindings, and its structure never depends on a configuration value.

- **Identity, per operator.** `OperatorDef::is_identity` (default `false`) says that a node passes every record
  through unchanged with the parameters it knows: `dogstatsd_mapper` with no profiles, `namespace_prefix` with an
  empty namespace (whatever its exclusions: the kernel is disabled), `tag_projection` with neither rules nor value
  rules (every counter and distribution is a rule miss), `merge` of one input, `host_enrichment` always (it takes no
  parameters and changes no metric), and `host_tags` with a literal empty list. A parameter still bound, a fact or a
  signal, is not known, so ADP's `host_tags`, bound to the host's tags, is never left out. Each claim has a kernel
  property test (`property_test_..._leave(s)_every_metric_as_it_is`, in the kernels crate) and, together, a reference
  interpreter test: the fixture with nothing configured for DogStatsD, with and without the nodes delivers the same points to every output
  (`export::eliminate::tests::what_is_left_out_changes_nothing_the_reference_interpreter_delivers`).
- **Constant splits.** `Predicate::literal_value` decides a predicate by its literal operands: a `const`, a literal
  `flag` or `is_empty`, and a string match over a literal empty list (`intake_name in []`, which matches nothing),
  with the junctions they decide, as `kind in [...] and intake_name in []`. A bound operand never decides, whatever its
  value, so the failover gateways, whose flags are a fact and signals, stay. Such a `split` is folded by R3's own
  rules (`compile::rewrite::r3::fold_split`, which shares R3's obligations: no source, fence, stateful or dynamic
  operator, or node a scope names is removed, a `merge` keeps its other inputs, and an `emit` that goes leaves its
  output declared). R3 itself is unchanged: it still folds only what port types decide.
- **One pass, shared.** `export::eliminate(plan, registry, literal)` removes constant splits, then identity nodes,
  until none is left, deciding on the plan's literal form and applying the same removals to the plan as given. Every
  use takes its literals from one catalog, `export::policy_literals` (the bindings the catalog declares as policy
  settings; facts, signals, and undeclared bindings stay bound), and the host's operator registry, so they leave out
  exactly the same nodes. The detached export (`export_as`, which now takes the registry) lists what it left out in
  the bundle's `omitted` (`identity`, `constant_split`, or `unreachable`, by local ID). `install::diff_operations`
  diffs what plans do the same way, for the editor's preview, and `export::policy_differences` treats a node that does
  nothing as following nothing. ADP's export test checks, for every configuration of the lowering matrix, that the
  export leaves out what this pass leaves out of the default program, and that the composed program has nothing left
  to leave out.
- **Installation compares as written** (the owner's decision after review, replacing adoption across what is left
  out). The installer compares a candidate with the running generation's plan by `export::operation_hash`, which does
  not leave anything out (it sorts nodes and edges, whose order means nothing to how a plan runs). So publishing the
  shorter export over the default program is a **generation that builds no kernel and makes no cut**: aggregation
  keeps its lineage (`retain`), every slot that ran a left-out node gets a program without it, and those kernels are
  released. The plan running is then the program as published, so `/plan`, `/explain`, telemetry, and the core's
  active plan agree, and later edits are precise. A program whose written form equals the running generation's, such
  as the same program published again, still adopts it. Revoked, the default program, which has every node, makes a
  generation of its own over such a program. The installer tests: `install::unified_scope_tests`'s
  `the_shorter_detached_export_over_the_default_program_is_a_commit_that_builds_no_kernel` (no kernel built, no cut,
  `retain`; then a filterlist edit changes only its slot; then the same program again adopts) and
  `the_default_program_takes_back_only_a_generation_that_runs_every_node_it_has`, and ADP's
  `the_shorter_detached_export_over_the_default_program_builds_no_kernel_and_makes_no_cut_in_adps_frame`, through ADP's
  own frame and core.
- **The editor.** The preview diffs what plans do, so the shorter export shows no change, with a note: "ADP will drop
  N nodes that do nothing as configured, which the program leaves out; that builds no kernel."
- **The note.** ADP names what the export left out in one line, `Annotations::omitted`, from the lowering module's
  table of names (`lowering::omitted_name`), in program order; the `drop` of a left-out filterlist goes with it,
  unnamed. The editor prints it at the top of the program, wrapped to 72 columns: "Not shown, because they do nothing
  as configured: mapper (no profiles), namespace prefix (none), metric filterlist (empty), tag filterlist (empty),
  post-aggregation filter (empty), enrichment merge (one input), host enrichment (changes no metric)".
- **Consequences.**
  - A published program built from the shorter export really does not contain those nodes, and runs without them.
    Under detachment that is correct: the program does what it says. Set natively later, a left-out filterlist does
    something the program does not, and `program.differs` lists the setting that made the difference (its entries,
    not its match mode: a node the running plan lacks lists the settings that, turned off alone, would make it do
    nothing again, or all its settings when none would). ADP computes it once per plan running and default program
    lowered, not at every status.
  - While such a program runs, the left-out nodes' telemetry is absent, as for any node a program removes:
    `plan_node_events_total` for their ports, and the native series their profiles keep. The installer sets the
    gauges it records from a plan for a node the plan running lacks to zero, what an empty list and no rules imply,
    rather than keep the value of the generation before: `filterlist.size` and `tag_filterlist.size`, as the Datadog
    Agent reads them, and the endpoint allowlist sizes. The other remapped series of those nodes are counters that
    stay zero under the configuration that leaves the node out (`dogstatsd.listener_filtered_points`,
    `aggregator.dogstatsd_filtered_metrics`, `aggregator.filtered_tags`, the tag filterlist memo's hits and evictions,
    `filterlist.updates`, `tag_filterlist.updates`). The only change the Agent sees is
    `aggregator.filtered_tags_cache_miss`, which counts the tag filterlist memo's lookups even with no rules (one per
    counter and distribution) and stops counting. Three counters no remapping reads stop too: the tag filterlist
    kernel's `tag_filterlist_rule_misses_total`, the mapper's cache series, and, for an endpoint routing group with
    neither names nor prefixes, `metrics_endpoint_allowlist_metrics_filtered_total` on `metrics_routing_filter_<i>`,
    which counted every metric the group did not receive. The default program reports all of them.
  - Memory limits: a program is measured against the default program's total, so a program that leaves out the tag
    filterlist may give its context cache's memory to another node (the ADP limit test now grows the mapper's interner
    by 64 MiB rather than one byte).
- **Left out of the design, on purpose.** A one-port `tee` is an identity too, but the policy `tee` is where a program
  routes, so it stays even with one branch. `timestamp_split` and aggregation are never left out: one is not an
  identity, the other a state owner.


## 11. Assumptions

Each minimizes surprise for operators and contributors, and can be revisited.

- The agreed staged path is followed: step 1 converts at export rather than lowering literals at once; lowering
  literals is the next simplification ([9](#9-what-this-lets-us-remove)).
- An unknown recipient name waits (`pending`), as an undeclared output does, rather than being refused, so routes and
  resources can be published in either order.
- An emit to a recipient that becomes unavailable while its program runs (failover turned off, a declaration revoked)
  degrades the program, with counted drops at that node, as stage 7 does for a removed output.
- While a program runs, an endpoint added to native configuration receives nothing from it; status lists it as not
  addressed. This follows from the decision.
- Recipient names are not fingerprinted: a program follows a name to whatever the resource owner declares under it.
- Folding in the detached export covers only `flag` and `is_empty` over literals and the junctions they make constant.

## 12. Open questions for the owner

1. **Recipients declared in configuration** (`experimental.programmable_metrics.resources.recipients`), recommended for
   the demo's teams B and C and as the operator's path to destinations only programs route to. Accept the new
   Saluki-only key, or seed the resources product at the demo's launch instead?
   *Decided by the owner (2026-10-14): neither. Native `additional_endpoints` declare destinations, named by their URLs,
   and the demo adds them live through the standalone configuration reload; part C2 is dropped.*
2. **Provenance under a published program.** Today an unmodified lossy node of a published program keeps native
   provenance, so its loss is attributed to native configuration and its panics fail the component. Under "programs
   own policy", should every node of a published program be the program's (simpler, and removes `guards_authority`),
   at the cost of S0 making a stateless generation? Assumed: keep today's rule for now.
   *Decided by the owner (2026-10-14): drop node provenance entirely, and keep S0 free of a generation by adoption.
   Built in [phase 2](#106-phase-2-as-built).*
