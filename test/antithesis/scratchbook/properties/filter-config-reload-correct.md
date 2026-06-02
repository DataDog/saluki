# filter-config-reload-correct

## Origin

The design partner's documented focus (Confluence "Tag Filter RC Relay Stress Test: agent + ADP",
AMCC space): the Core Agent pushes metric-filter config (`metric_tag_filterlist`, `metric_filterlist`,
`statsd_metric_blocklist`, …) over the Remote Config → AgentSecure → ADP config stream **at runtime,
while data is flowing**. Five components rebuild correctness-affecting filtering state live from that
stream. The existing `config-runtime-update-not-revalidated` treats config purely as a crash/
incompatibility gate; it never treats a config update as a **data-correctness** event. This property
fills that gap: a botched live reload produces *stale* or *fully-cleared* filtering applied to live
customer metrics — wrong tags retained/dropped, or all filtering silently disabled.

## Code paths

### The watcher (shared mechanism)

- `lib/saluki-config/src/dynamic/watcher.rs:36-74` — `FieldUpdateWatcher::changed`:
  - **Hazard 1 — silent lag drop:** `Err(broadcast::error::RecvError::Lagged(_))` (`:61-67`) only
    `warn!`s and `continue`s. The broadcast channel has capacity **100** (`lib.rs:363`,
    `broadcast::channel(100)`). Under a burst of config changes (or a slow consumer task), a receiver
    that falls >100 behind **silently loses** the intervening `ConfigChangeEvent`s. If the *latest*
    state was in the dropped span and no further change to that key arrives, the component keeps
    **stale filters forever** with no recovery — there is no re-read of current config on lag.
  - **Hazard 2 — partial-deserialize skip:** `:42-57` — `serde_json::from_value::<T>(...).ok()`; if
    the new value fails to deserialize, it `warn!`s and **skips the update** (loops). A
    multi-key/multi-entry filter config where one entry is malformed can leave the component on the
    **previous** config (half-applied across a multi-watcher component — see below).
- Each watcher is an independent `broadcast::Receiver` (`lib.rs:797-798,821-824`); a component with N
  watchers has N receivers that can lag/skip **independently** → a partially-updated filter set.

### Hazard 3 — key deletion never fires (the silent clear-all, subtler than expected)

- `lib/saluki-config/src/dynamic/diff.rs:12-48` — `diff_recursive` iterates **only `new_dict` keys**.
  A key present in the *old* config but **absent in the new snapshot produces NO `ConfigChangeEvent`.**
  So *deleting* `metric_tag_filterlist` from the streamed config does **not** notify the watcher; the
  component keeps the **old** filters (stale), it does not clear them.
- The clear-all DOES happen when the key is delivered as an explicit empty/null value:
  - `tag_filterlist/mod.rs:274-276` — on a `changed()` event,
    `compile_filters(new_entries.as_deref().unwrap_or(&[]))`. If `new_value` deserializes to `None`
    (e.g. explicit `null` or a shape that fails per-element but yields `Some([])`), this rebuilds with
    **`&[]` → ALL tag filtering removed**, and **rebuilds the context cache** (`build_context_cache()`).
  - `dogstatsd_prefix_filter/mod.rs:311-334` and `dogstatsd_post_aggregate_filter/mod.rs:290-313` use
    `if let Some(new) = maybe_new { … }` — they *ignore* a `None`, so an explicit-empty arriving as
    `None` is a **no-op** there, but an explicit empty `[]` (deserializes to `Some(vec![])`) clears
    the list.
- Net effect: **deletion = stale (Hazard 3a)**, **explicit-empty = cleared (Hazard 3b)**, and the two
  filter families (`tag_filterlist` vs `prefix_filter`/`post_agg_filter`) react **differently** to a
  `None`. This inconsistency across the five components is a correctness hazard in itself.

### The five live-reloading components

1. `bin/agent-data-plane/src/components/tag_filterlist/mod.rs:222,274-277` — 1 watcher
   (`metric_tag_filterlist`); rebuilds `self.filters` + `self.context_cache` live.
2. `bin/agent-data-plane/src/components/dogstatsd_prefix_filter/mod.rs:285-289,311-334` — **4 watchers**
   (`metric_filterlist`, `metric_filterlist_match_prefix`, `statsd_metric_blocklist`,
   `statsd_metric_blocklist_match_prefix`); each rebuilds the effective blocklist matcher.
3. `bin/agent-data-plane/src/components/dogstatsd_post_aggregate_filter/mod.rs:268-273,290-313` — **4
   watchers**; rebuilds the histogram-suffix matcher.
4. & 5. Any other `watch_for_updates` consumers on correctness-affecting keys (grep
   `watch_for_updates` — the prefix/post-agg filters share the same four keys, so a single key change
   fans out to multiple components that must stay mutually consistent).

## Failure scenario

While ADP forwards live metrics, the Core Agent pushes a rapid sequence of filter-config updates
(the RC relay stress test). One of:

- **Lag:** the burst exceeds the 100-slot broadcast buffer; a filter component's receiver lags, the
  `Lagged` arm drops the events, and the component keeps applying **stale** filters (e.g. still
  excluding a tag the operator just re-included, or still forwarding a metric just added to the
  blocklist) with no self-correction.
- **Partial:** one malformed entry in a multi-entry `metric_tag_filterlist` update fails
  deserialization → the whole update is skipped → stale filtering; or, in `prefix_filter`, one of the
  four keys updates while another's event is dropped → a **half-applied** filter config (new
  blocklist, old match-prefix flag) that filters inconsistently.
- **Clear-all:** an explicit empty `[]` (or a `None`-deserializing value) for `metric_tag_filterlist`
  rebuilds with `&[]` → **all tag filtering silently disabled** on live data (tags the customer
  intended to drop now flow to intake); deleting the key entirely instead leaves filtering **stale**
  (the opposite surprise).

All are silent (warn-only at most) and customer-visible (wrong tags / wrong metrics forwarded).

## Property

- **Type:** Safety (data-correctness under live config reload).
- **Invariant:**
  - `Always(after a filter-config update is acknowledged-applied, the next metric for an affected
    name is filtered per the NEW config)` — i.e. no stale filtering after a settled update. Assert
    SUT-side at the filter apply site by comparing the metric's post-filter tags/keep-decision to the
    currently-loaded `CompiledFilters`/matcher.
  - `Unreachable("filter update Lagged-dropped with no subsequent reconciliation")` on the
    `RecvError::Lagged` arm (`watcher.rs:61`) — or, if lag is accepted as best-effort, `Reachable`
    there + a liveness check that the component eventually converges to the latest config (it does
    not, today — there is no re-read).
  - `Sometimes(filter config reloaded while metrics in flight)` to prove the reload-under-load state
    is reached (non-vacuity).
  - `AlwaysOrUnreachable(tag filtering not silently fully-cleared by a config event that the operator
    did not intend as a clear)` — distinguishes deletion (should-stay-or-explicitly-clear) from
    explicit-empty.
- **Antithesis angle:** the core interaction is **burst + scheduling**: push many filter updates
  faster than the filter task drains its broadcast receiver (node throttling / CPU modulation on
  `adp` widens the lag window), interleaved with sustained metric load so the stale/half-applied/
  cleared window overlaps live data. Also explore (a) deletion vs explicit-empty vs malformed-entry
  shapes, and (b) updating one of the four prefix-filter keys while starving another receiver.
- **Priority:** High (this is the design partner's explicit stress scenario; correctness under live
  RC reload is the headline of the AMCC Confluence page).

## Config dependencies

- Dynamic config must be **enabled** (remote-agent mode, Add-on 1 topology with a Core Agent / config
  stub). In standalone mode `watch_for_updates` returns a watcher whose `rx` is `None` →
  `changed()` pends forever (`watcher.rs:30-33`) and none of this fires. **This property cannot run in
  standalone mode.**
- Keys to drive: `metric_tag_filterlist`, `metric_filterlist`, `metric_filterlist_match_prefix`,
  `statsd_metric_blocklist`, `statsd_metric_blocklist_match_prefix` (constants in
  `dogstatsd_filterlist.rs`).
- The Core Agent/stub must be able to send **malformed**, **explicit-empty**, **key-deleting**
  (snapshot omitting the key), and **bursty** sequences the real Agent might not — favor a stub.

## Open Questions

- Is the broadcast `Lagged` drop considered acceptable (best-effort) by the team, or a bug? There is
  **no re-read of current config** on lag, so a dropped *final* update is permanent staleness.
  `(needs human input)`
- Deletion-doesn't-diff (Hazard 3a): is the intended Agent semantics that removing a filter key from
  RC should *clear* the filter? If so, ADP's additive `diff_recursive` (`diff.rs:12-48`) silently
  diverges (keeps stale filters). Confirm against Agent RC semantics.
- The `tag_filterlist` vs `prefix_filter`/`post_agg_filter` asymmetry on a `None` update
  (clear-vs-ignore): intended or accidental? It means the same RC action has different effects on
  different filters.
- Cross-component consistency: a single `metric_filterlist` change fans out to both `prefix_filter`
  and `post_agg_filter` via separate receivers — can they diverge transiently (one applied, one
  lagged) and does that produce an observable wrong-filter window?
- `tag_filterlist` only filters **Counter + sketch** metrics (`tag_filterlist/mod.rs:235-237`); does
  the Agent's equivalent filter the same metric-type subset? (See `tag-filterlist-applied-consistently`.)

### Investigation Log

- Examined: `watcher.rs` (full), `diff.rs` (full), `lib.rs:363,541-651,797-824` (broadcast cap 100,
  dynamic updater, subscribe/watch), and the `select!` reload arms in all three filter components.
- Found: (1) `Lagged` is warn-and-continue with no re-read → permanent staleness on a dropped final
  update; (2) partial-deserialize is warn-and-skip → stale/half-applied; (3) `diff_recursive` is
  additive so key **deletion** emits no event (stale), while explicit-empty/`None` clears for
  `tag_filterlist` but is ignored by the prefix/post-agg filters. Three distinct hazards, all
  silent, all on live customer data.
- Confirmed: in standalone mode the watcher never fires (`rx: None`), so this needs Add-on 1.
