# Programmable metric processing: the pipeline syntax

A new surface syntax for programs, written as streaming queries in the style of Unix pipes and PRQL, and a printer that
turns any program, including the default program lowered from native configuration, into the form a person would
write. Read the [index](programmable-metrics-index.md) first. This replaces the node-and-edge syntax of
`saluki-metric-plan-syntax` wholesale; the IR does not change.

## Contents

- [Why](#why)
- [Principles](#principles)
- [The language](#the-language)
- [Node IDs](#node-ids)
- [Printing: from a graph to the form a person writes](#printing-from-a-graph-to-the-form-a-person-writes)
- [Normalization](#normalization)
- [Operator registry additions](#operator-registry-additions)
- [The demo](#the-demo)
- [Work breakdown](#work-breakdown)
- [Testing](#testing)
- [Deferred](#deferred)

## Why

The node-and-edge syntax is a wiring diagram: every node is named, every edge is drawn, merges have numbered ports, a
filter is a `split` with an explicit `false -> drop`, and inserting a step means rewiring two edges. A program is a
streaming query over metrics, and people know that shape from shells, `jq`, PRQL, Kusto, and streaming SQL.

## Principles

1. **One IR, a thin syntax.** The syntax only builds a graph and prints one. It adds no meaning of its own: the
   reference interpreter and the equivalence oracle stay the definition of what a program does.
2. **A small core, with sugar defined by rewrites.** The core maps one to one onto the IR. Each piece of sugar is one
   local rewrite into the core, so proving the syntax correct is proving the core's lowering once and each rewrite
   once.
3. **Declarative.** Statements are equations over streams, not steps in time. Statement order matters only for
   scoping; the order of stages inside a pipeline is the graph's order, as it is in the IR.
4. **A canonical printed form.** Printing is a function of the graph, like `gofmt`. Each sugar form is printed only
   where parsing it back gives exactly the same graph; anywhere else the core form is printed.
5. **Separate passes.** The parser makes a tree; lowering turns the tree into an artifact; normalization (a plan crate
   pass, not a syntax concern) puts equivalent programs into one form; the printer recognizes sugar in a graph and
   lays the tree out as text. No pass does the job of another.

## The language

### An example

The default program's export, as the printer writes it once normalized (parameters bound to native configuration are
abbreviated `...`):

```text
policy metrics {
  let dsd_enrich = dogstatsd | dogstatsd_mapper(profiles: ..., cache_size: ..., string_interner_bytes: ...)

  let dsd_prefix_filter_blocklist = dsd_enrich
    | namespace_prefix(config.dogstatsd.prefix_filter.metric_namespace, exclusions: ...)
    | where not intake_name matches(values: ..., prefix: ...)

  let dsd_no_agg_split = dsd_prefix_filter_blocklist | timestamp_split(config.dogstatsd.aggregation.window_duration_seconds)

  let dsd_tag_filterlist = dsd_no_agg_split.default | tag_projection(rules: ..., value_rules: ..., cache_capacity: ...)

  let dsd_agg = dsd_tag_filterlist | aggregate(window: ..., flush_interval: ..., ...)

  let dsd_post_agg_filter = (dsd_agg, dsd_no_agg_split.passthrough)
    | where not (kind in [counter, rate, gauge, set] and intake_name matches(values: ..., prefix: ...))

  dsd_post_agg_filter | emit output dsd_client_telemetry

  let metrics = (dsd_post_agg_filter, otlp, checks, liveness, trace_sampler)
    | host_enrichment
    | host_tags(config.host.tags, deadline: config.host.tags_deadline)

  metrics | emit output dd_metrics_encode

  let metrics_routing_filter_0 = metrics
    | where intake_name in config.metrics_endpoint_routing.group.0123456789abcdef.names
        or intake_name starts_with config.metrics_endpoint_routing.group.0123456789abcdef.prefixes

  metrics_routing_filter_0 | emit output metrics_routing_encode_0

  let mrf_metrics_gateway = metrics
    | where config.multi_region_failover.enabled and config.multi_region_failover.metric_mirroring.enabled

  mrf_metrics_gateway | emit output mrf_metrics_encode
  ...
}
```

Every `let` names a node whose ID matters: one whose native telemetry identity, or state, is keyed by it. Every other
node takes the ID its position derives, so the program needs no `as`. The demo's detached export leaves out what does
nothing there, the merge and host enrichment among them, and names the destinations' stream with `let metrics =
dsd_post_agg_filter`.

### Lexical structure

As today, `#` starts a comment to the end of the line, and strings, numbers, durations, and identifiers are
unchanged. Whitespace and line breaks are insignificant, with two exceptions that statements without separators need:
an operator's arguments follow its name directly, as `op(...)` (after a space, `(` starts the next statement's tuple),
and a word that could continue a stage or start a new statement (`as` followed by `|` or `.`, naming a stream) starts a
statement only at the start of a line. The printer always writes `op(` and starts statements on new lines. A word right
after an adjacent `.` is an identifier even if it starts with a digit, so binding segments such as
`config.metrics_endpoint_routing.group.0123456789abcdef.names` read as written. New tokens: `|`, `!=`. New keywords, reserved only where the grammar
expects them: `let`, `as`, `where`, `emit`, `output`, `drop`, `exit`. Retired: `node`, `->`, `input.`, and
`prefix_in`.

### Grammar

```text
program    = "policy" scope "{" header* statement* "}"
header     = "ir_version" int | "base" hash [ "outputs" "{" (output "=" hash ","?)* "}" ] | "blobs" "{" ... "}"
statement  = "let" ident "=" pipeline                   # a named stream; `let x = y` names the stream y as x
           | pipeline                                   # must end in a sink
pipeline   = head ("|" stage)* ["|" sink]
head       = ref | tuple
ref        = ident ["." port]                           # a let, or a scope input; `.port` picks an output port
tuple      = "(" ref ("," ref)* ","? ")" ["as" ident]   # a merge, its input ports in order
stage      = operator ["(" args ")"] ["as" ident]       # any registered operator
           | "where" predicate ["as" ident]             # sugar
sink       = "emit" recipients ["as" ident]             # `emit primary`, `emit [primary, "https://b.example.com"]`
           | "emit" "output" output ["as" ident]
           | "drop" ["as" ident]
           | "exit" exit-name ["as" ident]              # leaves a scope that ends early (until V4)
args       = [positional ","] (param ":" value ","?)*
```

A pipeline in a `let` never ends in a sink. A stream reaches several places by being used several times: there is one
way to fan out, reuse. (A `fork { ... }` block was built first and removed on 2026-10-16: it was a second spelling of
the same `tee`, and the printer had to choose between the two arbitrarily.)

### The core and what each form becomes

| Form | IR |
|---|---|
| a scope input, `dogstatsd` | edges from `input.dogstatsd` |
| `e \| op(args)` | a node running `op`, with an edge into its only input port from `e` |
| `x.port` | output port `port` of the last node of `x`, which must have several output ports |
| `(a, b, ...)` | a `merge(inputs: n)`, with `a` at port `0`, `b` at port `1`, and so on |
| `let x = e` | names the stream `e`; `x` is used by each later reference to it |
| `let x = y`, with no stages | one use of `y`, named `x`: `x` used once is `y` itself, and used `n > 1` times gets its own `tee(ports: n)`, `x_fanout` |
| a stream used `n > 1` times | a `tee(ports: n)` after it, port `i` feeding the `i`th use in source order |
| `e \| emit recipients`, `e \| emit output o`, `e \| drop`, `e \| exit name` | an inline terminal node |

**Sugar**, each one rewrite into the core:

- `e | where p` is `let s = e | split(p); s.false | drop; s.true`.
- Inside predicates, `a != b` is `not (a == b)`, and `tags.k is unset` is `not has_tag("k")` (see
  [predicates](#predicates)).

### Rules the syntax enforces

Each replaces a validator rule with a syntax rule, so the author meets it at the point of writing:

- **Define before use.** A `let` is visible only after its statement, and a `let` cannot refer to itself. A cycle
  cannot be written (validation's `Acyclic` rule stays, for artifacts from JSON).
- **Every stream and port is used.** An unused `let`, or an unused output port of a node with several, is an error, not
  a silent drop: "`dsd_no_agg_split.passthrough` is never used; write `dsd_no_agg_split.passthrough | drop`".
- **A node with several output ports is never used bare**: pick a port.
- **Sinks end pipelines**: a sink in the middle of a pipeline is an error that says how to copy a stream instead (name
  it with `let` and use it twice).
- **A bare statement ends in a sink, a `let` does not.**
- **Names are unique**: a `let` cannot reuse another `let`'s name or a node ID, and a name a later `let` defines
  cannot be used before it as a scope input. The scope's inputs are not known when parsing, so a name no `let` defines
  is a scope input, and validation reports one the scope does not have, at its span.

### Arguments and defaults

- An operator's **positional parameter**, when its registry entry declares one, may be passed first without its name:
  `namespace_prefix("tenant", exclusions: [...])`, `timestamp_split(10s)`, `split(<predicate>)`.
- A parameter with a **registry default** may be left out. The printer leaves out every parameter equal to its default
  (normalization fills them in; see below).
- An operator without parameters, or whose parameters are all left out, takes no parentheses: `host_enrichment`.

### Values

Unchanged, except that a configuration binding is written `config.host.tags` (the binding name's segments, after
`config.`); `config("...")` is retired.

### Predicates

`not` binds tightest, then `and`, then `or`. `all(...)` and `any(...)` are retired: a junction of any length is written
with `and` and `or`.

| IR leaf | Syntax |
|---|---|
| `Name`, `IntakeName`, `Host` with `Eq` | `name == "a"` |
| ... with `Prefix` | `name starts_with "a."` |
| ... with `In` | `name in ["a", "b"]` |
| ... with `PrefixIn` | `name starts_with ["a.", "b."]` |
| ... with `Matches { values: config.x, prefix: false }` | `name in config.x` |
| ... with `Matches { values: config.x, prefix: true }` | `name starts_with config.x` |
| ... with `Matches { values: [literals], ... }` | the core spelling `name matches(values: [...], prefix: false)`; normalization turns it into `in` or `starts_with` |
| `HostUnset` | `host is unset` (and `not` of it, `host is set`) |
| `HasTag("k")` | `tags.k is set`; a key that is not an identifier is quoted, `tags["app.kubernetes.io/name"] is set` |
| `Tag` | `tags.team == "platform"`, and the other string matches |
| `HasOriginTag`, `OriginTag` | `origin_tags.pod is set`, `origin_tags.pod starts_with "web-"` |
| `KindIn` | `kind in [counter, gauge]` |
| `Const` | `true`, `false` |
| `Flag(config.x)` | `config.x` |
| `Flag(literal)` | `true`, `false` written where a flag is expected: printed as the constant leaf only when the IR leaf is `Const` |
| `IsEmpty` | `is_empty(config.x)`, `is_empty([])` |
| `Not(p)` | `not p`; `Not(<leaf> == s)` prints as `<leaf> != s`; `Not(HasTag)` prints as `is unset` |

Every IR predicate has exactly one printed spelling, so printing and parsing back gives the same predicate. Where two
spellings parse to the same IR (`not name == "a"` and `name != "a"`), the printer uses one.

## Node IDs

Kernel reuse and state retention match nodes by ID (`KernelKey.node`, `LineageKey.node`), so derived IDs must stay put
when someone edits elsewhere. Every node has an ID: the one its `as` clause gives, or a derived one.

- **In `let x = s1 | ... | sn`**, the last stage's node is `x`. Each earlier stage is `x_<operator>` (`where` is
  `x_split`; a tuple head is `x_in`). A repeated operator takes `_2`, `_3`, ... in pipeline order.
- **In a bare statement**, the **anchor** is the port the pipeline starts from: for a reference to a node with one
  output port, that node's ID; for `x.port`, `x_<port>`; for a scope input, `input_<name>`; for a stream used several
  times, its tee's port, `<tee>_<i>`; for a tuple head, its merge is `<anchor of its first element>_in` and the anchor
  is that merge. Each stage is `<anchor>_<operator>`, repeats numbered as above.
- **An implicit tee** after a stream `x` with one output port is `x_fanout`; after a port `x.p` it is `x_p_fanout`.
- **A sink** is the inline terminal [`derived_terminal_id`] names, `<parent>_<port>`, as today.
- A derived ID longer than 63 bytes is shortened as `derived_terminal_id` shortens. A derived ID that equals another
  node's ID is an error that asks for an `as`.
- Merges (`_in`) and tees (`_fanout`) follow the conventions native lowering already uses, so the default program needs
  no `as` on those.

The editor warns when an edit would change the ID of a stateful node (an `aggregate`), since its state would not carry
over.

## Printing: from a graph to the form a person writes

Printing is `format(resugar(artifact))`:

- **Resugar** (artifact to tree) recognizes sugar in the graph. It is total: any artifact prints. It is exact for every
  **well-formed** artifact, one whose edges name nodes and ports that exist, with one edge per input port and at most
  one per output port, unique IDs, no cycles, every node reachable from an input, and every output port used, apart
  from parameters equal to their registry default, which printing leaves out and normalization writes back in (so a
  normalized artifact's text parses to the same artifact once normalized, which composition always does). Some
  artifacts that are not well-formed cannot be written at all, such as an edge into a scope input or between nodes
  that do not exist; they print as well as they can, and their text never parses back to them.
- **Format** (tree to text) lays the tree out at the line width, breaking values and predicates as today's printer
  does.

Resugar works from the graph alone, in the [canonical order](../../lib/saluki-metric-plan-syntax/src/graph.rs) parsing
writes:

1. **Chains.** A node continues the pipeline of the node before it when it is that node's only consumer and has one
   input. Nodes start a new statement where a chain cannot continue: after a node with several output ports, after a
   stream used several times, at a merge, and where a node's ID does not follow the derived scheme for the current
   statement but its successor's does (so a person's choice to name an intermediate stream survives).
2. **`let` or bare.** A chain whose last node is used by another statement is a `let` named for that node; a chain that
   ends in a sink is a bare statement.
3. **Sugar, in priority order.** A `split` whose `false` port leads to a `drop` terminal with the derived ID is
   `where`. A `tee` named `x_fanout` after a stream `x`, whose port order matches the order its uses are printed in, is
   an implicit tee (reuse of `x`); one named `x_fanout` after any other stream `s`, where `x` names nothing else, is
   `let x = s` and the reuse of `x`. A `merge` is a tuple. Anything else is the core form, such as `let t = x |
   tee(ports: n)` with `t.0 | ...`.
4. **Names come from `let`.** A node whose ID is not the derived one ends a `let` of that name, and the pipeline goes
   on from it in a new statement, wherever a `let` can carry the name; `as` is the last resort, for a sink with an ID
   of its own and a node named like a scope input. If something needs a specific name, a `let` gives it.
5. **Defaults and positionals.** Parameters equal to their registry default are left out; a positional parameter is
   written first, without its name.

Each sugar form's recognizer is the exact image of its rewrite: expanding what resugar recognized gives back the same
nodes, edges, and IDs. Where two forms could describe the same graph, the priority order picks one, and a test pins
each such pair.

## Normalization

A plan crate pass, `saluki_metric_plan::normalize`, that puts equivalent programs into one form. It runs at the end of
native lowering and on every artifact before composition, so that the running default program, its export, and an
unchanged program parsed back from the export are the same document with the same hash. It never adds or removes a node
and never changes a node ID, apart from renaming a derived terminal whose port it moves; V6 later folds it into
elaboration. The rules, each checked against the oracle and for idempotence:

1. **Fill in defaults.** Every parameter left out that has a registry default is written with it.
2. **Literal set matches.** `Matches { values: [literals], prefix }` becomes `Eq`/`Prefix` for one value, `In`/`PrefixIn`
   for several, and `Const(false)` for none. `In([x])` and `PrefixIn([x])` become `Eq(x)` and `Prefix(x)`. Bound
   values are left alone.
3. **Predicate simplification, keeping the order the author wrote.** Junctions of one kind are flattened, constants are
   folded (`false or p` is `p`, `true and p` is `p`), `not not p` is `p`, and `IsEmpty` of a literal list is a constant.
   Operands are not sorted: the printed form keeps their order.
4. **Split polarity.** A `split(p)` whose `true` port leads to a `drop` and whose `false` port does not becomes
   `split(not p)` with the ports swapped (removing a double negation), so it prints as `where not p`. The drop terminal
   moves from `<split>_true` to `<split>_false`, and the other edge's port changes from `false` to `true`.

## Operator registry additions

`ParamSpec` gains two declarations, both `const` builder methods so specs stay `Copy` and `'static`:

- `.with_default(json)`: the default value as JSON text. Decoding treats a missing parameter as its default, validation
  accepts its absence, and normalization fills it in. Defaults are the native configuration defaults, cited at each.
- `.positional()`: at most one parameter per operator. A sole predicate parameter is positional, as today.

## The demo

The editor, the language reference page, the recipes, and the scenario edits move to the new syntax, and the reference
page is reworked for it:

- **Reference page.** Leads with the model (streams, stages, sinks, `let`, reuse, tuples), then a table of each sugar
  form beside the core form it rewrites to, then predicates, operators with an example of each in pipeline form,
  node IDs and `as`, the rules the syntax and the validator enforce, and the limits. Every example parses and every
  recipe compiles into the default program, as today.
- **Recipes** become one-line inserts: add `| where not (name starts_with "demo.debug.")` at the end of `let metrics`.
- **Editor.** Highlights the new keywords, dims `as` clauses, warns when an edit changes a stateful node's ID, and keeps
  underlining validator findings through the span map.
- **Annotations** (`compile.rs`'s `annotate`) attach their comments to the new statements.

## Work breakdown

Interfaces first (`ast.rs`, module stubs, the registry declarations, the `normalize` signature), then in parallel:

| Part | Owns | Depends on |
|---|---|---|
| **A. Parser** | `lexer.rs`, `parser.rs` (the new one; the old is kept as `legacy/` until integration) | `ast.rs` |
| **B. Lowering** | `lower.rs`: tree to artifact and spans, derived IDs, syntax rules | `ast.rs` |
| **C. Resugar** | `resugar.rs`: artifact to tree | `ast.rs` |
| **D. Format** | `format.rs`: tree to text, line breaking, predicates and values | `ast.rs` |
| **E. Normalization and registry** | plan crate `normalize`, `ParamSpec` defaults and positionals, wiring into lowering and composition, fixtures | registry declarations |
| **F. Demo** | `bin/programmable-metrics-demo`: UI, reference page, recipes, scenario edits, annotations | the language as specified here |

Then integration: wire the public API (`parse`, `parse_with`, `print`, `Parsed`, `SpanMap`, unchanged in shape) to the
new passes, convert every source text in tests and fixtures with the old parser and the new printer, retire `legacy/`,
and run everything.

## Testing

- **Round trip on graphs**: for every well-formed artifact in canonical order, `parse(print(a)) == a`, by property test
  over generated graphs, extending today's generator.
- **Fixed point on text**: `print(parse(s))` printed again is unchanged, sugared input included.
- **Normalization**: idempotent, oracle-equivalent, never changes a stateful node's ID.
- **Overlaps**: a test for each pair of sugar forms that could describe the same graph.
- **Errors**: every syntax rule has a test of its message and span.
- **The default program** prints in the form shown above, and an unchanged print of the export publishes as no change.

## Progress

Built on 2026-10-16, in this order:

| Commit | What |
|---|---|
| `0cbc6677d3`, `2bc6434f2f` | This design; the shared tree, the derived ID rules, stubs, and the registry declarations |
| `1b9aabf293`, `0293323fb6`, `df55d09bc7`, `0a36132f35` | The four passes, each with its own unit tests, built in parallel |
| `9f1962f5d2` | Normalization and the registry defaults, wired into native lowering and composition |
| `95c8d545b0` | The public API on the new passes, the rewritten end-to-end and property tests, and the fixes they found |
| `d35c5f8453`, then the legacy removal | The editor, the reference page, the recipes, the scenario edits, and the stateful-ID warning; then the old syntax removed |
| `15a4d521a2` to `5d6107d10e`, and the demo after | `fork` removed and names from `let` (the owner's decisions of 2026-10-16): the printer names nodes with `let` and keeps `as` for sinks, native lowering gives the silent nodes it invented derived IDs, and `let x = y` names a stream |

What changed from the plan:

- **Telemetry.** Normalization moves the two DogStatsD filters' drops from `true` to `false`, so their derived
  terminals are now `dsd_prefix_filter_blocklist_false` and `dsd_post_agg_filter_false`. Native series still report
  under the native component (`dsd_prefix_filter`, through the telemetry profiles), and the profiles count drops on
  `false`; only the program-mode `plan_*` series, keyed by node ID, see the new names.
- **Composition** puts an artifact's nodes in the default program's order, as it already did for edges, so a printed
  and parsed export composes to the default program's hash.
- **The duplicate analysis** reads `split(not p)` as `split(p)` with its ports swapped, which the oracle's random plans
  found was needed once normalization flips filters.
- **Every nested junction is parenthesized** when printed, `(a and b) or c` included, for readers.

## Known limits

- **A name follows the last stage of its `let`.** Appending a stage to `let metrics` makes the new stage `metrics`, and
  the stage it follows takes a derived ID. Where enrichment survives in the program (host tags on, or a full export),
  appending moves the reserved ID, and with it the native identity, from host tags or host enrichment to the new stage,
  and rebuilds that node's kernel. The demo's detached export leaves enrichment out (`let metrics =
  dsd_post_agg_filter`), so it does not meet this. Adding the stage to each use of `metrics` instead keeps every ID.
- **Sink IDs follow position.** A sink's ID comes from the port it hangs off, so reordering statements that use one
  stream renumbers its tee's ports, and a sink can take the ID, and the native identity, the default program gives
  another sink: put `let metrics = dsd_post_agg_filter` above the client-telemetry statement, and the main `emit`
  becomes `dsd_post_agg_filter_fanout_0`, which reports as the client-telemetry tap. Native identity keyed by node ID is
  what V10 replaces.

## Deferred

- `emit <name>` resolving to a recipient or an output by the host's directory (needs the catalog at parse time).
- A general expression language for rewrites (needs effect contracts for arbitrary rewrites).
- A `route` block for first-match routing tables.
