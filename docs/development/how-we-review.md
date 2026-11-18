# How we review

This page describes how we review pull requests in Saluki: what we look for, in which order, and how we write review
comments. It applies to human and AI reviewers.

## What we look for

<!-- review:preamble -->
You review pull requests for Saluki the way a maintainer does. Saluki is a Rust toolkit for building observability
data planes. Its main binary, `bin/agent-data-plane` (ADP), runs next to the Datadog Agent.

We rank problems by their effect on the people who run ADP. Each item has a section in the checklist below:

1. **Correctness and data loss.** Silent drops, lost data at shutdown, wrong configuration meaning, and deadlocks.
   Security problems rank here too.
2. **Datadog Agent parity** of what users observe.
3. **Cost on per-event paths, and bounded memory.**
4. **Fit with the architecture.** Reuse what exists, keep `lib/saluki-*` generic, and follow the component model.
5. **A small, honest API** with nothing that it doesn't need.
6. **Signals that operators can trust:** metrics, logs, and errors.
7. **Comments and docs that describe the present.**
8. **Tests that protect production behavior.**
9. **Scope.** Large refactors go to follow-up pull requests.

Write the review body as a short verdict of three sentences or fewer. Say "patch is correct" or "patch is incorrect,"
give the number of blocking findings, and give the main reason. Don't describe how you reviewed the change, and don't
list what you checked and found correct.
<!-- /review -->

## How we write review comments

<!-- review:writing-findings -->
- **Short and informative.** Be direct and concise, as in "Don't clone here." or "Why are we cloning here?" If there is
  relevant detail to go along with the suggestion, name it specifically: "Otherwise, this will allocate for every packet
  received."
- **"We" for project rules.** We state a convention as a fact: "We avoid fully qualified paths here." Personal
  preference is "I would," and an optional idea is "take it or leave it."
- **Questions about intent and evidence.** "What does this add over the existing eviction?" "Is this how the Core Agent
  does it?" Use questions to gain clarity or seek evidence for changes which aren't self-explanatory. Don't assume if
  you're unsure.
- **Concrete fixes.** For simple fixes, like spelling errors or type swaps, use the form "`old` -> `new`" for them. For
  larger changes, use a suggestion block. When referencing existing code that should be followed, link directly to the
  relevant code rather than stating a vague location or reference point.

It's not unreasonable that a small pull request can pass review and be approved without any review comments.
<!-- /review -->

## Severity

<!-- review:severity -->
There are three inherent levels of severity:

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

Post nits after all other findings, and try limiting the number of nits to somewhere around 3-5. If the same class of
nit comes up often, try addressing it with a single comment that mentions the different areas where the nit occurred.
Overall, the goal is not to be perfect and note every single thing a review finds, but to ensure that all blocking
issues are resolved, and that we fix _easy_ problems (non-blocking/nits), leaving the code a little better than we found
it.
<!-- /review -->

## What we don't flag

<!-- review:do-not-flag -->
- Formatting and lint problems that `make fmt` and `make check-all` already catch.
- A missing release note. The release note check already requires a note or the `changelog/no-changelog` label.
- In test harness and tooling code, such as `bin/correctness/` and `test/`: production style rules, such as typed
  errors, extra clones, and wide visibility, and missing unit tests for code that a passing test case already runs or
  that a later pull request in the stack connects. Still flag logic bugs in that code.
- A claim that ADP differs from the Datadog Agent, or another upstream project, when the repository doesn't show the
  upstream behavior. Ask the author directly instead.
- Edge cases that production can't reach, such as a race that needs an implausible timing window, or a hazard in an API
  that nothing calls yet.
- Stricter input validation, by itself. Rejecting input that ADP accepted before can be intentional. If the Datadog
  Agent may accept that input, ask the author directly.
- Problems in code that the pull request didn't change, and notes that ask for no action. There is one exception: a
  reference that the pull request's own rename or move leaves stale, such as an old name in a test case. Name its file
  and line in one sentence of the review body, because you can only comment on lines in the diff.
- Pinned links to upstream source in tests and in comments that show where a behavior comes from.
<!-- /review -->

## How we disagree

- We give way to evidence, especially Agent source.
- We hold firm on runtime correctness and per-event cost.
- When both sides have a point, we split the difference, and we record decisions made outside the pull request in the
  thread.
- We say so when we were wrong, and we fix our own mistakes in the framework rather than asking the author to work
  around them.

## How we treat AI review

An AI finding is input, like any other review comment. We agree with it, or we dismiss it with a reason: we don't apply
nits by reflex, and we don't resolve threads in silence. [Contributing](./contributing.md#responding-to-ai-review)
covers the author's side.

We also watch for the marks that coding agents leave: comments that narrate a change, banner comments, Markdown design
notes, and tests of test helpers. Eliminating these marks is not in service of hampering AI-assisted development, but
maintaining a consistent style and feel to the codebase that prioritizes humans as agents are already more than capable
of finding the necessary and relevant context for a change.
