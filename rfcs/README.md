# Saluki RFCs

This directory contains all Saluki RFCs (Request for Comment), which are documents intended to provide insight into the
design decisions and changes made to the Saluki project, and preserve them for future reference.

## Writing an RFC

Writing an RFC should not be an overly-complicated affair, because ultimately we want to focus on finding a solution to
the stated problem. However, we all need to make sure we're on the same page when it comes to what an RFC should look like.

Here are the high-level guidelines you should when writing an RFC:

1. RFCs should be written in English, as this is the lingua franca of the project.
    * Do not worry _too_ much about spelling and grammar, as we can easily fix spelling/grammar during the review
      period. The **content** is the primary concern.
2. RFCs should be placed in this directory and follow the naming scheme of `rfc<number>-<title>.md`.
	* The number should be the next available number in the sequence.
	* The title should be a very condensed version of the RFC's title. See existing RFCs for examples.
3. RFCs must be written in Markdown format.
4. RFCs should always have a problem statement, followed by some amount of discussion of the problem, historical
   context, trade-offs that must be considered, and so on, followed by the proposed solution, and any alternatives
   considered.
   * At a minimum, an RFC must have a problem statement and a proposed solution.
5. When you believe your RFC is ready, open a pull request to start the review process.

## Finalized RFCs

None yet.
