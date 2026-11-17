# How we review

This page describes how we review pull requests in Saluki: what we look for, in which order, and how we write review
comments. It applies to human and AI reviewers.

## What we look for

<!-- review:preamble -->
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

Security problems are treated the same as correctness problems.
<!-- /review -->

## How we write review comments

<!-- review:writing-findings -->
- **Short and informative.** Be direct and concise: "Don't clone here.", "Why are we cloning here?". If there is
  relevant detail to go along with the suggestion, name it specifically: "Otherwise, this will allocate for every packet
  received."
- **"We" for project rules.** We state a convention as a fact: "We avoid fully qualified paths here." Personal
  preference is "I would," and an optional idea is "take it or leave it."
- **Questions about intent and evidence.** "What does this add over the existing eviction?" "Is this how the Core Agent
  does it?" Use questions to gain clarity or seek evidence for changes which aren't self-explanatory. Don't assume if
  you're unsure.
- **Concrete fixes.** For simple fixes, like spelling errors or type swaps, use the form of "`old` -> `new`". For larger
  changes, use a suggestion block. When referencing existing code that should be followed, link directly to the relevant
  code rather than stating a vague location or reference point.

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
nit comes up often, try addressing it with a single comment that mentions the different areas where the nit occured.
Overall, the goal is not to be perfect and note every single thing a review finds, but to ensure that all blocking
issues are resolved, and that we fix _easy_ problems (non-blocking/nits) and try to leave the areas we touch a little
better than we found them,
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
