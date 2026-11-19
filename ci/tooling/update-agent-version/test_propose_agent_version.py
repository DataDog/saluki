import base64
import contextlib
import importlib.util
import io
import json
import unittest
from pathlib import Path


spec = importlib.util.spec_from_file_location(
    "propose_agent_version", Path(__file__).with_name("propose_agent_version.py")
)
subject = importlib.util.module_from_spec(spec)
spec.loader.exec_module(subject)

REPOSITORY = "DataDog/saluki"
BRANCH = "ci/update-agent-nightly"
BASE_SHA = "a" * 40
BOT_HEAD = "b" * 40
OLD_BASE_SHA = "c" * 40
NEW_VERSION_FILE = json.dumps({"schema_version": 1, "integration": {"version": "new"}}, indent=2) + "\n"
OLD_VERSION_FILE = json.dumps({"schema_version": 1, "integration": {"version": "old"}})


def commit(sha, author_type="Bot", verified=True):
    return {"sha": sha, "author": {"type": author_type}, "commit": {"verification": {"verified": verified}}}


class FakeApi:
    """A GitHub repository with one base branch and an optional bot branch.

    Ref updates follow GitHub's rules: creating an existing ref fails, and a non-force update must be a fast-forward.
    """

    def __init__(self, open_pr=False, files=(subject.VERSION_FILE,), commits=None, branch_version_file=OLD_VERSION_FILE,
                 branch_head=None, ahead_by=1, behind_by=0, signed=True, after_inspection=None,
                 base_version_file=OLD_VERSION_FILE, before_reset=None, after_branch_read=None):
        self.open_pr = open_pr
        self.files = [{"filename": name} for name in files]
        self.branch_head = branch_head if branch_head is not None else (BOT_HEAD if open_pr else None)
        self.commits = list(commits) if commits is not None else [commit(BOT_HEAD)]
        self.branch_version_file = branch_version_file
        self.base_version_file = base_version_file
        self.before_reset = before_reset
        self.after_branch_read = after_branch_read
        self.ahead_by = ahead_by
        self.behind_by = behind_by
        self.signed = signed
        self.after_inspection = after_inspection
        # A bot branch that is behind the base forked from an older base commit.
        self.parents = {BOT_HEAD: [OLD_BASE_SHA if behind_by else BASE_SHA], BASE_SHA: [OLD_BASE_SHA]}
        self.created = 0
        self.writes = []
        self.pages_read = 0

    def is_ancestor(self, ancestor, descendant):
        pending = [descendant]
        while pending:
            sha = pending.pop()
            if sha == ancestor:
                return True
            pending.extend(self.parents.get(sha, []))
        return False

    def push(self, sha, parent):
        """Simulate someone else pushing `sha` onto the bot branch."""
        self.parents[sha] = [parent]
        self.branch_head = sha

    def page(self, path, items):
        query = dict(part.split("=") for part in path.split("?", 1)[1].split("&"))
        size, number = int(query["per_page"]), int(query["page"])
        self.pages_read += 1
        return items[(number - 1) * size:number * size]

    def request(self, method, path, body=None, allow_404=False):
        if method != "GET":
            self.writes.append((method, path, body))
            return self.write(method, path, body)
        if path == f"/repos/{REPOSITORY}":
            return {"node_id": "R_saluki"}
        if path == f"/repos/{REPOSITORY}/git/ref/heads/main":
            return {"object": {"sha": BASE_SHA}}
        if path.startswith(f"/repos/{REPOSITORY}/git/commits/"):
            return {"tree": {"sha": "tree-" + path.rsplit("/", 1)[1]}}
        if path.startswith(f"/repos/{REPOSITORY}/pulls?"):
            pr = {"number": 5, "html_url": "https://example.test/pull/5", "head": {"sha": self.branch_head}}
            return [pr] if self.open_pr else []
        if "/files?" in path:
            return self.page(path, self.files)
        if "/commits?" in path:
            commits = self.page(path, self.commits)
            if self.after_inspection and len(commits) < subject.PAGE_SIZE:
                self.after_inspection(self)
            return commits
        if path.startswith(f"/repos/{REPOSITORY}/contents/"):
            text = self.base_version_file if path.endswith(f"ref={BASE_SHA}") else self.branch_version_file
            return {"content": base64.b64encode(text.encode()).decode()}
        if path == f"/repos/{REPOSITORY}/git/ref/heads/{BRANCH}":
            head = self.branch_head
            if self.after_branch_read:
                self.after_branch_read(self)
            return {"object": {"sha": head}} if head else None
        if path.startswith(f"/repos/{REPOSITORY}/compare/"):
            return {
                "files": self.files, "commits": self.commits, "ahead_by": self.ahead_by, "behind_by": self.behind_by
            }
        raise AssertionError(f"unexpected request {method} {path}")

    def write(self, method, path, body):
        if path.endswith("/git/trees"):
            return {"sha": "f" * 40}
        if path == "/graphql":
            if self.before_reset:
                self.before_reset(self)
            (update,) = body["variables"]["updates"]
            assert body["variables"]["repository"] == "R_saluki" and update["name"] == f"refs/heads/{BRANCH}"
            if update["beforeOid"] != self.branch_head:
                return {"data": None, "errors": [{"message": "Expected branch to point to " + update["beforeOid"]}]}
            self.branch_head = update["afterOid"]
            return {"data": {"updateRefs": {"clientMutationId": None}}}
        if path.endswith("/git/commits"):
            self.created += 1
            sha = f"{self.created:040x}"
            self.parents[sha] = body["parents"]
            return {"sha": sha, "verification": {"verified": self.signed}}
        if path == f"/repos/{REPOSITORY}/git/refs":
            if self.branch_head is not None:
                raise subject.ProposeError("HTTP 422: Reference already exists")
            self.branch_head = body["sha"]
            return {}
        if path == f"/repos/{REPOSITORY}/git/refs/heads/{BRANCH}":
            if body.get("force") or not self.is_ancestor(self.branch_head, body["sha"]):
                raise subject.ProposeError("HTTP 422: Update is not a fast forward")
            self.branch_head = body["sha"]
            return {}
        if path.endswith("/pulls"):
            return {"number": 7, "html_url": "https://example.test/pull/7"}
        return {}


class ProposeTest(unittest.TestCase):
    def propose(self, api):
        with contextlib.redirect_stdout(io.StringIO()):
            return subject.propose(
                api, REPOSITORY, "main", BRANCH, NEW_VERSION_FILE, "title", "body", ["automated"],
                json.loads(OLD_VERSION_FILE),
            )

    def new_commit(self, api):
        tree = next(body for _, path, body in api.writes if path.endswith("/git/trees"))
        self.assertEqual(tree["base_tree"], "tree-" + BASE_SHA)
        self.assertEqual(tree["tree"][0]["path"], subject.VERSION_FILE)
        self.assertEqual(tree["tree"][0]["content"], NEW_VERSION_FILE)
        return next(body for _, path, body in api.writes if path.endswith("/git/commits"))

    def test_opens_a_pull_request_from_the_base_branch(self):
        api = FakeApi()
        self.assertEqual(self.propose(api), "opened")
        self.assertEqual(self.new_commit(api)["parents"], [BASE_SHA])
        self.assertEqual(api.parents[api.branch_head], [BASE_SHA])
        self.assertIn(("POST", f"/repos/{REPOSITORY}/issues/7/labels", {"labels": ["automated"]}), api.writes)

    def test_refreshes_a_bot_owned_pull_request_by_fast_forward(self):
        api = FakeApi(open_pr=True)
        self.assertEqual(self.propose(api), "refreshed")
        self.assertEqual(self.new_commit(api)["parents"], [BOT_HEAD])
        self.assertTrue(api.is_ancestor(BOT_HEAD, api.branch_head))
        self.assertFalse(any((body or {}).get("force") for _, _, body in api.writes))

    def test_a_refresh_brings_in_a_base_that_moved(self):
        api = FakeApi(open_pr=True, behind_by=1)
        self.assertEqual(self.propose(api), "refreshed")
        self.assertEqual(self.new_commit(api)["parents"], [BOT_HEAD, BASE_SHA])
        self.assertTrue(api.is_ancestor(BOT_HEAD, api.branch_head))
        self.assertTrue(api.is_ancestor(BASE_SHA, api.branch_head))

    def test_a_push_after_the_ownership_check_is_not_overwritten(self):
        human = "e" * 40
        api = FakeApi(open_pr=True, after_inspection=lambda api: api.push(human, BOT_HEAD))
        with self.assertRaisesRegex(subject.ProposeError, "not a fast forward"):
            self.propose(api)
        self.assertEqual(api.branch_head, human)

    def test_a_pull_request_that_moves_during_inspection_is_not_updated(self):
        api = FakeApi(open_pr=True, commits=[commit(BOT_HEAD), commit("e" * 40)])
        with self.assertRaisesRegex(subject.ProposeError, "changed while it was being inspected"):
            self.propose(api)
        self.assertEqual(api.writes, [])

    def test_reads_every_page_of_a_long_pull_request(self):
        commits = [commit(f"d{n:039x}") for n in range(1, 150)] + [commit(BOT_HEAD)]
        api = FakeApi(open_pr=True, commits=commits)
        self.assertEqual(self.propose(api), "refreshed")
        self.assertEqual(api.pages_read, 3)

    def test_leaves_an_unchanged_proposal_alone(self):
        api = FakeApi(open_pr=True, branch_version_file=NEW_VERSION_FILE)
        self.assertEqual(self.propose(api), "unchanged")
        self.assertEqual(api.writes, [])

    def test_keeps_a_pull_request_that_proposes_a_later_nightly(self):
        def nightly(pipeline_id):
            integration = {"kind": "nightly", "version": f"pipeline.{pipeline_id}", "pipeline_id": pipeline_id}
            return json.dumps({"schema_version": 1, "integration": integration})

        api = FakeApi(open_pr=True, branch_version_file=nightly(99200))
        with contextlib.redirect_stdout(io.StringIO()):
            outcome = subject.propose(
                api, REPOSITORY, "main", BRANCH, nightly(99000), "title", "body", ["automated"],
                json.loads(OLD_VERSION_FILE),
            )
        self.assertEqual(outcome, "later")
        self.assertEqual(api.writes, [])

        api = FakeApi(open_pr=True, branch_version_file=nightly(98000))
        with contextlib.redirect_stdout(io.StringIO()):
            outcome = subject.propose(
                api, REPOSITORY, "main", BRANCH, nightly(99000), "title", "body", ["automated"],
                json.loads(OLD_VERSION_FILE),
            )
        self.assertEqual(outcome, "refreshed")

    def test_holds_a_pull_request_that_carries_an_adp_change(self):
        api = FakeApi(open_pr=True, files=(subject.VERSION_FILE, "lib/saluki-components/src/lib.rs"))
        self.assertEqual(self.propose(api), "held")
        self.assertEqual(api.writes, [])

    def test_never_overwrites_human_commits(self):
        for commits in ((commit("c" * 40), commit(BOT_HEAD, author_type="User")), (commit(BOT_HEAD, verified=False),)):
            with self.subTest(commits=commits):
                api = FakeApi(open_pr=True, commits=commits)
                self.assertEqual(self.propose(api), "held")
                self.assertEqual(api.writes, [])

    def test_restarts_a_leftover_branch_from_the_base(self):
        # A closed pull request leaves its branch, possibly with human commits, or ahead of and behind the base.
        cases = {
            "human commits": dict(commits=[commit(BOT_HEAD, author_type="User")]),
            "bot commits": dict(),
            "behind the base": dict(behind_by=3),
            "already in the base": dict(commits=[], files=(), ahead_by=0),
        }
        for name, options in cases.items():
            with self.subTest(name):
                api = FakeApi(branch_head=BOT_HEAD, **options)
                self.assertEqual(self.propose(api), "opened")
                self.assertEqual(self.new_commit(api)["parents"], [BASE_SHA])
                (reset,) = [body for _, path, body in api.writes if path == "/graphql"]
                self.assertEqual(reset["variables"]["updates"][0]["beforeOid"], BOT_HEAD)
                self.assertEqual(api.parents[api.branch_head], [BASE_SHA])

    def test_a_push_before_a_leftover_branch_resets_is_not_overwritten(self):
        # Someone reopens the closed pull request and pushes after the tool found no open pull request.
        human = "e" * 40
        api = FakeApi(branch_head=BOT_HEAD, before_reset=lambda api: api.push(human, BOT_HEAD))
        with self.assertRaisesRegex(subject.ProposeError, "could not reset"):
            self.propose(api)
        self.assertEqual(api.branch_head, human)
        self.assertFalse(any(path.endswith("/pulls") for _, path, _ in api.writes))

    def test_a_pull_request_reopened_with_a_push_after_the_branch_read_is_not_overwritten(self):
        human = "e" * 40

        def reopen_and_push(api):
            api.open_pr = True
            api.push(human, BOT_HEAD)
            api.commits = [commit(BOT_HEAD), commit(human, author_type="User")]

        api = FakeApi(branch_head=BOT_HEAD, after_branch_read=reopen_and_push)
        self.assertEqual(self.propose(api), "held")
        self.assertEqual(api.branch_head, human)
        self.assertEqual(api.writes, [])

    def test_proposes_nothing_when_the_base_version_changed_after_discovery(self):
        for open_pr in (False, True):
            with self.subTest(open_pr=open_pr):
                api = FakeApi(open_pr=open_pr, base_version_file=NEW_VERSION_FILE)
                self.assertEqual(self.propose(api), "stale")
                self.assertEqual(api.writes, [])

    def test_refuses_to_push_an_unsigned_commit(self):
        api = FakeApi(signed=False)
        with self.assertRaisesRegex(subject.ProposeError, "did not sign"):
            self.propose(api)
        self.assertIsNone(api.branch_head)


if __name__ == "__main__":
    unittest.main()
