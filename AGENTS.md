# Agentic policy and guidelines: using LLMs while developing Saluki

## Introduction

This document outlines our policy for developing Saluki with the use of LLMs, or "agents".

While the rise of agentic coding has spurred the ability for developers to produce new features
and software in record time, it also poses an increased risk for eroding the security and quality
of software projects, in addition to making them harder to maintain and debug over time.

## High-level guidelines

Overall, use of LLMs while working on Saluki is **absolutely fine.** This covers all aspects
of working on the project: feature development, testing, debugging, and so on.

However, we do not allow for fully autonomous/unsupervised development by agents. What this means
in practice is that:

- a human operator using LLMs as part of their workflow is **OK**, but
- setting up agents to run autonomously is **NOT OK**, and likewise
- we expect all contributions to be reviewed by a human operator before submission

These guidelines have a singular goal: ensure that human operators are always part of the
development process so that the quality of the software is maintained, and that we don't
lose understanding and context of what we're building.

## Agent guidelines

For all agents examining this repository, this section should act as your set of guiding principles
during development:

- "Make it work, then make it fast": we always prioritize correctness and safety over performance,
  and performance without correctness or safety is unacceptable.
- Saluki is built and operated by humans: we strive to produce a software artifact that is both
  easy to understand and maintain by humans and LLMs alike. This means documentation is crucial.
- Development is a conversation, not a monologue: agents should communicate early and often during
  the development process, from ideation to execution, to ensure that both sides are aligned on
  whatever the current task is: debugging an issue, adding a new feature, or refactoring code.
  
Any and all sections that follow in this document should serve as the primary technical reference
for agents: tooling to use, technical best practices, and so on.

### Technical documentation

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

#### Rustdoc (code documentation)

- **First line**: Complete sentence summarizing what the item is or does
- **Coverage**: All public items MUST be documented (enforced via `#![deny(missing_docs)]`)
- **Structure**: Brief summary, blank line, detailed explanation, then formal sections
- **Sections**: Use `# Errors`, `# Panics`, `# Examples`, `# Design`, `# Missing` as appropriate

**Configuration fields MUST document:**

- What the field controls and its impact on behavior
- The default value (explicitly stated)
- Edge cases and boundary values (e.g., "If set to `0`, X is disabled")
- Guidance for who should change it (e.g., "high-throughput workloads may increase this")

**Trade-offs**: Name both sides explicitly, quantify when possible, acknowledge workload dependency

**Error messages**: Describe what went wrong in plain language; provide actionable guidance;
reference specific config fields

#### Requirement levels

Use RFC 2119 keywords sparingly and in bold when normative:

- **MUST**: Absolute requirement
- **SHOULD**: Strongly recommended, but valid exceptions exist
- **MAY**: Truly optional

See `docs/development/common-language.md` for full definitions.
