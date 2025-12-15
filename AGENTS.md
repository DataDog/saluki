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
