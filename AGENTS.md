# Agentic policy and guidelines: using LLMs while developing Saluki

## Introduction

This document outlines our policy for developing Saluki with the use of LLMs, or "agents".

While the rise of agentic coding has spurred the ability for developers to produce new features
and software in record time, it also poses an increased risk for eroding the security and quality
of software projects, in addition to making them harder to maintain and debug over time.

## Guiding Principles

- Top priorities are correctness, then performance, in that order.
- Code should be maintainable and human-readable.
- Documentation is crucial.
- Development is a conversation, not a monologue: agents should communicate early and often during
  the development process, from ideation to execution, to ensure that both sides are aligned on
  whatever the current task is: debugging an issue, adding a new feature, or refactoring code.

## Project Index

- `bin/agent-data-plane`: the primary artifact of this project. A binary that runs alongside the
  DataDog Agent.
- `lib/datadog-agent/`: Core Agent configuration and IPC communication protocols.
- `bin/correctness`: the framework for integration tests.
- `lib/`: the production frameworks and common code supporting `agent-data-plane`
- `docs/`: Human-oriented documentation.
- `test/`: Integration test cases.

### Gotchas

- `agent-data-plane` `standalone` mode is a vestige of earlier development and testing cycles. It is
  not for production use and supporting it need not be a blocker during feature development.
- We have customized our use of `cargo fmt` and `clippy`. The `Makefile` is authoritative.
- Our Rust code wraps at 120 characters.
- Datadog configuration inventory is managed by `lib/datadog-agent/config/schema/*.yaml` and code is
  generated from there. See `.claude/skills/config-management/SKILL.md` for details.

## Building and Testing

Use `./Makefile` to understand build commands.

### Checking your Work

Use these commands to check your Rust work. Each command is progressively deeper. Use Level 1 when
you are editing Rust code, then when you think you are done, progress through the additional levels.

- Level 1: `cargo check --workspace && cargo check --workspace --tests`: At first this may be
  specialized to the `--bin` or `--lib` you are working on, but run it on the whole workspace before
  moving on to the next level.
  - Run the locally relevant tests using `cargo nextest run`
  - Always use `make fmt` when you are done editing.
- Level 2: `make check-all` for lint checks.
- Level 3: `make test-all` to run all unit tests.

Level 4: At the user's discretion, proceed to integration testing. There are two integration test
suites:
- Name "integration": test definitions at `test/integration`.
- Name "correctness": test definitions at `test/correctness/cases`.
- The integration harness and libraries are in `bin/correctness`.

Check these facts against the Makefile as this is a fast-moving project.
- Suite "integration": `make test-integration`. Only use `make test-integration-quick` when we know that the images(s)
  we depend on are up-to-date with the current state of the codebase. Failure to rebuild the images can lead to a
  confusing experience for you and the user both.
- Suite "correctness": `make test-correctness` or one test case by name
  `make test-correctness-case CASE=dsd-mapper-blocklist`

Alternatively, `panoramic` can be invoked directly, for example if the user requests certain command-line options like
`--no-tui`. Examples:
- `cargo run --release --bin panoramic -- run -d test/correctness/cases --no-tui`
- `cargo run --release --bin panoramic -- run -d test/correctness/cases -t test-name-1 -t test-name-2`
- `cargo run --release --bin panoramic -- --help`

## Agent guidelines

Use of LLMs while working on Saluki is **absolutely fine.** This covers all aspects of working on the
project: feature development, testing, debugging, and so on.

However, we do not allow fully autonomous/unsupervised development by agents. What this means in
practice is that:

- a human operator using LLMs as part of their workflow is **OK**, but
- setting up agents to run autonomously is **NOT OK**, and likewise
- we expect all contributions to be reviewed by a human operator before submission

These guidelines have a singular goal: ensure that human operators are always part of the
development process so that the quality of the software is maintained, and that we don't
lose understanding and context of what we're building.

## Writing technical documentation

When writing or updating documentation, follow these guidelines. For full details, see
`docs/development/style-guide.md`.

#### Voice and tone

- **Perspective**: Use "you" for instructions, "we" for project-collective statements
- **Tone**: Write as a knowledgeable colleague—direct and practical, not curt or condescending
- **Active voice**: Make clear who performs actions ("Run the command" not "The command should be run")
- **Present tense**: Describe what things do, not what they will do
- **Avoid**: "please" in instructions, "simply/easy/just" (minimizes difficulty), excessive
  exclamation marks, jargon, pop culture references

#### Markdown documentation

- **Headings**: Sentence case ("Configure the source", not "Configure The Source"); never skip
  levels; H1 for page title only
- **Code formatting**: Backticks for code, commands, file paths, config keys, type names
- **Emphasis**: Bold for UI elements and key terms on first use; italics sparingly for introducing
  terms
- **Lists**: Numbered for sequential steps; bullets for non-sequential items
- **Conditions first**: "To enable debug mode, set `debug: true`" not "Set `debug: true` to enable
  debug mode"
- **Admonitions**: Use `> [!WARNING]`, `> [!NOTE]`, `> [!TIP]` for callouts
- **Links**: Descriptive text (not "click here"); relative paths for internal docs
- **Code blocks**: Always specify language; keep examples minimal and focused

#### Audience and scope

Generic saluki crates (`lib/saluki-*`, `lib/saluki-io`) are general-purpose infrastructure.
Their documentation and code comments **SHOULD NOT** reference:

- The Datadog Agent by name (use "the server process", "the remote endpoint", or the specific
  protocol instead)
- Internal Datadog codenames or project names ("Nitro Enclaves" is fine as a well-known AWS
  product, but internal Datadog project names are not)
- Datadog-specific deployment topologies (use generic terms like "guest VM", "host process", etc.)

This rule does NOT apply to `bin/agent-data-plane`, `lib/datadog-agent-commons`, configuration
registry annotations, or known-configs documentation — all of which are explicitly
Datadog Agent-specific.

#### Rustdoc (code documentation)

- **First line**: Complete sentence summarizing what the item is or does
- **Coverage**: All public items MUST be documented (enforced via `#![deny(missing_docs)]`)
- **Structure**: Brief summary, blank line, detailed explanation, then formal sections
- **Sections**: Use `# Errors`, `# Panics`, `# Examples`, `# Design`, `# Missing` as appropriate

**Configuration fields MUST document:**

- What the field controls and its impact on behavior
- The default value (explicitly stated)
- Edge cases and boundary values (for example, "If set to `0`, X is disabled")
- Guidance for who should change it (for example, "high-throughput workloads may increase this")

**Trade-offs**: Name both sides explicitly, quantify when possible, acknowledge workload dependency

**Error messages**: Describe what went wrong in plain language; provide actionable guidance;
reference specific config fields

#### Requirement levels

Use RFC 2119 keywords sparingly and in bold when normative:

- **MUST**: Absolute requirement
- **SHOULD**: Strongly recommended, but valid exceptions exist
- **MAY**: Truly optional

See `docs/development/common-language.md` for full definitions.
