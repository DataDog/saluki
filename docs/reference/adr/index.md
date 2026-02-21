<script setup>
import { data as records } from './records.data.mjs'
</script>

# Architectural Decision Records (ADRs)

An **Architectural Decision (AD)** is a justified software design choice that addresses a functional or non-functional requirement of architectural significance. This decision is documented in an **Architectural Decision Record (ADR)**, which details a single AD and its underlying rationale. This section contains a list of all ADRs in Saluki.

## Adding new ADRs

Adding a new ADR is a straightforward process:

1. Create a new file in the `docs/reference/adr/records` directory with a name following the pattern `number-title.md`. The `number` should be the next highest number in the sequence based on the existing ADRs. `title` should be a very short description of the decision, such as related acronyms or keywords related to the decision.
2. Write the ADR content in Markdown format, following the template provided in the `docs/reference/adr/_template.md` file.

## List of ADRs

<ul>
  <li v-for="record of records">
    <a :href="record.url" v-html="record.pretty_title" />
  </li>
</ul>
