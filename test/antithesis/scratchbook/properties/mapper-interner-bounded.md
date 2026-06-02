# mapper-interner-bounded

## Origin

Coverage-gap analysis. The catalog's `interner-full-bounded` covers the **DSD source's** context
interner. The `dogstatsd_mapper` carries a **second, independent** string interner (a whole separate
`ContextResolver` built inside the mapper, default 64 KiB) that interns the *mapped* names and the
*expanded* tags. When that interner is full, the mapper's `try_map` returns `None` and the metric is
**silently left un-remapped** — it flows downstream under its *original* (unmapped) name/tags. This is
a distinct, uncovered silent-failure surface: a second bounded resource with its own
saturation-drop behavior, on a transform that claims Agent equivalence.

## Code paths

- `lib/saluki-components/src/transforms/dogstatsd_mapper/mod.rs`
  - Interner built at `mod.rs:158-167`:
    `ContextResolverBuilder::from_name("…/dsd_mapper/primary")…with_interner_capacity_bytes(64 KiB
    default)…build()`. **It does NOT call `with_heap_allocations(false)`**, so heap fallback defaults
    to `true` (`resolver.rs:258`). Default size = `default_context_string_interner_size` = `ByteSize::kib(64)`
    (`mod.rs:34-36`), key `dogstatsd_mapper_string_interner_size` (`mod.rs:51-55`).
  - Slow-path resolve: `self.context_resolver.resolve_with_origin_tags(new_name.as_str(), merged_tags,
    origin_tags.clone())?` (`mod.rs:317-321`). The trailing `?` means **when resolution returns
    `None`, `try_map` returns `None`** → the caller does not replace the context.
  - Cache-hit path resolves too: `resolve_with_origin_tags(result.name.clone(), merged_tags, …)`
    (`mod.rs:277-282`) — returns the `None` directly. So even a cached positive result can fail to
    materialize a context if the interner is full at apply time.
  - Caller: `DogStatsDMapper::transform_buffer` (`mod.rs:388-398`) — `if let Some(new_context) =
    try_map(...) { *metric.context_mut() = new_context; }`. **No `else`** → on `None` the metric keeps
    its original context silently. No drop, no `events_discarded`, no dedicated counter.
- Resolution `None` semantics: `resolve_inner` → `create_context` returns `None` when name/tag
  interning fails and heap is disallowed (`resolver.rs:436-483`, name interned at the
  `try_intern…or_else(allow_heap_allocations.then(...))` site `resolver.rs:346-349`).

## Failure scenario

Two distinct modes:

1. **Default config (heap fallback ON):** under a high-cardinality flood of mappable names, the mapper
   interner never returns `None` but spills mapped names/tags to the heap — the mapper's declared
   64 KiB bound is voided and memory grows unbounded (parallels `interner-full-bounded`'s
   default-config failure, but for a *second* interner the firm bound accounts for at
   `mod.rs:374-375`).
2. **Heap fallback OFF (if the operator disables it):** the mapper interner fills; `try_map` returns
   `None`; the metric is **forwarded under its original, unmapped name/tags**. Downstream filters
   (`dsd_prefix_filter`, `dsd_tag_filterlist`, `dsd_post_agg_filter`) then make decisions on the wrong
   name, and the customer sees the pre-mapping identity — a silent correctness divergence from the
   Agent, not just a dropped metric. Behavior is non-deterministic across the cardinality of *mapped*
   strings, independent of the source interner.

## Property

- **Type:** Safety. Heap-OFF: the silent-non-remap is a correctness hazard to surface. Heap-ON
  (default): bounded-memory claim fails by design (mirrors `interner-full-bounded`).
- **Invariant:**
  - Heap-OFF: `AlwaysOrUnreachable(mapper interner full ⇒ metric forwarded UNDER ORIGINAL context,
    accounted)` — i.e. the silent-non-remap must be observable/counted, never a silent partial-map.
    `Sometimes(mapper resolve == None)` proves exhaustion is reached.
  - Heap-ON (default): `Sometimes(mapper intern heap fallback > 0)` proves the unbounded spill path is
    reachable for the *mapper's own* interner.
  - SUT-side instrumentation required to distinguish mapper-interned / heap-fallback / resolve-None /
    forwarded-original — none of these has a metric today (the firm-bound accounting at
    `mod.rs:367-382` is a static declaration, not a runtime counter).
- **Antithesis angle:** small `dogstatsd_mapper_string_interner_size` + a flood of *distinct mappable*
  names (each expands to a unique mapped name + tags) fills the mapper interner specifically; combine
  with the source-interner flood (`interner-full-bounded`) to show the two interners saturate
  independently. Timing/scheduling exploration stresses the resolver under churn (idle-context
  expiration is 30s, `mod.rs:166`).
- **Priority:** High.

## Config dependencies

- `dogstatsd_mapper_string_interner_size` (default 64 KiB) — shrink to force exhaustion cheaply.
- `dogstatsd_allow_context_heap_allocs` — note this is the **DSD source** key; the mapper interner
  does **not** read it (it never sets `with_heap_allocations`), so the mapper always defaults to
  heap-ON unless the resolver default changes. Confirm there is no separate mapper heap flag (there is
  not in current source). This asymmetry is itself a finding.
- `dogstatsd_mapper_profiles` must be set (a profile must match) for the mapper interner to be
  exercised at all.
- `dogstatsd_mapper_cache_size` (default 1000): a cached positive result still re-resolves
  (`mod.rs:277-282`), so the interner can fail even on a cache hit.

## Open Questions

- Is the mapper's lack of a `with_heap_allocations(false)` option intentional, or an oversight that
  makes the mapper interner's declared 64 KiB firm bound (`mod.rs:374-375`) unenforceable under
  default behavior? `(needs human input)`
- The cache stores the mapped `name`/`extra_tags` but resolution can still fail at apply time
  (`mod.rs:277-282`): does that mean a metric can be remapped on one call and silently NOT remapped on
  the next, for the *same* name, purely due to interner pressure? That would be a non-deterministic,
  load-dependent identity flip — needs confirmation under churn.
- Does the Datadog Agent mapper have an analogous bounded interner with the same drop-to-original
  behavior, or does it always allocate? Determines whether heap-OFF behavior is an ADP-specific
  divergence (ties to `mapper-output-matches-agent`).

### Investigation Log

- Examined: `dogstatsd_mapper/mod.rs` build (`158-183`), `try_map` (`259-342`), `transform_buffer`
  (`388-398`), `specify_bounds` (`367-382`); `resolver.rs:251-299,334-349,436-483` for the resolver's
  default `allow_heap_allocations=true` and the `Option<Context>` `None` path.
- Found: the mapper instantiates a fully separate `ContextResolver` with its own 64 KiB interner and
  the default heap-ON behavior; on resolve-`None` the metric is silently forwarded under its original
  context with no counter. This is a genuinely second interner-full surface, distinct from
  `interner-full-bounded` (DSD source) — different resource, different downstream consequence
  (silent non-remap vs. dropped context).
