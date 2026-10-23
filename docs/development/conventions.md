# Conventions

This page collects the project-specific rules that aren't mechanical enough for the
[style guide](./style-guide.md) or a lint: design preferences, scope decisions, and matters of taste. Reviewers call
these out in PR reviews when they're missed.

When a review or a design discussion settles a question that's likely to come up again, add it here.

## Code comments

Keep code comments, including rustdoc on private items, short. A comment should do one of two things:

- flag a caveat that a reader could easily miss
- annotate a large block of logic at a high level, so that it's easier to navigate

Don't write comments that restate what the code already says, and don't re-explain the design at every call site.
Explain a design once, in the module documentation, and keep comments at call sites to a sentence or two. Before
keeping a comment, ask whether deleting it would make the reader miss anything.

## Generated and vendored files

Never hand-edit files marked `@generated` or `DO NOT EDIT`: change their source of truth and regenerate them. The
vendored Datadog configuration schema under `lib/datadog-agent/config/schema/core/` is a pristine upstream copy; see
[Configuration system](./configuration-system.md) for how to change what it feeds into.

## Hot paths

Much of Saluki processes high volumes of metrics, logs, and traces. In code that runs per event, avoid heap
allocations, clones, and string formatting that aren't needed, unbounded buffers or queues, and logging at any level
whose volume scales with event volume.

## Supervision and shutdown

Every supervised child declares its own shutdown strategy, and whoever owns it, whether a supervisor or a scope,
enforces exactly that strategy:

- Don't special-case supervisors versus workers when it comes to deadlines.
- Don't hold back deadline margins, or pass an owner's deadline down to its children.
- If something outside has a shorter limit, it preempts by aborting the subtree. That's by design.

A component that overruns its grace period is a bug in that component. Fix it with rigorous code and tests rather than
hiding it with runtime deadline tricks.

## API routes and control plane workers

Publish internal API routes and gRPC services by asserting a dynamic route from a worker, rather than registering
static handlers on the API builder. Dynamic routes let any worker in the supervision tree contribute to an endpoint it
doesn't own, which keeps workers composable.

Shared crates provide workers, and each binary assembles its own supervisors from them. Don't wrap supervisor
construction in a library builder.

## Project scope

- The `standalone` mode of `agent-data-plane` is left over from earlier development and testing. It isn't for
  production use, and supporting it shouldn't block feature development.
- `panoramic` and `stele` under `bin/correctness/` are our own black-box correctness and integration tools. They
  don't replace unit tests for the branches inside a crate, and they aren't the Antithesis platform, which lives
  separately under `test/antithesis/`. See [Testing](./testing.md).
