#!/usr/bin/env python3
"""Open or refresh the bot pull request that proposes a discovered `.datadog-agent-version.json`.

Each base branch has one bot branch and at most one open pull request. The tool refreshes that pull request only
while every change on it is the bot's own Agent version update. Once anyone adds a commit or touches another file, the
pull request keeps its candidate and blocks newer proposals until it merges or closes.

Every commit the tool writes has the base branch's current tree plus the new version file, so the branch's CI tests
the new Agent against current code. While a pull request is open, its branch only moves forward: the new commit's
parents include the branch's current head, plus the base branch's head when the bot branch is behind it, and the ref
update is a fast-forward, never a force push. If someone pushes after the ownership check, the update is no longer a
fast-forward, GitHub rejects it, and the run fails without touching their commit. With no open pull request, the
branch belongs to the automation alone. A leftover branch, such as one from a closed pull request, restarts from the
base branch. Its commits stay reachable from the closed pull request. The reset applies only if the branch still points
at the commit the tool read, so a push after that, for example to a reopened pull request, makes the run fail instead.

If the version file on the base branch changed after discovery read it, the candidate may be stale, so the tool
proposes nothing and the next run discovers again.

Commits are created through GitHub's API without custom author information, so GitHub signs them as the app that
owns the token. The token comes from `GITHUB_TOKEN`.
"""

import argparse
import base64
import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

VERSION_FILE = ".datadog-agent-version.json"
PAGE_SIZE = 100


class ProposeError(Exception):
    """GitHub rejected a request, or returned something unexpected."""


class GitHubApi:
    def __init__(self, token):
        self.token = token

    def request(self, method, path, body=None, allow_404=False):
        url = path if path.startswith("https://") else f"https://api.github.com{path}"
        data = json.dumps(body).encode() if body is not None else None
        request = urllib.request.Request(url, method=method, data=data)
        request.add_header("Accept", "application/vnd.github+json")
        request.add_header("Authorization", f"Bearer {self.token}")
        request.add_header("X-GitHub-Api-Version", "2022-11-28")
        if body is not None:
            request.add_header("Content-Type", "application/json")
        try:
            with urllib.request.urlopen(request, timeout=60) as response:
                payload = response.read()
        except urllib.error.HTTPError as error:
            if allow_404 and error.code == 404:
                return None
            raise ProposeError(f"{method} {path} returned HTTP {error.code}: {error.read()[:500]!r}") from error
        return json.loads(payload) if payload else None


def list_all(api, path):
    """Return every item of a paginated GitHub list endpoint."""
    items = []
    page = 1
    while True:
        batch = api.request("GET", f"{path}?per_page={PAGE_SIZE}&page={page}")
        items.extend(batch)
        if len(batch) < PAGE_SIZE:
            return items
        page += 1


def is_bot_commit(commit):
    """Return whether a commit was authored by a bot and signed by GitHub, as the API-created commits are."""
    author = commit.get("author") or {}
    verified = (commit.get("commit") or {}).get("verification", {}).get("verified")
    return author.get("type") == "Bot" and verified is True


def foreign_changes(files, commits):
    """Return why changes on the bot branch are not the bot's own Agent version update, or `None`."""
    other_files = sorted(entry["filename"] for entry in files if entry.get("filename") != VERSION_FILE)
    if other_files:
        return f"it changes files other than {VERSION_FILE}: {', '.join(other_files)}"
    foreign = [commit["sha"][:12] for commit in commits if not is_bot_commit(commit)]
    if foreign:
        return f"it has commits the automation did not make: {', '.join(foreign)}"
    return None


def commit_parents(comparison, base_sha, head_sha):
    """Return the parents for a new commit on a bot branch at `head_sha`, given GitHub's comparison of base...head.

    A branch that is already part of the base starts again from the base. Otherwise the commit keeps `head_sha`, so the
    ref update is a fast-forward, and adds `base_sha` when the branch is behind, so the base's changes come in.
    """
    if comparison.get("ahead_by", 0) == 0:
        return [base_sha]
    if comparison.get("behind_by", 0) == 0:
        return [head_sha]
    return [head_sha, base_sha]


def reset_ref(api, repository, branch, expected_sha, sha):
    """Move `branch` to `sha`, even if that isn't a fast-forward, only if it still points at `expected_sha`.

    The REST API has no such precondition, so this uses GraphQL's atomic `updateRefs`.
    """
    repository_id = api.request("GET", f"/repos/{repository}")["node_id"]
    query = (
        "mutation($repository: ID!, $updates: [RefUpdate!]!) "
        + "{ updateRefs(input: {repositoryId: $repository, refUpdates: $updates}) { clientMutationId } }"
    )
    update = {"name": f"refs/heads/{branch}", "beforeOid": expected_sha, "afterOid": sha, "force": True}
    response = api.request(
        "POST", "/graphql", {"query": query, "variables": {"repository": repository_id, "updates": [update]}}
    )
    if response.get("errors"):
        raise ProposeError(f"could not reset {branch} from {expected_sha[:12]}: {response['errors']}")


def write_commit(api, repository, branch, base_sha, parents, version_text, message, exists, expected_head=None):
    """Commit the base's tree plus the version file, with `parents`, and move `branch` to that commit.

    Without `expected_head`, an existing branch moves only by fast-forward, so the update fails if the branch no longer
    points at a parent. With it, the branch is reset only if it still points at `expected_head`.
    """
    base_tree = api.request("GET", f"/repos/{repository}/git/commits/{base_sha}")["tree"]["sha"]
    entry = {"path": VERSION_FILE, "mode": "100644", "type": "blob", "content": version_text}
    tree = api.request("POST", f"/repos/{repository}/git/trees", {"base_tree": base_tree, "tree": [entry]})["sha"]
    commit = api.request(
        "POST", f"/repos/{repository}/git/commits", {"message": message, "tree": tree, "parents": parents}
    )
    if not commit.get("verification", {}).get("verified"):
        raise ProposeError(f"GitHub did not sign commit {commit.get('sha')}; not pushing it")
    if expected_head is not None:
        reset_ref(api, repository, branch, expected_head, commit["sha"])
    elif exists:
        api.request("PATCH", f"/repos/{repository}/git/refs/heads/{branch}", {"sha": commit["sha"], "force": False})
    else:
        api.request("POST", f"/repos/{repository}/git/refs", {"ref": f"refs/heads/{branch}", "sha": commit["sha"]})
    return commit["sha"]


def read_version_file_at(api, repository, ref):
    document = api.request("GET", f"/repos/{repository}/contents/{VERSION_FILE}?ref={urllib.parse.quote(ref)}",
                           allow_404=True)
    if document is None:
        return None
    return json.loads(base64.b64decode(document["content"]))


def propose(api, repository, base, branch, version_text, title, body, labels, merged):
    """Open, refresh or hold the bot pull request.

    `merged` is the version file discovery compared the candidate with, or `None` when there was none. Returns
    `opened`, `refreshed`, `unchanged`, `held` or `stale`.
    """
    owner = repository.split("/", 1)[0]
    base_sha = api.request("GET", f"/repos/{repository}/git/ref/heads/{base}")["object"]["sha"]
    if read_version_file_at(api, repository, base_sha) != merged:
        print(f"[*] {VERSION_FILE} on {base} changed after discovery. Proposing nothing; the next run discovers again.")
        return "stale"
    query = urllib.parse.urlencode({"state": "open", "base": base, "head": f"{owner}:{branch}"})
    open_prs = api.request("GET", f"/repos/{repository}/pulls?{query}")

    if open_prs:
        pr = open_prs[0]
        number = pr["number"]
        head_sha = pr["head"]["sha"]
        files = list_all(api, f"/repos/{repository}/pulls/{number}/files")
        commits = list_all(api, f"/repos/{repository}/pulls/{number}/commits")
        if not commits or commits[-1]["sha"] != head_sha:
            raise ProposeError(f"{pr['html_url']} changed while it was being inspected; try again")
        problem = foreign_changes(files, commits)
        if problem:
            print(f"[*] Holding {pr['html_url']}: {problem}. Newer candidates wait until it merges or closes.")
            return "held"
        if read_version_file_at(api, repository, head_sha) == json.loads(version_text):
            print(f"[*] {pr['html_url']} already proposes this Agent version.")
            return "unchanged"
        comparison = api.request("GET", f"/repos/{repository}/compare/{base_sha}...{head_sha}")
        parents = commit_parents(comparison, base_sha, head_sha)
        write_commit(api, repository, branch, base_sha, parents, version_text, title, exists=True)
        api.request("PATCH", f"/repos/{repository}/pulls/{number}", {"title": title, "body": body})
        print(f"[*] Refreshed {pr['html_url']}")
        return "refreshed"

    # With no open pull request, a leftover branch restarts from the base. A closed pull request keeps its commits.
    existing = api.request("GET", f"/repos/{repository}/git/ref/heads/{branch}", allow_404=True)
    expected_head = existing["object"]["sha"] if existing is not None else None
    if expected_head is not None:
        print(f"[*] Restarting leftover branch {branch} from {base}.")
    write_commit(
        api, repository, branch, base_sha, [base_sha], version_text, title, exists=existing is not None,
        expected_head=expected_head,
    )
    pr = api.request("POST", f"/repos/{repository}/pulls", {"title": title, "head": branch, "base": base, "body": body})
    api.request("POST", f"/repos/{repository}/issues/{pr['number']}/labels", {"labels": labels})
    print(f"[*] Opened {pr['html_url']}")
    return "opened"


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--repository", required=True, help="owner/name")
    parser.add_argument("--base", required=True, help="Branch the pull request targets")
    parser.add_argument("--branch", required=True, help="Bot branch that carries the proposal")
    parser.add_argument(
        "--candidate-dir", type=Path, required=True, help="Output directory of discover_agent_version.py"
    )
    parser.add_argument("--label", action="append", default=[], dest="labels")
    arguments = parser.parse_args(argv)

    token = os.environ.get("GITHUB_TOKEN")
    if not token:
        print("error: GITHUB_TOKEN is not set", file=sys.stderr)
        return 1
    candidate = arguments.candidate_dir
    merged_path = candidate / "merged-datadog-agent-version.json"
    try:
        propose(
            GitHubApi(token),
            arguments.repository,
            arguments.base,
            arguments.branch,
            (candidate / "datadog-agent-version.json").read_text(encoding="utf-8"),
            (candidate / "pr-title.txt").read_text(encoding="utf-8").strip(),
            (candidate / "pr-body.md").read_text(encoding="utf-8"),
            arguments.labels,
            json.loads(merged_path.read_text(encoding="utf-8")) if merged_path.exists() else None,
        )
    except (ProposeError, OSError, KeyError, json.JSONDecodeError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
