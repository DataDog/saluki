# Agent guide

This page is the starting point for LLM agents working on Saluki, and for the humans directing them. It covers our
policy on agent use, then points to the guidance for each part of the development process. Read it before starting
any work, then read the linked pages relevant to your task.

## Policy

Use of LLMs while working on Saluki is **absolutely fine**. This covers all aspects of working on the project: feature
development, testing, debugging, and so on.

However, we don't allow fully autonomous or unsupervised development by agents. In practice:

- a human operator using LLMs as part of their workflow is **OK**
- setting up agents to run autonomously is **NOT OK**
- a human operator reviews every contribution before submission

These rules have a single goal: keep human operators part of the development process, so that we maintain the quality
of the software and don't lose understanding of what we're building.

## Guiding principles

- Top priorities are correctness, then performance, in that order.
- Code should be maintainable and human-readable.
- Documentation is crucial.
- Development is a conversation, not a monologue. Communicate early and often, from ideation to execution, so that you
  and the human operator stay aligned on the current task: debugging an issue, adding a feature, or refactoring code.

## Documentation lives under `docs/`

All documentation lives under `docs/`, and all development guidance lives under `docs/development/`. When you update
documentation or propose a new page or section, it **MUST** go there. Don't add READMEs or other Markdown files next to
code, and don't add guidance to the entrypoint files listed below. See
[Where documentation lives](./style-guide.md#where-documentation-lives) for the details.

### Entrypoints

Some files must stay where tooling expects to find them. They point into `docs/` instead of holding guidance:

| File | Read by | Contents |
|---|---|---|
| `AGENTS.md` | Agents that follow the `AGENTS.md` convention | Pointer to this page |
| `.claude/CLAUDE.md` | Claude Code | Imports this page |
| `.agents/skills/*/SKILL.md`, symlinked from `.claude/skills` | Agent skill discovery | Procedures; reference material lives in `docs/development/` |
| `codereview_guideline.md` files | The `/dd-review` workflow | Self-contained review prompts; see [Code review](./code-review.md) |
| `test/antithesis/AGENTS.md` | Agents working in `test/antithesis/` | Antithesis workflow notes |

## Project map

- `bin/agent-data-plane`: the primary artifact of this project, a binary that runs alongside the Datadog Agent
- `bin/correctness`: tools for correctness and integration tests
- `lib/`: production frameworks and common code supporting `agent-data-plane`
- `lib/datadog-agent/`: core Agent configuration and IPC protocols
  - `lib/datadog-agent/runtime`: the shared runtime for processes that act as a remote agent for the Datadog Agent,
    such as `agent-data-plane`: registration, configuration streaming, the workload provider, the status, flare, and
    telemetry services, the control plane workers, and the `RemoteAgent` startup facade
- `docs/`: all documentation, published as the project's docs site
- `test/`: integration, correctness, benchmark, and Antithesis test cases

## Checking your work

The `Makefile` is authoritative for build commands. These checks get progressively deeper: use level 1 while editing
Rust code, and work through the later levels when you think you're done.

1. `cargo check --workspace && cargo check --workspace --tests`. At first this may be narrowed to the `--bin` or
   `--lib` you're working on, but run it on the whole workspace before moving on. Run the relevant tests with
   `cargo nextest run`, and always run `make fmt` when you're done editing.
2. `make check-all` for lints, including Vale on documentation.
3. `make test-all` for all unit tests.

Use the `/panoramic` skill for correctness and integration testing.

## Topics

| Topic | Read when |
|---|---|
| [Style guide](./style-guide.md) | Writing any code or documentation |
| [Conventions](./conventions.md) | Making design choices; covers project-specific design rules and taste |
| [Common language](./common-language.md) | Interpreting requirement levels such as **MUST** and **SHOULD** |
| [Contributing](./contributing.md) | Writing commits, opening pull requests, adding release notes, and addressing review feedback |
| [Code review](./code-review.md) | Reviewing a change, or checking your own before opening a pull request |
| [Testing](./testing.md) | Choosing which kind of test to write, and running test suites |
| [Testing patterns](./testing-patterns.md) | Writing unit tests or property tests |
| [Configuration system](./configuration-system.md) | Working on configuration, the schema overlay, or the configuration inventory |
| [Plans](#plans) | Planning work that spans several pull requests |

For Antithesis work, read `test/antithesis/AGENTS.md`.

## Skills

Skills under `.agents/skills/` hold step-by-step procedures. Where a skill needs reference material, it links to a page
in this guide.

| Skill | Purpose | Reference |
|---|---|---|
| `binary-size-analysis` | Find what contributes to the size of the `agent-data-plane` binary | |
| `config-schema-update` | Update the vendored Datadog configuration schema | [Configuration system](./configuration-system.md) |
| `config-system` | Work on the configuration system | [Configuration system](./configuration-system.md) |
| `create-release-note` | Write a release note fragment | [Release notes](./contributing.md#release-notes) |
| `external-pr-ci-trigger` | Run CI for an external contributor's pull request (maintainers only) | |
| `panoramic` | Run correctness and integration tests and read their results | [Testing](./testing.md) |

## Plans

Write plans for work that spans several pull requests in `docs/development/plans/`, one file per effort. Plans aren't
published to the docs site and aren't linted. Keep a plan current as work lands, move anything of lasting value into
the relevant page of this guide, and delete the plan in the final pull request of the effort.
