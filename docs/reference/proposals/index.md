<script setup>
import { data as records } from './records.data.mjs'
</script>

# Proposals

A **proposal** is a longer-form design document, similar to an RFC, that describes a significant change or addition to Saluki before implementation begins. Proposals give contributors a structured way to discuss and refine ideas in the open, in keeping with Saluki's philosophy of developing transparently.

## Adding new proposals

Adding a new proposal is a straightforward process:

1. Create a new file in the `docs/reference/proposals/records` directory with a name following the pattern `number-title.md`. The `number` should be the next highest number in the sequence based on the existing proposals. `title` should be a very short description of the proposal, such as related acronyms or keywords.
2. Write the proposal content in Markdown format, following the template provided in the `docs/reference/proposals/_template.md` file.

## List of proposals

<ul v-if="records.length > 0">
  <li v-for="record of records">
    <a :href="record.url" v-html="record.pretty_title" />
  </li>
</ul>
<p v-else><b>No proposals defined yet!</b></p>
