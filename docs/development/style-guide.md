# Style Guide

Our style guide focuses on three main areas: **formatting**, **aesthetics**, and **documentation**.

While these are generally intertwined, formatting is almost entirely mechanical: we can consistently format code with
tools, reducing the mental burden on developers. Aesthetics, however, are more subjective and can't be easily automated.

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
committing changes.

Additionally, we use `cargo clippy` to catch common mistakes and suboptimal code patterns. Clippy will generally show
suggestions of more idiomatic or succinct ways to write common code patterns, as well as potentially point out subtle
bugs with certain usages. While Clippy _does_ support being able to automatically apply suggested fixes, doing so
requires a clean working copy (no uncommitted changes), and we opt not to override that behavior as it could potentially
overwrite changes that the developer has not yet fully worked through.

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

## Documentation

Documentation is crucial to Saluki's mission of producing reliable, maintainable software. This section covers
guidelines for both prose documentation (Markdown) and code documentation (rustdoc).

### Voice and tone

Write as a knowledgeable colleague—direct and practical, without being curt or condescending. Documentation should feel
like guidance from someone who genuinely wants to help you succeed.

**Perspective:**

- Use "you" when addressing the reader: "You can configure the buffer size..."
- Use "we" when referring to the project collectively: "We use RFC 2119 requirement levels..."

**What to avoid:**

- "Please" in instructions—just tell the reader what to do: "Run `make fmt`" not "Please run `make fmt`"
- Terms like "simply", "easy", "just", or "obviously"—they minimize difficulty and can frustrate readers who find
  something challenging
- Excessive exclamation marks—reserve them for genuinely exciting announcements
- Jargon and buzzwords without explanation
- Pop culture references and idioms that may not translate globally

**Active voice and present tense:**

Use active voice to make clear who performs actions:

- Good: "The source sends events to the transform"
- Avoid: "Events are sent by the source to the transform"

Use present tense to describe what things do:

- Good: "This method returns the current count"
- Avoid: "This method will return the current count"

### Markdown documentation

This section covers documentation written in markdown files, such as those in the `docs/` directory.

#### Structure

- **Headings**: Use sentence case for all headings ("Configure the source", not "Configure The Source")
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
- Use `#![deny(missing_docs)]` in library crates to enforce documentation coverage

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
3. **Sections**: Formal sections using markdown headers (see below)

#### Documentation sections

Use these sections as applicable:

- `# Errors` — Document when errors are returned and what they mean
- `# Panics` — Document conditions that cause panics
- `# Examples` — Provide runnable examples for public APIs
- `# Design` — Explain architectural decisions for complex items
- `# Missing` — Document known limitations or planned future work

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
- Edge cases and boundary values
- Guidance for who should consider changing it

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

When documenting performance, memory, or design trade-offs, be explicit about what is being traded and why:

- **Name both sides**: "This represents a trade-off between throughput and memory usage"
- **Quantify when possible**: "Approximately 150–200 bytes per context based on typical metric names and tags"
- **Acknowledge variability**: "The optimal value is workload-dependent"
- **Provide guidance**: "A good starting point is 1MB per 5000 unique contexts"

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

### Error messages

Error messages should guide users toward solutions. When writing error types and messages:

- **Describe what went wrong** in plain language
- **Provide actionable guidance** when possible
- **Reference specific configuration** fields or values

Example:

```rust
#[derive(Debug, Snafu)]
enum Error {
    #[snafu(display(
        "No listeners configured. Specify a port (`dogstatsd_port`) or \
         socket path (`dogstatsd_socket`) to enable a listener."
    ))]
    NoListenersConfigured,

    #[snafu(display("Failed to bind to port {port}: {source}"))]
    BindFailed {
        port: u16,
        source: std::io::Error,
    },
}
```

### Requirement levels

We use RFC 2119 requirement levels to clarify expectations. See [Common understanding](./common-language.md) for full
definitions. In brief:

- **MUST**: Absolute requirement
- **SHOULD**: Strongly recommended, but valid exceptions may exist
- **MAY**: Truly optional

Use these terms sparingly and format them in bold when using them normatively.
