# Common understanding

Saluki is a large software project with a growing number of contributors. In order to ensure everyone is on the same
page, we need to make sure we're all using the same terminology and have a common understanding on the various areas of
the project and the tools and processes for developing it.

## Writing style

We strive to use ASD-STE100, or "Simplified Technical English," when writing user-facing
code comments and documentation. ASD-STE100 is a large, highly opinionated reference for
technical writing, but can generally be summarized as having the following high-level
aspects:

- prefer using "active voice" ("The dog bit the postal carrier." instead of "The postal carrier was bitten by the dog.")
- use short sentences (no more than 20-25 words per sentence) and short paragraphs (no more than 5-6 sentences per paragraph)
- avoid synonyms where possible, preferring simple nouns and verbs used consistently throughout the codebase

ASD-STE100 consists of two main parts -- the writing rules and the dictionary -- which are designed to be used together. We don't expect users to precisely follow the dictionary portion of ASD-STE100, but users are expected to write in a straightforward manner -- following the above rules, preferring consistent, simple wording -- whether by hand or through agentic help.

## Requirement levels

If you have ever read an IETF RFC, you may be familiar with the sight of words like _MUST_ and _SHOULD NOT_.
[RFC 2119](https://datatracker.ietf.org/doc/html/rfc2119) defines these as _requirement levels_, where the use of
these terms indicates whether or not something is required, or is a suggestion, or if the behavior is purely optional.

These requirement levels are used throughout the documentation to help clarify the expectations around certain
processes. You're encouraged to read the RFC, but here's a quick rundown:

- **MUST**/**SHALL**: this is an absolute requirement
- **MUST NOT**/**SHALL NOT**: this is an absolute prohibition
- **SHOULD**/**RECOMMENDED**: you really should do this, but there may be valid reasons not to
- **SHOULD NOT**/**NOT RECOMMENDED**: you really shouldn't do this, but there may be valid reasons to do so
- **MAY**/**OPTIONAL**: this is legitimately optional ("dealer's choice")

Like RFC 2119 mentions, these should be used sparingly and only when necessary to clarify expectations... so if you see
them, pay attention.
