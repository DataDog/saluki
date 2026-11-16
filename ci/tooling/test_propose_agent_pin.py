import base64
import contextlib
import importlib.util
import io
import json
import unittest
from pathlib import Path


spec = importlib.util.spec_from_file_location("propose_agent_pin", Path(__file__).with_name("propose_agent_pin.py"))
subject = importlib.util.module_from_spec(spec)
spec.loader.exec_module(subject)

REPOSITORY = "DataDog/saluki"
BRANCH = "ci/update-agent-nightly"
BASE_SHA = "a" * 40
BOT_HEAD = "b" * 40
NEW_PIN = json.dumps({"schema_version": 1, "integration": {"version": "new"}}, indent=2) + "\n"
OLD_PIN = json.dumps({"schema_version": 1, "integration": {"version": "old"}})


def commit(sha, author_type="Bot", verified=True):
    return {"sha": sha, "author": {"type": author_type}, "commit": {"verification": {"verified": verified}}}


class FakeApi:
    """A GitHub repository with one base branch and an optional bot branch.

    Ref updates follow GitHub's rules: creating an existing ref fails, and a non-force update must be a fast-forward.
    """

    def __init__(self, open_pr=False, files=(subject.PIN_PATH,), commits=None, branch_pin=OLD_PIN, branch_head=None,
                 ahead_by=1, signed=True, after_inspection=None):
        self.open_pr = open_pr
        self.files = [{"filename": name} for name in files]
        self.branch_head = branch_head if branch_head is not None else (BOT_HEAD if open_pr else None)
        self.commits = list(commits) if commits is not None else [commit(BOT_HEAD)]
        self.branch_pin = branch_pin
        self.ahead_by = ahead_by
        self.signed = signed
        self.after_inspection = after_inspection
        self.parents = {BOT_HEAD: BASE_SHA}
        self.created = 0
        self.writes = []

    def is_ancestor(self, ancestor, descendant):
        while descendant is not None:
            if descendant == ancestor:
                return True
            descendant = self.parents.get(descendant)
        return False

    def push(self, sha, parent):
        """Simulate someone else pushing `sha` onto the bot branch."""
        self.parents[sha] = parent
        self.branch_head = sha

    def request(self, method, path, body=None, allow_404=False):
        if method != "GET":
            self.writes.append((method, path, body))
            return self.write(method, path, body)
        if path == f"/repos/{REPOSITORY}/git/ref/heads/main":
            return {"object": {"sha": BASE_SHA}}
        if path.startswith(f"/repos/{REPOSITORY}/git/commits/"):
            return {"tree": {"sha": "9" * 40}}
        if path.startswith(f"/repos/{REPOSITORY}/pulls?"):
            pr = {"number": 5, "html_url": "https://example.test/pull/5", "head": {"sha": self.branch_head}}
            return [pr] if self.open_pr else []
        if path.endswith("/files?per_page=100"):
            return self.files
        if path.endswith("/commits?per_page=100"):
            commits = self.commits
            if self.after_inspection:
                self.after_inspection(self)
            return commits
        if path.startswith(f"/repos/{REPOSITORY}/contents/"):
            return {"content": base64.b64encode(self.branch_pin.encode()).decode()}
        if path == f"/repos/{REPOSITORY}/git/ref/heads/{BRANCH}":
            return {"object": {"sha": self.branch_head}} if self.branch_head else None
        if path.startswith(f"/repos/{REPOSITORY}/compare/"):
            return {"files": self.files, "commits": self.commits, "ahead_by": self.ahead_by}
        raise AssertionError(f"unexpected request {method} {path}")

    def write(self, method, path, body):
        if path.endswith("/git/trees"):
            return {"sha": "f" * 40}
        if path.endswith("/git/commits"):
            self.created += 1
            sha = f"{self.created:040x}"
            self.parents[sha] = body["parents"][0]
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
            return subject.propose(api, REPOSITORY, "main", BRANCH, NEW_PIN, "title", "body", ["automated"])

    def new_commit(self, api):
        tree = next(body for _, path, body in api.writes if path.endswith("/git/trees"))
        self.assertEqual(tree["tree"][0]["path"], subject.PIN_PATH)
        self.assertEqual(tree["tree"][0]["content"], NEW_PIN)
        return next(body for _, path, body in api.writes if path.endswith("/git/commits"))

    def test_opens_a_pull_request_from_the_base_branch(self):
        api = FakeApi()
        self.assertEqual(self.propose(api), "opened")
        self.assertEqual(self.new_commit(api)["parents"], [BASE_SHA])
        self.assertEqual(api.parents[api.branch_head], BASE_SHA)
        self.assertIn(("POST", f"/repos/{REPOSITORY}/issues/7/labels", {"labels": ["automated"]}), api.writes)

    def test_refreshes_a_bot_owned_pull_request_by_fast_forward(self):
        api = FakeApi(open_pr=True)
        self.assertEqual(self.propose(api), "refreshed")
        self.assertEqual(self.new_commit(api)["parents"], [BOT_HEAD])
        self.assertTrue(api.is_ancestor(BOT_HEAD, api.branch_head))
        self.assertFalse(any((body or {}).get("force") for _, _, body in api.writes))

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

    def test_leaves_an_unchanged_proposal_alone(self):
        api = FakeApi(open_pr=True, branch_pin=NEW_PIN)
        self.assertEqual(self.propose(api), "unchanged")
        self.assertEqual(api.writes, [])

    def test_holds_a_pull_request_that_carries_an_adp_change(self):
        api = FakeApi(open_pr=True, files=(subject.PIN_PATH, "lib/saluki-components/src/lib.rs"))
        self.assertEqual(self.propose(api), "held")
        self.assertEqual(api.writes, [])

    def test_never_overwrites_human_commits(self):
        for commits in ((commit("c" * 40), commit(BOT_HEAD, author_type="User")), (commit(BOT_HEAD, verified=False),)):
            with self.subTest(commits=commits):
                api = FakeApi(open_pr=True, commits=commits)
                self.assertEqual(self.propose(api), "held")
                self.assertEqual(api.writes, [])

    def test_does_not_reuse_a_leftover_branch_with_human_commits(self):
        api = FakeApi(branch_head=BOT_HEAD, commits=[commit(BOT_HEAD, author_type="User")])
        self.assertEqual(self.propose(api), "held")
        self.assertEqual(api.writes, [])

    def test_extends_a_leftover_bot_branch(self):
        api = FakeApi(branch_head=BOT_HEAD)
        self.assertEqual(self.propose(api), "opened")
        self.assertEqual(self.new_commit(api)["parents"], [BOT_HEAD])

    def test_restarts_a_leftover_branch_that_is_already_in_the_base(self):
        api = FakeApi(branch_head=BOT_HEAD, commits=[], files=(), ahead_by=0)
        api.parents = {BASE_SHA: BOT_HEAD}
        self.assertEqual(self.propose(api), "opened")
        self.assertEqual(self.new_commit(api)["parents"], [BASE_SHA])

    def test_refuses_to_push_an_unsigned_commit(self):
        api = FakeApi(signed=False)
        with self.assertRaisesRegex(subject.ProposeError, "did not sign"):
            self.propose(api)
        self.assertIsNone(api.branch_head)


if __name__ == "__main__":
    unittest.main()
