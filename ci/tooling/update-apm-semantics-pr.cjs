'use strict';

// Pull request decisions for the `Update APM semantic mappings` workflow. The workflow loads this file from
// `actions/github-script`, and the tests load it with fake clients, so both run the same code.

const BASE_BRANCH = 'main';

// The first label marks every pull request this workflow opens and is how older ones are found and superseded. It is
// removed from a superseded pull request, which tells that closure apart from a reviewer declining the change.
const MARKER_LABEL = 'ci/update-apm-semantics';
const LABELS = [MARKER_LABEL, 'automated'];

const hasMarker = (pullRequest) => pullRequest.labels.some((label) => label.name === MARKER_LABEL);

/**
 * Decides whether the change on `branch` still needs a pull request.
 *
 * Each distinct change is reported once, and an unmerged pull request is what constitutes having reported it: an open
 * one is waiting for a reviewer, and a closed one that still has the marker label was declined. A merged or superseded
 * one does not count, because branch names are derived from the file's contents and upstream can return to earlier
 * contents.
 *
 * Returns `'reported'` when there is nothing to do and `'publish'` when the caller should create the branch and the
 * pull request.
 */
async function findReport({ github, context, core, branch }) {
  const { owner, repo } = context.repo;

  const { data: pullRequests } = await github.rest.pulls.list({
    owner,
    repo,
    head: `${owner}:${branch}`,
    state: 'all',
  });

  const reportedBy = pullRequests.find(
    (pullRequest) => !pullRequest.merged_at && (pullRequest.state === 'open' || hasMarker(pullRequest)),
  );
  if (reportedBy) {
    core.info(`Already reported by ${reportedBy.html_url} (${reportedBy.state}); nothing to do.`);

    // A run can die between creating the pull request and finishing with older ones. Repeat that work, which changes
    // nothing when it is already done.
    if (reportedBy.state === 'open') {
      await github.rest.issues.addLabels({ owner, repo, issue_number: reportedBy.number, labels: LABELS });
      await closeSuperseded({ github, context, core, current: reportedBy });
    }
    return 'reported';
  }

  // Any branch that remains is an orphan from a run that failed before creating its pull request, or the leftover of
  // a merged or superseded one. Creating it again would fail, so clear it for this run.
  try {
    await github.rest.git.deleteRef({ owner, repo, ref: `heads/${branch}` });
    core.info(`Deleted the leftover branch ${branch}.`);
  } catch (error) {
    if (error.status !== 404 && error.status !== 422) {
      throw error;
    }
  }
  return 'publish';
}

/**
 * Creates and labels the pull request for `branch`, then closes the older ones this workflow opened.
 *
 * The new pull request exists and is labelled before anything is closed, so a failure part way through never leaves
 * the change without an open pull request. `commitBody` is the refresh commit's message body, `previous` is the
 * upstream commit the old snapshot came from, and `source` the one the new snapshot comes from.
 */
async function openPullRequest({ github, context, core, branch, title, commitBody, previous, source }) {
  const { owner, repo } = context.repo;

  const { data: pullRequest } = await github.rest.pulls.create({
    owner,
    repo,
    title,
    head: branch,
    base: BASE_BRANCH,
    body: pullRequestBody({ commitBody, previous, source }),
  });
  core.info(`Pull request created: ${pullRequest.html_url}`);

  await github.rest.issues.addLabels({ owner, repo, issue_number: pullRequest.number, labels: LABELS });
  await closeSuperseded({ github, context, core, current: pullRequest });
  return pullRequest;
}

/** Closes every open pull request with the marker label other than `current`. */
async function closeSuperseded({ github, context, core, current }) {
  const { owner, repo } = context.repo;

  // Listing and filtering is used instead of searching, because the search index lags behind new labels.
  const openPullRequests = await github.paginate(github.rest.pulls.list, {
    owner,
    repo,
    state: 'open',
    base: BASE_BRANCH,
  });
  const superseded = openPullRequests.filter((other) => other.number !== current.number && hasMarker(other));

  for (const other of superseded) {
    core.info(`Closing superseded pull request: ${other.html_url}`);
    // The label goes first: a failure before closing leaves an open pull request for a human to close, rather than a
    // closed one that looks declined.
    await github.rest.issues.removeLabel({ owner, repo, issue_number: other.number, name: MARKER_LABEL });
    await github.rest.pulls.update({ owner, repo, pull_number: other.number, state: 'closed' });
    await github.rest.issues.createComment({
      owner,
      repo,
      issue_number: other.number,
      body:
        `Closing this pull request in favor of a newer mappings update: ${current.html_url}. ` +
        `If a fix was pushed to this branch, cherry-pick it onto the new one.`,
    });
  }
}

function pullRequestBody({ commitBody, previous, source }) {
  return `## Summary

This updates the vendored APM semantic mappings to a newer version of the Datadog Agent's
\`pkg/trace/semantics/mappings.json\`.

${commitBody}

Changes since the previous snapshot: [\`${previous.slice(0, 12)}...${source.slice(0, 12)}\`](https://github.com/DataDog/datadog-agent/compare/${previous}...${source})

**This changes the fallback used when no registry has come from Remote Configuration.** Review the changed
concepts as a behavior change.

**Failing tests on this PR are an expected outcome, not a broken build.** The embedded registry is parsed strictly,
so a concept that ADP does not model fails \`from_slice_parses_the_embedded_mappings\`, and a concept that the update
removes fails \`every_concept_variant_is_registered\`. To resolve it, push a commit to this branch that:

- adds the \`Concept\` variant to the enum, to \`Concept::ALL\` and to \`as_str\` in
  \`lib/datadog-agent/apm/src/semantics/mod.rs\`, and updates the count in \`concept_all_covers_every_variant\`, or
- for a removed concept, removes its variant.

Do not edit \`mappings.json\` by hand: the next refresh overwrites it. If a changed fallback alters behavior that users
can see, add a release note to this branch, and file a follow-up issue for any behavior that a new concept drives.

_This PR was automatically generated by the [Update APM Semantic Mappings](https://github.com/DataDog/saluki/blob/main/.github/workflows/update-apm-semantics.yml) workflow._

## Change Type

- [ ] Bug fix
- [ ] New feature
- [x] Non-functional (chore, refactoring, docs)
- [ ] Performance`;
}

module.exports = { findReport, openPullRequest };
