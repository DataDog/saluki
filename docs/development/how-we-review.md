# How we review

This page describes how we review pull requests in Saluki: what we look for, in which order, and how we write review
comments. It applies to human and AI reviewers. The instructions at the end of the page make up the start of the root
`codereview_guideline.md`, which the AI reviewers read.

We based this page on about 1,600 review comments from April 2024 to November 2026, and on the outcome of the AI
reviews that ran on the same pull requests.

## What we look for

We rank problems by their effect on the people who run Agent Data Plane:

1. **Correctness and data loss.** Silent drops, lost data at shutdown, configuration that means something different
   from what it says, missed notifications, and deadlocks.
2. **Datadog Agent parity.** Agent Data Plane matches what users observe from the Datadog Agent: output, defaults,
   accepted input, and behavior on bad input. We ask what the Agent does, and the answer comes from Agent source. We
   don't copy Go mechanics or Agent internals that don't fit Tokio or Saluki.
3. **Cost on per-event paths, and bounded memory.** No avoidable allocations, clones, or logs per event. No unbounded
   channels, caches, or cardinality.
4. **Fit with the architecture.** Reuse the mechanisms that we already have, keep the `lib/saluki-*` crates generic,
   and follow the component model.
5. **A small, honest API.** Remove what isn't needed: structs, `async`, `Arc` and `Mutex`, `pub` items, and
   speculative code. Ask what a new mechanism adds over the one that exists.
6. **Signals that operators can trust.** Metrics that count what their names say, logs at the right level that read
   as sentences, and errors that name the configuration key and the fix.
7. **Comments and docs that describe the present.** Plain wording, no narration of changes, and no restated code.
8. **Tests that protect production behavior.** Fewer, better tests that fail when the behavior breaks.
9. **Scope.** Large refactors and polish go to follow-up pull requests or to a `TODO` that says what is missing.

Security problems rank with correctness.

## What comes up most

These topics came up most often in past reviews, in order of frequency:

| Topic | Example |
|---|---|
| The content and shape of comments and rustdoc | [#1531](https://github.com/DataDog/saluki/pull/1531#discussion_r3164001132) |
| Code, indirection, `async`, or `pub` that nothing needs | [#858](https://github.com/DataDog/saluki/pull/858#discussion_r2283116383) |
| Log levels, log wording, and what a metric means | [#691](https://github.com/DataDog/saluki/pull/691#discussion_r2096113272) |
| Allocations and clones on per-event paths | [#1054](https://github.com/DataDog/saluki/pull/1054#discussion_r2614467073) |
| Datadog Agent parity | [#1811](https://github.com/DataDog/saluki/pull/1811#pullrequestreview-4423123946) |
| Names and API shape | [#890](https://github.com/DataDog/saluki/pull/890#discussion_r2322837888) |
| Reuse of existing helpers and mechanisms | [#3044](https://github.com/DataDog/saluki/pull/3044#discussion_r4528955626) |
| Tests | [#2185](https://github.com/DataDog/saluki/pull/2185#discussion_r3659971804) |
| Configuration surface and meaning | [#180](https://github.com/DataDog/saluki/pull/180#discussion_r1715511846) |
| Concurrency and component lifecycle | [#1946](https://github.com/DataDog/saluki/pull/1946#discussion_r3501007026) |

Most real bugs that reviews caught were in configuration meaning, lifecycle and concurrency, Agent parity, payload
limits, and per-event cost.

## How we write review comments

- **Short.** Most comments are one to three sentences, such as "Why are we cloning here?" or the rename
  `HashMap` -> `FastHashMap`. A long comment proposes a different design, and shows the shape of the code.
- **With the reason.** The reason follows the request in one clause, and it names the cost: an allocation per
  packet, a clock read per metric, a crash loop.
- **"We" for project rules.** We state a convention as a fact: "We avoid fully qualified paths here." Personal
  preference is "I would," and an optional idea is "take it or leave it."
- **Questions about intent and evidence.** "What does this add over the existing eviction?" "Is this how the Core
  Agent does it?" A real question gets a real answer, and the reviewer accepts it.
- **Concrete fixes.** A suggestion block for a mechanical fix, and the path of existing code to copy.
- **Severity in words.** A comment that doesn't block starts with "Nit:" or "Not blocking:" or says that there is
  nothing to change in this PR. The review body sizes the ask:
  "Two small things, but otherwise looking good."

Most small pull requests get an approval with no comment.

## What we don't flag

- Behavior that looks odd but matches the Datadog Agent, once the author links the Agent source.
- Inefficiency off the per-event path. A `TODO` that says what to improve is enough.
- Comments that are accurate and clear, but worded differently from how we would word them.
- Release notes for beta features.
- Races that need an implausible sequence of events.
- CI failures that the pull request didn't cause.
- Direct tests of trivial logic.

## How we disagree

- We give way to evidence, especially Agent source.
- We hold firm on runtime correctness and per-event cost.
- When both sides have a point, we split the difference, and we record decisions made outside the pull request in
  the thread.
- We say so when we were wrong, and we fix our own mistakes in the framework rather than asking the author to work
  around them.

## How we treat AI review

An AI finding is input, like any other review comment. We agree with it, or we dismiss it with a reason: we don't
apply nits by reflex, and we don't resolve threads in silence. [Contributing](./contributing.md#responding-to-ai-review)
covers the author's side.

We also watch for the marks that coding agents leave: comments that narrate a change, banner comments, Markdown design
notes, and tests of test helpers.

## Instructions for AI reviewers

These sections make up the start of the root `codereview_guideline.md`.

### Role and verdict

<!-- review:preamble -->
You review pull requests for Saluki the way a maintainer does. Saluki is a Rust toolkit for building observability
data planes. Its main binary, `bin/agent-data-plane` (ADP), runs next to the Datadog Agent.

Rank findings by impact. Correctness, data loss, and security come first. Performance comes next, then
maintainability. Flag only issues that this pull request introduces and that the author can fix. Cite the exact file
and line range, and comment only on lines that are in the diff. A line number is the number of the line in the new
version of the file, as the `+` side of each hunk header counts it, not a line number inside `diff.patch`. Use the
tools to check each line number, because the review is rejected if a comment names a line that isn't in the diff.

Most small pull requests need no comments. An empty review with a verdict is a normal result.

Write the review body as a short verdict of three sentences or fewer. Say "patch is correct" or "patch is incorrect,"
give the number of blocking findings, and give the main reason. Don't describe how you reviewed the change. Don't list
what you checked and found correct.
<!-- /review -->

### How to write a finding

<!-- review:writing-findings -->
- Put one issue in each comment. State the problem or the request first, and give the reason in one clause. For
  example: "This allocates a `String` for every packet. Use `MetaString::from_static` instead."
- Keep a comment to three sentences or fewer. Write more only to propose a different design, and then show the shape
  of the code.
- When the fix is mechanical, such as a rename, a doc comment, or a log message, give a suggestion block. For a rename,
  "`old_name` -> `new_name`" is enough.
- Make sure that a suggestion block compiles. Check for unused imports and unreachable match arms yourself: CI runs
  Clippy with warnings as errors, so one warning breaks the build. Don't give conditional instructions such as "if
  nothing else uses this import, remove it."
- Name the existing code to reuse or to copy, and give its path.
- Use "we" for project conventions. For example: "We avoid fully qualified paths here."
- When you can't confirm a fact from the diff or the repository, ask one direct question. For example: "Does the Core
  Agent also drop the payload here?" Don't write "worth double-checking," "as I read it," or "please confirm."
- When the same issue occurs at several lines, explain it at the first line. At each other line, write one sentence
  that refers to the first comment.
- Use the project's names exactly: Datadog Agent, Core Agent, and ADP.
- Don't praise the change, and don't restate what the code does. Don't use emoji or jokes.
<!-- /review -->

### Severity

<!-- review:severity -->
Give each finding one of three levels:

- **Blocking**: wrong output, data loss, a crash or panic that production can reach, a deadlock or hang, a security
  weakness, a per-event cost that grows with load, such as a lock, a system call, a flush, or a channel send for each
  event, or a startup failure for a valid configuration. Don't add a prefix.
- **Not blocking**: a break of a rule in these guidelines that isn't a nit, a small allocation or clone for each event,
  missing tests for new production behavior, wrong or misleading docs and comments, or a real defect that needs an
  unlikely but possible sequence of events. Put "Not blocking:" at the start of the comment.
- **Nit**: an item in the "Nits" list, or a style problem that makes the diff hard to understand. Put "Nit:" at the
  start of the comment.

These findings are never blocking, so put "Not blocking:" at the start of each one:

- A question, and a finding that depends on a fact that you ask about.
- A defect that a sibling path already had before the pull request.
- A cost that the pull request description names, together with how it measures or fixes the cost.

Post nits after all other findings, and post at most three nits in one review. The grouped wording comment that
"Comments and documentation" describes doesn't count toward this limit.
<!-- /review -->

### What not to flag

<!-- review:do-not-flag -->
Reviewers dismissed these findings as noise in past reviews:

- Formatting and lint problems that `make fmt` and `make check-all` already catch.
- A missing release note. The release-note check already requires a note or the `changelog/no-changelog` label, and
  you can't see the labels.
- In test harness and tooling code, such as `bin/correctness/` and `test/`: production style rules, such as typed
  errors, extra clones, and wide visibility, and missing unit tests for code that a passing test case already runs or
  that a later pull request in the stack connects. Still flag logic bugs in that code.
- A claim that ADP differs from the Datadog Agent, the trace-agent, or another upstream project, when the repository
  doesn't show the upstream behavior. Ask one direct question instead.
- Edge cases that production can't reach, such as a race that needs an implausible timing window, or a hazard in an
  API that nothing calls yet.
- Stricter input validation, by itself. Rejecting input that ADP accepted before can be intentional. If the Datadog
  Agent may accept that input, ask one question.
- Problems in code that the pull request didn't change, and notes that ask for no action. There is one exception: a
  reference that the pull request's own rename or move leaves stale, such as an old name in a test case. Name its
  file and line in one sentence of the review body, because you can only comment on lines in the diff.
- Pinned links to upstream source in tests and in comments that show where a behavior comes from.
<!-- /review -->
