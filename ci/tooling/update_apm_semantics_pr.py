#!/usr/bin/env python3
"""Pull request decisions for the `Update APM Semantic Mappings` workflow.

The workflow runs this file's subcommands, and the tests drive the same functions with a fake GitHub client.
It uses only the standard library.

Environment variables:
- GITHUB_REPOSITORY: the `owner/repo` that receives the pull request.
- GITHUB_TOKEN: the token that writes to GitHub.
"""

import argparse
import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

BASE_BRANCH = "main"

# The first label marks every pull request this workflow opens and is how older ones are found and superseded.
MARKER_LABEL = "ci/update-apm-semantics"
LABELS = [MARKER_LABEL, "automated"]

# Tells a superseded pull request's closure apart from a reviewer declining the change. GitHub does not promise to
# create a label when adding it, so the first run that needs this one creates it.
SUPERSEDED_LABEL = "ci/update-apm-semantics-superseded"
SUPERSEDED_COLOR = "c5def5"
SUPERSEDED_DESCRIPTION = "CI workflow: Update APM Semantic Mappings closed this as superseded."

API_ROOT = "https://api.github.com"
PAGE_SIZE = 100


class GitHubError(Exception):
    """A failed GitHub API call, with the HTTP status."""

    def __init__(self, status, message):
        super().__init__(message)
        self.status = status


class GitHub:
    """The few GitHub REST calls this workflow makes, for one repository."""

    def __init__(self, repository, token):
        self.owner, _, self.repo = repository.partition("/")
        if not self.owner or not self.repo:
            raise ValueError(f"GITHUB_REPOSITORY '{repository}' is not 'owner/repo'.")
        self._token = token

    def list_pulls(self, state, head=None, base=None):
        """Lists every pull request in `state` ('open' or 'all'), optionally limited to a `head` or `base`."""
        params = {"state": state, "per_page": PAGE_SIZE}
        if head is not None:
            params["head"] = head
        if base is not None:
            params["base"] = base

        pulls = []
        for page in range(1, 1000):
            batch = self._request("GET", "pulls", params={**params, "page": page})
            pulls.extend(batch)
            if len(batch) < PAGE_SIZE:
                return pulls
        raise GitHubError(None, "Pull request listing did not end.")

    def create_pull(self, title, head, body):
        """Opens a pull request from `head` into the base branch."""
        return self._request("POST", "pulls", {"title": title, "head": head, "base": BASE_BRANCH, "body": body})

    def close_pull(self, number):
        """Closes a pull request."""
        self._request("PATCH", f"pulls/{number}", {"state": "closed"})

    def add_labels(self, number, labels):
        """Adds labels to a pull request, keeping the ones it has."""
        self._request("POST", f"issues/{number}/labels", {"labels": labels})

    def get_label(self, name):
        """Gets a repository label."""
        return self._request("GET", f"labels/{urllib.parse.quote(name, safe='')}")

    def create_label(self, name, color, description):
        """Creates a repository label."""
        self._request("POST", "labels", {"name": name, "color": color, "description": description})

    def remove_label(self, number, label):
        """Removes a label from a pull request."""
        self._request("DELETE", f"issues/{number}/labels/{urllib.parse.quote(label, safe='')}")

    def create_comment(self, number, body):
        """Comments on a pull request."""
        self._request("POST", f"issues/{number}/comments", {"body": body})

    def delete_branch(self, branch):
        """Deletes a branch."""
        self._request("DELETE", f"git/refs/heads/{urllib.parse.quote(branch, safe='/')}")

    def _request(self, method, path, body=None, params=None):
        url = f"{API_ROOT}/repos/{self.owner}/{self.repo}/{path}"
        if params:
            url += "?" + urllib.parse.urlencode(params)
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(url, data=data, method=method)
        request.add_header("Authorization", f"Bearer {self._token}")
        request.add_header("Accept", "application/vnd.github+json")
        request.add_header("X-GitHub-Api-Version", "2022-11-28")
        if data is not None:
            request.add_header("Content-Type", "application/json")
        try:
            with urllib.request.urlopen(request, timeout=60) as response:
                payload = response.read()
        except urllib.error.HTTPError as error:
            raise GitHubError(error.code, f"{method} {path} failed with HTTP {error.code}.") from error
        return json.loads(payload) if payload else None


def log(message):
    print(message, file=sys.stderr)


def has_label(pull, name):
    return any(label["name"] == name for label in pull["labels"])


def is_declined(pull):
    return pull["state"] == "closed" and has_label(pull, MARKER_LABEL) and not has_label(pull, SUPERSEDED_LABEL)


def find_report(github, branch):
    """Decides whether the change on `branch` still needs a pull request.

    Each distinct change is reported once, and an unmerged pull request is what constitutes having reported it: an open
    one is waiting for a reviewer, and a closed one with the marker label but not the superseded label was declined. A
    merged or superseded one does not count, because branch names are derived from the file's contents and upstream can
    return to earlier contents.

    Returns 'reported' when there is nothing to do and 'publish' when the caller should create the branch and the pull
    request.
    """
    pulls = github.list_pulls(state="all", head=f"{github.owner}:{branch}")

    reported_by = next(
        (pull for pull in pulls if not pull["merged_at"] and (pull["state"] == "open" or is_declined(pull))), None
    )
    if reported_by:
        log(f"Already reported by {reported_by['html_url']} ({reported_by['state']}); nothing to do.")

        # A run can die between creating the pull request and finishing with older ones. Repeat that work, which
        # changes nothing when it is already done. Older ones are obsolete even when this one was declined.
        if reported_by["state"] == "open":
            github.add_labels(reported_by["number"], LABELS)
            # Upstream returned to this change before a failed run finished closing it, so it is current again.
            if has_label(reported_by, SUPERSEDED_LABEL):
                github.remove_label(reported_by["number"], SUPERSEDED_LABEL)
        close_superseded(github, reported_by)
        return "reported"

    # Any branch that remains is an orphan from a run that failed before creating its pull request, or the leftover of
    # a merged or superseded one. Creating it again would fail, so clear it for this run.
    try:
        github.delete_branch(branch)
        log(f"Deleted the leftover branch {branch}.")
    except GitHubError as error:
        if error.status not in (404, 422):
            raise
    return "publish"


def open_pull_request(github, branch, title, commit_body, previous, source):
    """Creates and labels the pull request for `branch`, then closes the older ones this workflow opened.

    The new pull request exists and is labelled before anything is closed, so a failure part way through never leaves
    the change without an open pull request. `commit_body` is the refresh commit's message body, `previous` is the
    upstream commit the old snapshot came from, and `source` the one the new snapshot comes from.
    """
    pull = github.create_pull(title, branch, pull_request_body(commit_body, previous, source))
    log(f"Pull request created: {pull['html_url']}")

    github.add_labels(pull["number"], LABELS)
    close_superseded(github, pull)
    return pull


def close_superseded(github, current=None):
    """Closes every open pull request with the marker label other than `current`.

    Without `current`, the caller found the embedded mappings already match upstream, so every open pull request
    proposes mappings that upstream no longer has.
    """
    # Listing and filtering is used instead of searching, because the search index lags behind new labels.
    open_pulls = github.list_pulls(state="open", base=BASE_BRANCH)
    keep = None if current is None else current["number"]
    superseded = [other for other in open_pulls if other["number"] != keep and has_label(other, MARKER_LABEL)]

    if current is None:
        reason = (
            "Closing this pull request because the embedded mappings match upstream again. "
            "If upstream returns to these mappings, a new pull request proposes them."
        )
    else:
        reason = (
            f"Closing this pull request in favor of a newer mappings update: {current['html_url']}. "
            "If a fix was pushed to this branch, cherry-pick it onto the new one."
        )

    if superseded:
        try:
            github.get_label(SUPERSEDED_LABEL)
        except GitHubError as error:
            if error.status != 404:
                raise
            log(f"Creating the {SUPERSEDED_LABEL} label.")
            github.create_label(SUPERSEDED_LABEL, SUPERSEDED_COLOR, SUPERSEDED_DESCRIPTION)

    for other in superseded:
        log(f"Closing superseded pull request: {other['html_url']}")
        # The label goes first, so a failure part way never leaves a closed pull request that looks declined. One left
        # open keeps the marker label, and a later run closes it.
        github.add_labels(other["number"], [SUPERSEDED_LABEL])
        github.close_pull(other["number"])
        github.create_comment(other["number"], reason)


def pull_request_body(commit_body, previous, source):
    return f"""## Summary

This updates the vendored APM semantic mappings to a newer version of the Datadog Agent's
`pkg/trace/semantics/mappings.json`.

{commit_body}

Changes since the previous snapshot: [`{previous[:12]}...{source[:12]}`](https://github.com/DataDog/datadog-agent/compare/{previous}...{source})

**This changes the fallback used when no registry has come from Remote Configuration.** Review the changed
concepts as a behavior change.

An ordinary update, such as new concepts or new fallbacks that use a known provider and value type, passes CI without
a code change. The embedded registry ignores concepts that ADP does not model yet, so add a `Concept` variant in
`lib/datadog-agent/apm/src/semantics/mod.rs` as a follow-up if ADP should use one.

CI fails here only when ADP can no longer use the snapshot as it stands. Push the fix to this branch if:

- a fallback for a known concept has a provider or value type that ADP does not support, so the registry cannot load it,
  or
- an update removes a concept that ADP models, so `every_concept_variant_is_registered` fails.

Do not edit `mappings.json` by hand: the next refresh overwrites it. If a changed fallback alters behavior that users
can see, add a release note to this branch.

_This PR was automatically generated by the [Update APM Semantic Mappings](https://github.com/DataDog/saluki/blob/main/.github/workflows/update-apm-semantics.yml) workflow._

## Change Type

- [ ] Bug fix
- [ ] New feature
- [x] Non-functional (chore, refactoring, docs)
- [ ] Performance"""


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)

    find = commands.add_parser("find-report", help="Print 'publish' or 'reported' for a branch.")
    find.add_argument("--branch", required=True)

    open_ = commands.add_parser("open-pr", help="Open the pull request for a published branch.")
    open_.add_argument("--branch", required=True)
    open_.add_argument("--title", required=True)
    open_.add_argument("--commit-body-file", type=Path, required=True)
    open_.add_argument("--previous", required=True)
    open_.add_argument("--source", required=True)

    commands.add_parser("close-superseded", help="Close every open update pull request, after a run with no change.")

    args = parser.parse_args(argv)
    github = GitHub(os.environ["GITHUB_REPOSITORY"], os.environ["GITHUB_TOKEN"])

    if args.command == "find-report":
        print(find_report(github, args.branch))
    elif args.command == "close-superseded":
        close_superseded(github)
    else:
        open_pull_request(
            github,
            args.branch,
            args.title,
            args.commit_body_file.read_text(encoding="utf-8").strip(),
            args.previous,
            args.source,
        )


if __name__ == "__main__":
    main()
