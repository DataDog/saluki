# Style guide

Our style guide focuses on four main areas: **formatting**, **aesthetics**, **errors and logs**, and **documentation**.

While these are generally intertwined, formatting is almost entirely mechanical: we can consistently format code with
tools, reducing the mental burden on developers. Aesthetics, however, are more subjective and can't be easily automated.

A pull request number at the end of a rule points to the fix for a bug that the rule guards against.

## Formatting

Formatting primarily includes the formatting of Rust _code_, but we refer to it here more broadly: any mechanical
transformation that we can automate around the structure or ordering of code, configuration, and so on. We do this in
service on making the codebase consistent and easier to read, but also to reduce the mental overhead of worrying about
the styling of code as it's being written, as well as dealing with needless debate over _how_ to style. Developers run
the formatting tools, and the tools apply certain rules to the codebase.

Developers **MUST** use the **fmt** Make target (`make fmt`) to run all of the configured formatting operations on the
codebase as a whole. This target includes:

- running `cargo fmt` on the code (customized via `rustfmt.toml`)
- running `cargo autoinherit` to promote the use of shared workspace dependencies
- running `cargo sort` to sort dependencies within the `Cargo.toml` files

These formatters are checked and enforced in CI, so it's best to get in the habit of running `make fmt` locally before
committing changes. Rust code wraps at 120 characters.

We customize how `cargo fmt` and `cargo clippy` run, so the `Makefile` is authoritative: use its targets rather than
invoking the tools directly with default flags.

Additionally, we use `cargo clippy` to catch common mistakes and suboptimal code patterns. Clippy will generally show
suggestions of more idiomatic or succinct ways to write common code patterns, as well as potentially point out subtle
bugs with certain usages. While Clippy _does_ support being able to automatically apply suggested fixes, doing so
requires a clean working copy (no uncommitted changes), and we opt not to override that behavior as it could potentially
overwrite changes that the developer hasn't yet fully worked through.

As such, developers **MUST** run `make check-clippy` and apply the suggestions. Like the formatters, Clippy lints are
checked and enforced in CI.

## Aesthetics

Aesthetics are the more subjective areas of how we write and structure code in Saluki. We'll talk about a number of
different items below, which range from very specific to more broad. We don't expect everyone to remember all of these,
especially since they're not easily automated, and so you might find us calling them out in PR reviews if missed.

### Generic types and type bounds

You'll often encounter types that use generics. These can vary in complexity and size, and can be applied on types,
trait implementations, method signatures, so on. In order to try and keep things clean and readable, we try to use
generics in the following way:

```rust
// Types and traits MUST use `where` clauses to introduce type parameters:
pub struct MyType<T>
where
    T: SomeTrait,
{
}

pub trait MyTrait<T>
where
	T: SomeTrait,
{
}

// Method signatures SHOULD use `where` clauses to introduce type parameters,
// but using `impl Trait` notation is also acceptable, especially when being
// used to convert arguments to concrete types:
fn my_method<F>(&self, frobulator: F) -> T
where
	F: Frobulator,
{
	frobulator.frobulate();
}

fn set_name(&mut self, name: impl Into<String>) {
	self.name = name.into();
}
```

### API design

Keep each API small, and name each item for what it does:

<!-- review:api-design -->
- Conversion methods follow the Rust API Guidelines: `as_` is a cheap borrow, `to_` is an expensive or owned
  conversion, and `into_` consumes `self`. To give callers owned access to the internals, add an `into_parts(self)`
  method.
- A change can keep a public signature and still change the result, as when `EventD::hostname()` started to return
  `Some("")`. Search the repository for each caller. Flag a caller that the new result breaks, such as one that
  expects `Some` to hold a hostname that isn't empty. Put the comment on the changed item, and give the caller's path.
  If the description doesn't mention the new result, ask whether the author intends it.
- A setter named `set_foo` takes `&mut self`. A builder method named `with_foo` takes `self` and returns `Self`.
- A name says what the item does. For example, a function that spawns a task is `spawn_*`, not `new_*`. Method names
  use full words, and variable names can use abbreviations.
- Use an enum, not strings, for a known set of values. An `Other(String)` variant can hold the values that the enum
  doesn't know. Take a typed value, such as a `VsockAddr`, not a string that the code parses.
- When a consumer must tell an unset value from an empty or zero one, use `Option<T>`, not a sentinel value. (Events
  used an empty `MetaString` to mean an unset hostname, so ADP overwrote explicitly empty hostnames: #3076.)
- Flag parts that the code doesn't need:
  - a wrapper, trait, struct, or task that adds nothing, or a struct that the caller builds only to split it into its
    fields again
  - an `async fn` that never awaits, unless it implements a trait method that is `async`
  - an `Arc` or a `Mutex` around a value that nothing shares, or that's already safe to share
  - an `Option` parameter that the pull request adds when every caller passes `Some`
  - in a library crate, a new `pub` item that nothing outside the crate uses. A `pub` method on a `pub(crate)` type
    is fine, and so is an item that a stacked pull request uses.
- Prefer the standard library to a new crate, for example `std::sync::LazyLock` instead of `once_cell`. When the
  workspace already uses a major version of a dependency, use that version.
<!-- /review -->

### Saluki and ADP layering

<!-- review:layering -->
The generic Saluki crates (`lib/saluki-*`) are general-purpose infrastructure. Their names, docs, code comments, log
messages, and metric names **SHOULD NOT** refer to:

- the Datadog Agent, Agent Data Plane (ADP), their configuration keys, or Agent concepts such as Agent flares and the
  Remote Agent Registry:
  name the specific protocol, or use a generic term such as "diagnostics" or "the remote endpoint"
- internal Datadog code names or project names: well-known public products, such as AWS Nitro Enclaves, are fine
- Datadog-specific deployment topologies: use generic terms such as "guest VM" or "host process"

The name of a protocol or a feature that the crate implements, such as DogStatsD or tracer flares, is fine. In code
that implements a Datadog protocol or format, such as the DogStatsD codec or the Datadog encoders, a comment can say
which Datadog Agent behavior the code matches. Generic infrastructure, such as networking, logging, and configuration
loading, has no such exception.

This rule doesn't apply to `bin/agent-data-plane`, `lib/datadog-agent/`, `lib/agent-data-plane-config*`, or
`docs/agent-data-plane/`, which are specific to the Datadog Agent.

In a review, put all wording that breaks this rule into one comment at the first site. Start it with "Not blocking:"
and list the other sites. This replaces the usual rule for repeated issues, so don't comment at the other sites.

Logic that only the Datadog Agent needs belongs in `bin/agent-data-plane`, `lib/datadog-agent/`, or
`lib/agent-data-plane-config*`. Flag an option, a special case, or a workaround that a `lib/saluki-*` crate adds for
one caller. Report it in a comment of its own.
<!-- /review -->

In any crate, a test or a comment can link to upstream source pinned to a commit, to show where a behavior comes from.
Describe the behavior itself in the text around the link.

### Imports, layout, and naming

No formatter or lint checks these conventions, so reviewers point them out as nits:

<!-- review:nits -->
- Don't repeat the enclosing module or type in a name: `eventd::Priority`, not `eventd::EventPriority`. File paths
  follow the same rule: a `datadog-metrics-v3` directory under `lib/datadog-agent/` repeats a word, but a crate name
  can keep the prefix.
- Import an item instead of writing its fully qualified path. If two imports conflict, import the parent module.
- Import child modules with `self::`.
- Put `mod` declarations below the block of external `use` imports. A `mod foo;` line directly above its
  `use self::foo::...` line is fine.
- Put constants at the top of the file, below the imports.
- Put the doc comment above the attributes of an item.
- Write an `expect` message as what the code expects, such as `expect("should not fail to set item on dict")`, not as
  the failure, such as `expect("Could not set name")`.
- Derive `Copy` on small enums without fields. Return a `&'static str` from a method instead of allocating a `String`
  for a fixed name.
- In the root `Cargo.toml`, set `default-features = false` on each dependency. Each crate enables the features that it
  uses.
<!-- /review -->

## Errors and logs

Operators read errors and logs to find out what went wrong, so we write both for them.

### Error handling

Return an error instead of a panic, and keep the cause of each failure:

<!-- review:error-handling -->
In tests and harness code, flag only an ignored error that lets a test pass when it should fail.

- Flag a new `unwrap()`, `expect()`, `panic!`, `unreachable!`, `assert!`, or panicking index when its assumption can
  break. Input, configuration, timing, or a wrong estimate can break it. Timing includes a late response and a lagging
  `broadcast` receiver. Where the code can skip the bad input and count it, it does that instead. (A size estimate
  that asked for a flush before the first write made the encoder panic: #1310.)
- Untyped errors use `saluki_error::GenericError`, with context from `error_context` or `with_error_context` in
  `saluki_error::ErrorContext`. Typed errors use `snafu`. Flag `anyhow` types used directly, `Box<dyn Error>` as a new
  error type, and `String` as an error type. A trait bound that needs `Box<dyn Error>` is fine.
- Keep the source error. Flag `map_err(|_| ...)` when it drops an I/O, parse, TLS, or other error that the operator
  needs.
- Flag code that formats the source into a new error and drops it, such as `generic_error!("...: {e}")`. Use
  `with_error_context` for a `GenericError`, or a snafu context selector with a `source` field for a typed error. A
  log field such as `error = %e` shows only the outermost context of a `GenericError`. So don't flag the formatted
  source when a caller logs the error that way.
- Flag an ignored error, such as `.ok()` or `let _ =`, when the failure loses data or the process needs the result.
  (ADP ignored an unreadable `datadog.yaml`, so a later failure didn't show the cause: #981.)
- An error message, or a log line about a failure, says what went wrong in plain words, not with function names.
  When a configuration key, a file or socket path, or an endpoint is involved, the message names it. An error names
  it in its text, and a log line names it in a structured field. Give an endpoint as the full URI with its port. When
  the operator can fix the problem, the message says how.
<!-- /review -->

For example, the first message below names the configuration keys and says how to fix the problem. The second names
the address and its key, and keeps the I/O error in its `source` field. It also shows the I/O error in its text, so a
log line with `error = %e` shows the cause.

```rust
#[derive(Debug, Snafu)]
enum Error {
    #[snafu(display(
        "No listeners configured. Specify a port (`dogstatsd_port`) or \
         socket path (`dogstatsd_socket`) to enable a listener."
    ))]
    NoListenersConfigured,

    #[snafu(display("Failed to bind to {address} (`dogstatsd_port`): {source}"))]
    BindFailed {
        address: SocketAddr,
        source: std::io::Error,
    },
}
```

### Log messages

Reviewers check new and changed log lines against these rules:

<!-- review:log-messages -->
- A log message is a full sentence that starts with a capital letter and ends with a period. Put values such as
  addresses, names, and counts in structured fields, not in the message text. For example:
  `debug!(%endpoint, "Connected to endpoint.")`. Report a break of this rule as a nit.
- Choose the level by the effect on the operator. Use `error` only when an operation failed. Use `warn` for a
  condition that needs someone's attention. Use `debug` for a fallback that loses no data.
- Flag logging at any level that runs once for each event.
- Flag a `warn!` or `error!` that can repeat for each connection or entity with no bound. Count the case with a
  metric and log the details at `debug`, or log once for each key. (A warning for each tagger entity with an
  unhandled ID prefix grew to over 1.1 billion log lines each week: #2353.)
- Log diagnostics with the `tracing` macros, not `println!` or `eprintln!`. Tests, build scripts, and command-line
  output can print.
<!-- /review -->

## Documentation

Documentation is crucial to Saluki's mission of producing reliable, maintainable software. This section covers
guidelines for both prose documentation (Markdown) and code documentation (rustdoc).

### Where documentation lives

All documentation lives under `docs/`, and all development guidance lives under `docs/development/`. New pages and
sections **MUST** go there, whether you're updating an existing page or proposing a new one. Don't add new READMEs or
other Markdown files next to code. Add every new page to the sidebar in `docs/.vitepress/config.mts`, and preview the
site locally with `make run-docs`.

The only exceptions are entrypoint files whose location is fixed by tooling, such as `AGENTS.md`,
`.claude/CLAUDE.md`, skills under `.agents/skills/`, and `codereview_guideline.md` files. Keep these as thin pointers
into `docs/` wherever the tooling allows. The `codereview_guideline.md` files must be self-contained, so we generate
them from marked sections of `docs/development/` instead. [Agent guide](./agents.md#entrypoints) lists them.

### Voice and tone

Write as a knowledgeable colleague—direct and practical, without being curt or condescending. Documentation should feel
like guidance from someone who genuinely wants to help you succeed.

**Perspective:**

- Use "you" when addressing the reader: "You can configure the buffer size..."
- Use "we" when referring to the project collectively: "We use RFC 2119 requirement levels..."

**What to avoid:**

- "Please" in instructions: tell the reader what to do ("Run `make fmt`" instead of "Please run `make fmt`")
- Excessive exclamation marks: reserve them for genuinely exciting announcements
- Jargon and buzzwords without explanation

### Writing style

We write code comments, rustdoc, log messages, error messages, and Markdown docs in the style of ASD-STE100, also
called Simplified Technical English (STE). This applies to every comment, including `//` comments and rustdoc on
private items. You don't have to use the STE dictionary. Follow these rules:

1. Write one topic in each sentence. Use 20 words or fewer in an instruction, and 25 words or fewer in a description.
   Count a code item as one word. Use six sentences or fewer in a paragraph.
2. Use the active voice, and name who or what does the action: "The source sends events to the transform," not
   "Events are sent to the transform by the source."
3. Use the present tense to say what code does: "Returns `None`," not "Will return `None`."
4. Write instructions as commands: "Register all components before you call this method."
5. Use one word for one concept. For example, don't change between "drop," "discard," and "throw away" for the same
   action.
6. Use simple words: "to," not "in order to"; "use," not "utilize." Remove filler words such as "basically,"
   "simply," "just," "obviously," "Note that," and "It is important to."
7. Write "that is" and "for example" instead of "i.e." and "e.g."
8. Keep the articles ("a," "the"). Don't write in note style.
9. Use a list for three or more conditions, steps, or items.
10. Don't use idioms, jokes, or pop-culture references.

Rustdoc follows the same rules, in full sentences with end punctuation:

- The first line is one sentence that says what the item is or does. Details and edge cases come after a blank line.
- For a method that returns a `bool`, start with "Returns `true` if" and the condition. For a `bool` field or
  setting, start with "Whether" and the condition.
- A configuration field ends with a sentence that states its default, such as "Defaults to 1000."

This section covers how to word a comment. For what a comment should say, see
[Code comments](./conventions.md#code-comments).

#### Examples

These examples come from the Saluki codebase.

Future tense, passive voice, and filler, in `lib/saluki-env/src/workload/helpers/cgroups.rs`:

```rust
// Before
/// This method will attempt to find the cgroup for the given process ID by looking at the `/proc/<pid>/cgroup`
/// file. If the process ID doesn't exist or isn't attached to a cgroup, `None` will be returned.

// After
/// Reads `/proc/<pid>/cgroup` to find the cgroup of the process.
///
/// Returns `None` if the process doesn't exist or isn't in a cgroup.
```

"In order to," passive voice, and a sentence of 40 words, in
`lib/saluki-core/src/data_model/event/metric/context/resolver.rs`:

```rust
// Before
/// In order to resolve a context, `resolve` must be called which requires taking a lock to check for an existing
/// context. A read/write lock is used in order to prioritize lookups over inserts, as lookups are expected to be more
/// common given how often a given context is used and resolved.

// After
/// `resolve` takes a lock to look for an existing context. We use a read/write lock because lookups happen much more
/// often than inserts.
```

"Note that," future tense, and an instruction with no clear actor, in `lib/saluki-core/src/health/mod.rs`:

```rust
// Before
/// Note that components can be registered while this method is waiting, which will influence how long this method
/// takes to return. Callers should ensure that all components have been registered before calling this method.

// After
/// A component that registers while this method waits makes the wait longer. Register all components before you call
/// this method.
```

Long sentences that hide the rule, in `lib/saluki-components/src/sources/dogstatsd/mod.rs`:

```rust
// Before
/// When metrics are sent with a sample rate _lower_ than this value then it will be clamped to this value. This is
/// done in order to ensure an upper bound on how many equivalent samples are tracked for the metric, as high sample
/// rates (very small numbers, such as `0.00000001`) can lead to large memory growth.

// After
/// A metric with a lower sample rate gets this sample rate instead. This limits the number of equivalent samples that
/// we track for one metric: a very small sample rate, such as `0.00000001`, can use a large amount of memory.
```

### Markdown documentation

This section covers documentation written in Markdown files, such as those in the `docs/` directory.

#### Structure

- **Headings**: Use sentence case for all headings ("Configure the source" instead of "Configure The Source")
- **Heading levels**: Use H1 (`#`) for page title only; start content headings at H2 (`##`); never skip levels
- **Lists**: Use numbered lists for sequential steps; use bullet lists for non-sequential items
- **Paragraphs**: Keep paragraphs focused on a single idea; break up walls of text

#### Formatting

- **Code**: Use backticks for code, commands, file paths, configuration keys, and type names
- **Bold**: Use for UI elements and emphasis of key terms on first use
- **Italics**: Use sparingly for introducing technical terms
- **Conditions first**: Place conditions before instructions: "To enable debug mode, set `debug: true`" rather than "Set
  `debug: true` to enable debug mode"

#### Admonitions

Use GitHub-style admonitions for callouts:

```markdown
> [!NOTE]
> Supplementary information the reader should be aware of.

> [!TIP]
> Optional guidance to help readers be more successful.

> [!WARNING]
> Critical information about potential problems or pitfalls.
```

#### Code blocks

- Always specify the language identifier for syntax highlighting
- Keep examples minimal and focused on the concept being illustrated
- Use comments within code blocks to explain non-obvious parts
- Prefer runnable examples over pseudo-code when possible

#### Links

- Use descriptive link text that makes sense out of context: "see the [architecture overview](../reference/architecture/)"
  rather than "[click here](../reference/architecture/)"
- Use relative paths for internal documentation links
- Link to relevant sections on first mention of concepts defined elsewhere

### Rustdoc (code documentation)

This section covers documentation written in Rust source files using doc comments.

#### Module documentation

Module-level documentation uses `//!` comments and appears at the top of the file:

```rust
//! Core primitives for building data planes.
//!
//! This crate provides the foundational types and traits used throughout
//! the project, including the topology builder and event model.

#![deny(missing_docs)]
```

- Begin with a one-sentence purpose statement
- Add a blank line, then provide detailed explanation if needed
- All public items **MUST** be documented: use `#![deny(missing_docs)]` in library crates to enforce this

#### Item documentation

Item-level documentation uses `///` comments and documents types, traits, functions, and other items:

```rust
/// A centralized store for resolved contexts.
///
/// Contexts combine a name and a set of tags to identify a specific metric
/// series. As contexts are constructed entirely of strings, they are expensive
/// to construct and compare. This store provides interning and caching to
/// reduce these costs.
///
/// # Design
///
/// The resolver maintains an interner for strings and a cache for fully
/// resolved contexts. When caching is enabled, repeated resolutions of the
/// same context return a reference to the cached value.
pub struct ContextResolver { /* ... */ }
```

**Structure:**

1. **First line**: A complete sentence summarizing what the item is or does
2. **Body**: Detailed explanation, design rationale, and usage notes (after a blank line)
3. **Sections**: Formal sections using Markdown headers (see below)

#### Documentation sections

Use these sections as applicable:

- `# Errors`—Document when errors are returned and what they mean
- `# Panics`—Document conditions that cause panics
- `# Examples`—Provide runnable examples for public APIs
- `# Design`—Explain architectural decisions for complex items
- `# Missing`—Document known limitations or planned future work

Example with an `# Errors` section:

```rust
/// Adds a source component to the blueprint.
///
/// The source will be validated and registered with the topology graph.
///
/// # Errors
///
/// Returns an error if the component ID is invalid or if the component
/// cannot be added to the graph due to a conflict.
pub fn add_source(&mut self, id: &str, source: Source) -> Result<(), Error> {
    // ...
}
```

#### Configuration documentation

Configuration structs require thorough documentation. Each field **MUST** document:

- What the field controls and its impact on behavior
- The default value (stated explicitly)
- Edge cases and boundary values: for example, "If set to `0`, idle counter tracking is disabled"
- Guidance for who should consider changing it: for example, "High-cardinality workloads may need to increase this"

```rust
/// Aggregate transform configuration.
///
/// Aggregates metrics into fixed-size windows, flushing them at regular
/// intervals.
///
/// ## Zero-value counters
///
/// When counters are flushed, they are tracked as "zero-value" counters.
/// If not updated again, the transform emits a zero value until the counter
/// expires. This provides continuity for sparse counter updates.
#[derive(Deserialize)]
pub struct AggregateConfig {
    /// Maximum number of contexts to aggregate per window.
    ///
    /// A context is the unique combination of a metric name and its tags.
    /// When the limit is reached, additional metrics are dropped until the
    /// next window starts.
    ///
    /// High-cardinality workloads may need to increase this value.
    ///
    /// Defaults to 1000.
    #[serde(default = "default_context_limit")]
    context_limit: usize,

    /// How long to keep idle counters alive after flush, in seconds.
    ///
    /// Idle counters emit zero values during each flush until they expire
    /// or receive an update. This prevents gaps in sparse time series.
    ///
    /// Set to `0` to disable idle counter tracking.
    ///
    /// Defaults to 300 (5 minutes).
    #[serde(default = "default_expiry")]
    counter_expiry_seconds: u64,
}
```

#### Trait documentation

Traits should explain their purpose, how they fit into the system, and provide examples of typical implementations:

```rust
/// A source of events for the topology.
///
/// Sources are the entry points where events are ingested from external
/// systems or generated internally. They run until shutdown is signaled
/// or an error occurs.
///
/// Examples of sources include DogStatsD receivers, file readers, and
/// internal metrics collectors.
#[async_trait]
pub trait Source {
    /// Runs the source until completion.
    ///
    /// The context provides access to the forwarder for sending events
    /// downstream, as well as shutdown signaling.
    ///
    /// # Errors
    ///
    /// Returns an error if an unrecoverable failure occurs during operation.
    async fn run(self: Box<Self>, context: SourceContext) -> Result<(), Error>;
}
```

### Trade-off documentation

When documenting performance, memory, or design trade-offs, be explicit about what's being traded and why:

- **Name both sides**: "This represents a trade-off between throughput and memory usage"
- **Quantify when possible**: "Approximately 150–200 bytes per context based on typical metric names and tags"
- **Acknowledge variability**: "The optimal value is workload-dependent"
- **Provide guidance**: "A good starting point is 1 MB per 5000 unique contexts"

Example:

```rust
/// Sets whether to enable context caching.
///
/// Caching speeds up repeated context resolutions but increases memory
/// usage. The cache grows with the number of unique contexts and does not
/// shrink, even after expiration frees entries.
///
/// Disabling caching reduces average memory usage but decreases memory
/// determinism: each resolution allocates, so resolving the same context
/// ten times results in ten allocations.
///
/// Defaults to enabled.
pub fn with_caching(mut self, enabled: bool) -> Self {
    // ...
}
```

### Reviewing comments and docs

Reviewers check the comments and docs that a pull request adds or changes:

<!-- review:comments-and-docs -->
We write comments and docs in the style of ASD-STE100 (Simplified Technical English), without its dictionary. Check
each comment and doc that the pull request adds or changes. Flag these problems, and start each finding with "Not
blocking:":

- A comment that describes a change, such as "previously," "now uses," or "replaces X." Comments describe the current
  code, and Git keeps the history. A regression test can describe, in the present tense, the failure that it guards
  against, and link its issue.
- A comment that restates the code, the signature, or Rust basics. A comment gives a caveat or a reason that the code
  can't show.
- Banner comments, such as `// ---- Section ----`, outside tests.
- Markdown design notes or proof-of-concept files, `TODO: Document` placeholders, and work-in-progress wording. A
  sentence that explains how a module is laid out is fine.
- A `TODO` that doesn't say what is missing. Known gaps in a type go in a `# Missing` section of its rustdoc.
- Rustdoc that doesn't start with one sentence that says what the item is or does. A fallible public function has an
  `# Errors` section. A configuration field says what it controls and its edge cases, and ends with a sentence that
  states its default.
- Docs that give implementation details that callers don't need.

Also check the wording of new and changed text against these rules:

- A sentence has 25 words or fewer, and a paragraph has six sentences or fewer.
- The active voice names who or what acts. Passive voice is a problem only when the reader can't tell who acts.
- The present tense describes what the code does now, not "will return."
- No filler, such as "in order to," "basically," "simply," or "Note that."
- No "i.e." or "e.g." in `//` comments. Vale already checks rustdoc and Markdown for these.

Report wording problems in one "Nit:" comment, for the file with the most problems, and list the other files in it.
Give rewrites as plain text with line numbers, and use a suggestion block only when it covers every replaced line. A
rewrite keeps every fact. Skip text that the pull request didn't change or only moved, and text that already follows
these rules.

Also:

- Items that a crate exports in its public API **MUST** have rustdoc. Module docs use `//!` at the top of the module.
- If the pull request changes behavior but not the docs, comments, or doc strings that describe it, flag it.
- Reference documentation under `docs/reference/` describes how the code works today. Flag aspirational or inaccurate
  descriptions there.
- If the pull request adds a release note, check that it describes the observable effect and matches the code.
<!-- /review -->

### Requirement levels

We use RFC 2119 requirement levels to clarify expectations. See [Common understanding](./common-language.md) for full
definitions. In brief:

- **MUST**: Absolute requirement
- **SHOULD**: Strongly recommended, but valid exceptions may exist
- **MAY**: Truly optional

Use these terms sparingly and format them in bold when using them normatively.
