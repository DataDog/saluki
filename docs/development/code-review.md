# Code review

This page covers the guidelines we use when reviewing changes, whether the reviewer is a human or an AI. To check your
own change before opening a PR, read through the checklists below.

## How AI review works

A collaborator with write access can comment `/dd-review` on a PR to request an AI code review. The review uses the
`codereview_guideline.md` files in the repository as its instructions:

- The file at the repository root applies to every PR.
- A file in a subdirectory applies only to PRs that change files under that subdirectory.
- The guidelines are read from `main`, not from the PR, so changes to them take effect after they merge.

The review action removes `AGENTS.md` and `CLAUDE.md` files before the review runs, so each guideline file **MUST**
stay self-contained, rather than pointing to other pages. This makes the guideline files one of the few exceptions to
[Where documentation lives](./style-guide.md#where-documentation-lives). To add review guidance for one area of the
codebase, add a `codereview_guideline.md` file in that area's directory and include it on this page.

The sections below render the guideline files as they're written.

## General guidelines

From `codereview_guideline.md`:

<!--@include: ../../codereview_guideline.md-->

## Scoped guidelines

From `lib/datadog-agent/config/codereview_guideline.md`, which applies to PRs that change the configuration system:

<!--@include: ../../lib/datadog-agent/config/codereview_guideline.md-->

From `lib/saluki-core/codereview_guideline.md`, which applies to PRs that change the core crate and data model:

<!--@include: ../../lib/saluki-core/codereview_guideline.md-->
