---
date: 2026-03-12
title: ADR 001 - Designing and planning in the open
---

# ADR 001 - Open design documentation

## Problem Statement

As Saluki grows, architectural decisions and design rationale are spread across pull requests, issues, and ad-hoc comments with no canonical, discoverable record. How should we capture and communicate design decisions so that current and future contributors can understand why the project is shaped the way it is?

## Context

Saluki is developed in the open. Source code, issues, and pull requests are all public — but the reasoning behind significant design choices is often buried in long PR threads or lost entirely. This makes it hard for new contributors to understand past trade-offs and for existing contributors to recall why a particular path was chosen.

In the spirit of open development, we want our design and planning process to be equally transparent. A structured, version-controlled home for this information keeps it discoverable, reviewable, and tied to the same repository as the code it describes.

## Considered Options

* **Continue with ad-hoc documentation in PRs and issues** — no new process; decisions stay where they were discussed.
* **Introduce an ADR section only** — add a dedicated section for recording point-in-time architectural decisions.
* **Introduce both an ADR section and an RFC/plans section** — add an ADR section for decisions and a separate section for longer-form design proposals and plans (RFCs).

## Decision Outcome

Chosen option: "Introduce both an ADR section and an RFC/plans section", because each format serves a distinct purpose and together they cover the full spectrum of design documentation we need.

ADRs are concise, point-in-time records of a decision and its rationale — ideal for capturing *what* was decided and *why*. RFCs/plans are longer-form documents suited for proposing and discussing designs *before* a decision is finalized. Having both gives contributors a clear place to propose ideas (RFCs) and a clear place to record the outcome (ADRs), all within the same public documentation site.

### Consequences

* Good, because design rationale becomes discoverable and version-controlled alongside the code.
* Good, because it reinforces Saluki's commitment to developing in the open — not just the code, but the thinking behind it.
* Good, because separating ADRs from RFCs gives each document type a focused scope, making them easier to write and review.
* Bad, because it introduces a small amount of process overhead when making significant decisions.
