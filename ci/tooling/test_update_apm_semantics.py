"""Tests for `update-apm-semantics.sh` and `update_apm_semantics_pr.py`.

The script runs for real inside a throwaway git repository, with a fake `curl` standing in for the network. The pull
request module runs against a fake GitHub client.
"""

import contextlib
import importlib.util
import io
import json
import os
import shutil
import subprocess
import tempfile
import unittest
import unittest.mock
import urllib.error
from pathlib import Path

TOOLING = Path(__file__).parent
SCRIPT = TOOLING / "update-apm-semantics.sh"
WORKFLOW = TOOLING.parent.parent / ".github/workflows/update-apm-semantics.yml"

spec = importlib.util.spec_from_file_location("update_apm_semantics_pr", TOOLING / "update_apm_semantics_pr.py")
subject = importlib.util.module_from_spec(spec)
spec.loader.exec_module(subject)

SEMANTICS_DIR = "lib/datadog-agent/apm/src/semantics"
MAPPINGS = f"{SEMANTICS_DIR}/mappings.json"
VERSION_FILE = f"{SEMANTICS_DIR}/_version.txt"

OLD_COMMIT = "a" * 40
NEW_COMMIT = "b1c2d3e4f5a6".ljust(40, "0")

# Stands in for `curl -fsSL --retry 3 [-H header] -o <file> <url>`. It logs the URL and the header sent, if any, then
# copies the response named by the URL. A missing response is an HTTP failure.
FAKE_CURL = """#!/bin/sh
out=""; auth=none
while [ "$#" -gt 1 ]; do
  case "$1" in
    -o) out="$2"; shift 2 ;;
    -H) auth="$2"; shift 2 ;;
    *) shift ;;
  esac
done
echo "auth=$auth $1" >> "$FAKE_CURL_DIR/curl.log"
case "$1" in
  https://api.github.com/*) response="$FAKE_CURL_DIR/api.json" ;;
  https://raw.githubusercontent.com/*) response="$FAKE_CURL_DIR/raw.json" ;;
  *) exit 1 ;;
esac
[ -f "$response" ] || exit 22
cp "$response" "$out"
"""


def mappings(concepts, content_hash="sha256:abc"):
    return json.dumps({"metadata": {"content_hash": content_hash}, "concepts": concepts}, indent=2) + "\n"


OLD_MAPPINGS = mappings(
    {
        "kept.same": {"fallbacks": [1]},
        "kept.changed": {"fallbacks": [1]},
        "dropped.concept": {"fallbacks": [1]},
    }
)

NEW_MAPPINGS = mappings(
    {
        "kept.same": {"fallbacks": [1]},
        "kept.changed": {"fallbacks": [1, 2]},
        "added.concept": {"fallbacks": [1]},
    }
)

# Keeps the developer's own git configuration (commit signing, hooks, templates) out of the fixture repositories.
ISOLATED_GIT_ENV = {"GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1"}


def git(directory, *args):
    env = {**os.environ, **ISOLATED_GIT_ENV}
    result = subprocess.run(["git", "-C", str(directory), *args], capture_output=True, text=True, env=env)
    if result.returncode != 0:
        raise AssertionError(f"git {' '.join(args)} failed: {result.stderr}")
    return result.stdout


def api_response(sha=NEW_COMMIT):
    return json.dumps([{"sha": sha}])


class Fixture:
    """A git repository holding the real script, a vendored snapshot and a seed commit, plus a fake `curl`.

    `api` and `raw` are the contents of the two responses, and a missing one makes `curl` fail.
    """

    def __init__(self, test, api=None, raw=None, seed_version=True, old_mappings=OLD_MAPPINGS):
        self.root = Path(tempfile.mkdtemp(prefix="update-apm-semantics-"))
        test.addCleanup(shutil.rmtree, self.root, ignore_errors=True)

        self.repo = self.root / "repo"
        bin_dir = self.root / "bin"
        self.curl_dir = self.root / "curl"
        for directory in (self.repo / SEMANTICS_DIR, self.repo / "ci/tooling", bin_dir, self.curl_dir):
            directory.mkdir(parents=True)
        self.bin_dir = bin_dir

        script = self.repo / "ci/tooling/update-apm-semantics.sh"
        shutil.copy(SCRIPT, script)
        script.chmod(0o755)
        (self.repo / MAPPINGS).write_text(old_mappings)
        if seed_version:
            (self.repo / VERSION_FILE).write_text(f"{OLD_COMMIT}\n")
        (self.repo / "tracked.txt").write_text("original\n")

        (bin_dir / "curl").write_text(FAKE_CURL)
        (bin_dir / "curl").chmod(0o755)
        if api is not None:
            (self.curl_dir / "api.json").write_text(api)
        if raw is not None:
            (self.curl_dir / "raw.json").write_text(raw)

        git(self.repo, "init", "--quiet", "-b", "main")
        git(self.repo, "config", "user.name", "Test")
        git(self.repo, "config", "user.email", "test@example.com")
        git(self.repo, "add", "--all")
        git(self.repo, "commit", "--quiet", "-m", "seed")

    def run(self, args=(), extra_env=None):
        env = {
            "PATH": f"{self.bin_dir}:{os.environ['PATH']}",
            "HOME": str(self.root),
            **ISOLATED_GIT_ENV,
            "FAKE_CURL_DIR": str(self.curl_dir),
            **(extra_env or {}),
        }
        return subprocess.run(
            [str(self.repo / "ci/tooling/update-apm-semantics.sh"), *args],
            cwd=self.repo,
            capture_output=True,
            text=True,
            env=env,
        )

    def curl_log(self):
        log = self.curl_dir / "curl.log"
        return [line for line in log.read_text().split("\n") if line] if log.exists() else []

    def head(self):
        return git(self.repo, "rev-parse", "HEAD").strip()

    def read(self, path):
        return (self.repo / path).read_text()


class UpdateScriptTest(unittest.TestCase):
    def assert_untouched(self, fx, seed_head):
        """Asserts that a run changed nothing: no commit, a clean tree and the original contents."""
        self.assertEqual(fx.head(), seed_head)
        self.assertEqual(git(fx.repo, "status", "--porcelain"), "")
        self.assertEqual(fx.read(MAPPINGS), OLD_MAPPINGS)

    def test_does_nothing_when_the_bytes_are_the_same_even_if_the_upstream_commit_differs(self):
        fx = Fixture(self, api=api_response(), raw=OLD_MAPPINGS)
        seed_head = fx.head()

        result = fx.run()

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_untouched(fx, seed_head)
        self.assertEqual(fx.read(VERSION_FILE), f"{OLD_COMMIT}\n")

    def test_commits_only_the_two_paths_and_leaves_unrelated_changes_where_they_were(self):
        fx = Fixture(self, api=api_response(), raw=NEW_MAPPINGS)
        seed_head = fx.head()
        (fx.repo / "staged.txt").write_text("staged\n")
        git(fx.repo, "add", "staged.txt")
        (fx.repo / "tracked.txt").write_text("edited\n")

        result = fx.run()

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(git(fx.repo, "rev-list", "--count", f"{seed_head}..HEAD").strip(), "1")
        committed = sorted(git(fx.repo, "show", "--name-only", "--format=", "HEAD").split())
        self.assertEqual(committed, sorted([VERSION_FILE, MAPPINGS]))
        self.assertEqual(fx.read(MAPPINGS), NEW_MAPPINGS)
        self.assertEqual(fx.read(VERSION_FILE), f"{NEW_COMMIT}\n")

        message = git(fx.repo, "log", "-1", "--format=%B")
        self.assertRegex(message, r"^chore\(apm\): refresh semantic mappings from datadog-agent b1c2d3e4f5a6\n")
        self.assertIn("Added concepts:\n- added.concept\n", message)
        self.assertIn("Removed concepts:\n- dropped.concept\n", message)
        self.assertIn("Changed concepts:\n- kept.changed\n", message)
        self.assertIn(f"/commit/{NEW_COMMIT}", message)

        self.assertEqual(git(fx.repo, "diff", "--cached", "--name-only"), "staged.txt\n")
        self.assertEqual(git(fx.repo, "diff", "--name-only"), "tracked.txt\n")
        self.assertEqual(fx.read("tracked.txt"), "edited\n")

    def test_says_none_for_a_list_with_nothing_in_it(self):
        added = {**json.loads(OLD_MAPPINGS)["concepts"], "added.concept": {"fallbacks": [1]}}
        fx = Fixture(self, api=api_response(), raw=mappings(added))

        result = fx.run()

        self.assertEqual(result.returncode, 0, result.stderr)
        message = git(fx.repo, "log", "-1", "--format=%B")
        self.assertIn("Removed concepts: none\n", message)
        self.assertIn("Changed concepts: none\n", message)

    def test_passes_ref_to_the_commit_query(self):
        fx = Fixture(self, api=api_response(), raw=OLD_MAPPINGS)

        self.assertEqual(fx.run(["feature/x-1.2"]).returncode, 0)

        self.assertRegex(fx.curl_log()[0], r"pkg/trace/semantics/mappings\.json&sha=feature/x-1\.2&per_page=1$")

    def test_defaults_ref_to_main_including_when_it_is_empty(self):
        for args in ([], [""]):
            with self.subTest(args=args):
                fx = Fixture(self, api=api_response(), raw=OLD_MAPPINGS)

                self.assertEqual(fx.run(args).returncode, 0)

                self.assertIn("&sha=main&", fx.curl_log()[0])

    def test_rejects_unsafe_refs_before_any_network_call(self):
        for ref in ("a&b", "-x", "with space", "semi;colon", "new\nline"):
            with self.subTest(ref=ref):
                fx = Fixture(self, api=api_response(), raw=NEW_MAPPINGS)
                seed_head = fx.head()

                result = fx.run([ref])

                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(fx.curl_log(), [])
                self.assert_untouched(fx, seed_head)

    def test_fails_and_leaves_no_trace_on_bad_upstream_responses(self):
        bad_upstream = {
            "invalid JSON": {"api": api_response(), "raw": "not json"},
            "empty concepts": {"api": api_response(), "raw": mappings({})},
            "missing content_hash": {
                "api": api_response(),
                "raw": json.dumps({"metadata": {}, "concepts": {"a": {}}}),
            },
            "an empty commit list": {"api": "[]", "raw": NEW_MAPPINGS},
            "a SHA that is not hex": {"api": api_response("Z" * 40), "raw": NEW_MAPPINGS},
            "a SHA that is too short": {"api": api_response("abc123"), "raw": NEW_MAPPINGS},
            "a failing commit query": {"raw": NEW_MAPPINGS},
            "a failing download": {"api": api_response()},
        }
        for name, responses in bad_upstream.items():
            with self.subTest(name):
                fx = Fixture(self, **responses)
                seed_head = fx.head()

                result = fx.run()

                self.assertNotEqual(result.returncode, 0)
                self.assert_untouched(fx, seed_head)
                self.assertEqual(sorted(p.name for p in (fx.repo / SEMANTICS_DIR).iterdir()), ["_version.txt", "mappings.json"])

    def test_fails_rather_than_describe_the_change_wrongly_when_the_vendored_copy_is_not_json(self):
        fx = Fixture(self, api=api_response(), raw=NEW_MAPPINGS, old_mappings="not json\n")
        seed_head = fx.head()

        result = fx.run()

        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(fx.head(), seed_head)
        self.assertEqual(git(fx.repo, "status", "--porcelain"), "")

    def test_refuses_to_run_with_uncommitted_changes_to_either_target(self):
        def unstaged(fx):
            with open(fx.repo / MAPPINGS, "a") as handle:
                handle.write(" ")

        def staged(fx):
            unstaged(fx)
            git(fx.repo, "add", MAPPINGS)

        def untracked_version(fx):
            (fx.repo / VERSION_FILE).write_text("x\n")

        dirty_targets = {
            "an unstaged change to mappings.json": (unstaged, True),
            "a staged change to mappings.json": (staged, True),
            "an untracked _version.txt": (untracked_version, False),
        }
        for name, (dirty, seed_version) in dirty_targets.items():
            with self.subTest(name):
                fx = Fixture(self, api=api_response(), raw=NEW_MAPPINGS, seed_version=seed_version)
                seed_head = fx.head()
                dirty(fx)
                before = git(fx.repo, "status", "--porcelain")

                result = fx.run()

                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(fx.curl_log(), [])
                self.assertEqual(fx.head(), seed_head)
                self.assertEqual(git(fx.repo, "status", "--porcelain"), before)

    def test_sends_github_token_to_the_api_and_nowhere_else(self):
        fx = Fixture(self, api=api_response(), raw=NEW_MAPPINGS)

        result = fx.run(extra_env={"GITHUB_TOKEN": "secret"})

        self.assertEqual(result.returncode, 0, result.stderr)
        api, raw = fx.curl_log()
        self.assertRegex(api, r"^auth=Authorization: Bearer secret https://api\.github\.com/")
        self.assertRegex(raw, r"^auth=none https://raw\.githubusercontent\.com/")
        self.assertNotIn("secret", result.stdout + result.stderr)

    def test_sends_no_header_without_github_token(self):
        fx = Fixture(self, api=api_response(), raw=OLD_MAPPINGS)

        self.assertEqual(fx.run().returncode, 0)

        self.assertTrue(all(line.startswith("auth=none ") for line in fx.curl_log()))


BRANCH = "ci/update-apm-semantics-0123456789ab"
MARKER = {"name": "ci/update-apm-semantics"}
SUPERSEDED = {"name": "ci/update-apm-semantics-superseded"}
LABELS = ["ci/update-apm-semantics", "automated"]


def pull_request(number, **fields):
    """An open, unlabelled pull request from `BRANCH` into `main`, unless `fields` says otherwise."""
    return {
        "number": number,
        "html_url": f"https://example.test/pull/{number}",
        "head": {"label": f"DataDog:{BRANCH}"},
        "base": {"ref": "main"},
        "state": "open",
        "merged_at": None,
        "labels": [],
        **fields,
    }


def older_pull_request(number, **fields):
    defaults = {"head": {"label": f"DataDog:ci/update-apm-semantics-old{number}"}, "labels": [MARKER]}
    return pull_request(number, **{**defaults, **fields})


def http_error(status):
    def raise_error(**_):
        raise subject.GitHubError(status, f"HTTP {status}")

    return raise_error


def fail_once(status, number=None, effect=None):
    """A response that fails the first call, or the first for pull request `number`, with HTTP `status`.

    `effect` runs first when given, as when a request takes effect but its response is lost.
    """
    pending = [True]

    def respond(**args):
        if pending and number in (None, args.get("number")):
            pending.clear()
            if effect:
                effect()
            raise subject.GitHubError(status, f"HTTP {status}")

    return respond


class FakeGitHub:
    """Records every call as `(name, args)`, in order.

    `responses` maps a call name to the value it returns, or to a function that returns it or raises. `list_pulls`
    filters `pulls` by `head`, `state` and `base` as GitHub does. Closing and labelling calls that succeed update
    `pulls`, so a later call sees their result.

    `labels` names the repository's labels, which start as the ones that exist in DataDog/saluki. Adding a label that is
    missing fails, because GitHub does not document that it creates one.
    """

    owner = "DataDog"
    repo = "saluki"

    def __init__(self, responses=None, pulls=(), labels=tuple(LABELS)):
        self.calls = []
        self.responses = responses or {}
        self.pulls = list(pulls)
        self.labels = set(labels)

    def names(self):
        return [name for name, _ in self.calls]

    def _respond(self, name, args, fallback=None):
        self.calls.append((name, args))
        response = self.responses.get(name, fallback)
        return response(**args) if callable(response) else response

    def list_pulls(self, state, head=None, base=None):
        def matching(**args):
            return [
                pull
                for pull in self.pulls
                if (head is None or pull["head"]["label"] == head)
                and (state == "all" or pull["state"] == state)
                and (base is None or pull["base"]["ref"] == base)
            ]

        return self._respond("list_pulls", {"state": state, "head": head, "base": base}, matching)

    def create_pull(self, title, head, body):
        created = {"number": 99, "html_url": "https://example.test/pull/99"}
        return self._respond("create_pull", {"title": title, "head": head, "body": body}, created)

    def _update(self, number, change):
        for pull in self.pulls:
            if pull["number"] == number:
                change(pull)

    def close_pull(self, number):
        self._respond("close_pull", {"number": number})
        self._update(number, lambda pull: pull.update(state="closed"))

    def get_label(self, name):
        self._respond("get_label", {"name": name})
        if name not in self.labels:
            raise subject.GitHubError(404, "HTTP 404")
        return {"name": name}

    def create_label(self, name, color, description):
        self._respond("create_label", {"name": name, "color": color, "description": description})
        if name in self.labels:
            raise subject.GitHubError(422, "HTTP 422")
        self.labels.add(name)

    def add_labels(self, number, labels):
        self._respond("add_labels", {"number": number, "labels": labels})
        if not self.labels.issuperset(labels):
            raise subject.GitHubError(422, "HTTP 422")

        def add(pull):
            missing = [label for label in labels if not any(have["name"] == label for have in pull["labels"])]
            pull["labels"] = pull["labels"] + [{"name": label} for label in missing]

        self._update(number, add)

    def remove_label(self, number, label):
        self._respond("remove_label", {"number": number, "label": label})

        def remove(pull):
            pull["labels"] = [have for have in pull["labels"] if have["name"] != label]

        self._update(number, remove)

    def create_comment(self, number, body):
        self._respond("create_comment", {"number": number, "body": body})

    def delete_branch(self, branch):
        self._respond("delete_branch", {"branch": branch})


class QuietTestCase(unittest.TestCase):
    def setUp(self):
        patcher = unittest.mock.patch.object(subject, "log")
        patcher.start()
        self.addCleanup(patcher.stop)


class FindReportTest(QuietTestCase):
    def test_deletes_the_branch_when_there_is_no_pull_request_tolerating_a_missing_branch(self):
        github = FakeGitHub(responses={"delete_branch": http_error(404)})

        self.assertEqual(subject.find_report(github, BRANCH), "publish")

        self.assertEqual(github.names(), ["list_pulls", "delete_branch"])
        self.assertEqual(github.calls[0][1], {"state": "all", "head": f"DataDog:{BRANCH}", "base": None})
        self.assertEqual(github.calls[1][1], {"branch": BRANCH})

    def test_repairs_the_labels_and_closes_older_pull_requests_when_a_pull_request_is_open(self):
        github = FakeGitHub(
            pulls=[pull_request(7), older_pull_request(5), older_pull_request(6, labels=[])],
            labels=[*LABELS, SUPERSEDED["name"]],
        )

        self.assertEqual(subject.find_report(github, BRANCH), "reported")

        self.assertEqual(
            github.names(),
            ["list_pulls", "add_labels", "list_pulls", "get_label", "add_labels", "close_pull", "create_comment"],
        )
        self.assertEqual(github.calls[1][1], {"number": 7, "labels": LABELS})
        self.assertEqual(github.calls[4][1], {"number": 5, "labels": [SUPERSEDED["name"]]})
        self.assertEqual(github.calls[5][1], {"number": 5})
        self.assertIn("https://example.test/pull/7", github.calls[6][1]["body"])

    def test_leaves_the_pull_request_alone_when_a_reviewer_closed_it(self):
        declined = pull_request(7, state="closed", labels=[MARKER])
        github = FakeGitHub(pulls=[declined])

        self.assertEqual(subject.find_report(github, BRANCH), "reported")

        self.assertEqual(github.names(), ["list_pulls", "list_pulls"])
        self.assertEqual((declined["state"], declined["labels"]), ("closed", [MARKER]))

    def test_publishes_again_when_the_only_pull_request_was_merged_or_superseded(self):
        cases = {
            "merged": {"state": "closed", "merged_at": "2026-01-01T00:00:00Z", "labels": [MARKER]},
            "superseded": {"state": "closed", "labels": [MARKER, SUPERSEDED]},
            "closed without the marker label": {"state": "closed"},
        }
        for name, fields in cases.items():
            with self.subTest(name):
                github = FakeGitHub(pulls=[pull_request(7, **fields)])

                self.assertEqual(subject.find_report(github, BRANCH), "publish")

                self.assertEqual(github.names(), ["list_pulls", "delete_branch"])

    def test_tolerates_an_unprocessable_branch_deletion(self):
        github = FakeGitHub(responses={"delete_branch": http_error(422)})

        self.assertEqual(subject.find_report(github, BRANCH), "publish")

    def test_rethrows_any_other_branch_deletion_failure(self):
        github = FakeGitHub(responses={"delete_branch": http_error(500)})

        with self.assertRaises(subject.GitHubError) as raised:
            subject.find_report(github, BRANCH)

        self.assertEqual(raised.exception.status, 500)


class OpenPullRequestTest(QuietTestCase):
    INPUTS = {
        "branch": BRANCH,
        "title": "chore(apm): refresh semantic mappings from datadog-agent b1c2d3e4f5a6",
        "commit_body": "Added concepts:\n- added.concept",
        "previous": OLD_COMMIT,
        "source": NEW_COMMIT,
    }

    def test_creates_and_labels_the_pull_request_then_closes_older_labelled_ones_only(self):
        github = FakeGitHub(
            pulls=[
                pull_request(99, labels=[MARKER]),
                older_pull_request(5),
                older_pull_request(6, labels=[{"name": "automated"}, {"name": "unrelated"}]),
                older_pull_request(8, labels=[]),
                older_pull_request(9, state="closed"),
                older_pull_request(10, base={"ref": "release"}),
            ],
            labels=[*LABELS, SUPERSEDED["name"]],
        )

        created = subject.open_pull_request(github, **self.INPUTS)

        self.assertEqual(created["number"], 99)
        self.assertEqual(
            github.names(),
            ["create_pull", "add_labels", "list_pulls", "get_label", "add_labels", "close_pull", "create_comment"],
        )
        create, label, _, _, supersede, close, comment = (args for _, args in github.calls)
        self.assertEqual(create["title"], self.INPUTS["title"])
        self.assertEqual(create["head"], BRANCH)
        self.assertIn(self.INPUTS["commit_body"], create["body"])
        self.assertIn(f"compare/{OLD_COMMIT}...{NEW_COMMIT}", create["body"])
        self.assertEqual(label, {"number": 99, "labels": LABELS})
        self.assertEqual(supersede, {"number": 5, "labels": [SUPERSEDED["name"]]})
        self.assertEqual(close, {"number": 5})
        self.assertEqual(comment["number"], 5)
        self.assertIn("https://example.test/pull/99", comment["body"])

    def test_closes_nothing_when_no_older_pull_request_exists(self):
        github = FakeGitHub(pulls=[pull_request(99, labels=[MARKER])])

        subject.open_pull_request(github, **self.INPUTS)

        self.assertEqual(github.names(), ["create_pull", "add_labels", "list_pulls"])

    def test_closes_nothing_when_creating_the_pull_request_fails(self):
        github = FakeGitHub(responses={"create_pull": http_error(422)})

        with self.assertRaises(subject.GitHubError) as raised:
            subject.open_pull_request(github, **self.INPUTS)

        self.assertEqual(raised.exception.status, 422)
        self.assertEqual(github.names(), ["create_pull"])


class CloseSupersededWithoutReplacementTest(QuietTestCase):
    def test_closes_open_updates_into_main_once_and_leaves_their_content_publishable(self):
        rolled_back = pull_request(7, labels=[MARKER])
        declined = older_pull_request(5, state="closed")
        github = FakeGitHub(
            pulls=[
                rolled_back,
                declined,
                older_pull_request(6, labels=[{"name": "automated"}]),
                older_pull_request(8, base={"ref": "release"}),
            ]
        )

        subject.close_superseded(github)

        self.assertEqual(
            github.names(), ["list_pulls", "get_label", "create_label", "add_labels", "close_pull", "create_comment"]
        )
        _, _, _, supersede, close, comment = (args for _, args in github.calls)
        self.assertEqual(supersede, {"number": 7, "labels": [SUPERSEDED["name"]]})
        self.assertEqual(close, {"number": 7})
        self.assertEqual(comment["number"], 7)
        self.assertIn("match upstream again", comment["body"])
        self.assertEqual((rolled_back["state"], rolled_back["labels"]), ("closed", [MARKER, SUPERSEDED]))

        # A second run with no change finds nothing left to close.
        github.calls.clear()
        subject.close_superseded(github)
        self.assertEqual(github.names(), ["list_pulls"])

        # The closure counts as superseded, so the content is proposed again if upstream returns to it, while the
        # declined pull request still holds back its own content.
        self.assertEqual(subject.find_report(github, BRANCH), "publish")
        self.assertEqual(subject.find_report(github, "ci/update-apm-semantics-old5"), "reported")
        self.assertEqual(declined["labels"], [MARKER])


def run_main(github, *args):
    """Runs the command line with `github` as the client and returns what it printed."""
    env = {"GITHUB_REPOSITORY": "DataDog/saluki", "GITHUB_TOKEN": "secret"}
    with unittest.mock.patch.dict(os.environ, env), unittest.mock.patch.object(
        subject, "GitHub", return_value=github
    ), contextlib.redirect_stdout(io.StringIO()) as stdout:
        subject.main(list(args))
    return stdout.getvalue()


class PartialSupersessionTest(QuietTestCase):
    """A run that fails part way through closing an older pull request, and the later run that finishes.

    Regression tests: `close_superseded` used to remove the marker label and then close. When closing failed, the older
    pull request stayed open without the label that later runs use to find it, so its stale mappings stayed mergeable.
    """

    STALE_BRANCH = "ci/update-apm-semantics-old5"
    CURRENT_LABELS = [MARKER, {"name": "automated"}]

    def github(self):
        """New update #99, not labelled yet, an older open update #5, and pull requests that must be left alone."""
        self.current = pull_request(99)
        self.stale = older_pull_request(5)
        self.untouched = [
            older_pull_request(6, state="closed"),
            older_pull_request(8, labels=[]),
            older_pull_request(10, base={"ref": "release"}),
        ]
        self.untouched_before = json.dumps(self.untouched)
        return FakeGitHub(pulls=[self.current, self.stale, *self.untouched])

    def comments_on(self, github, number):
        return [args["body"] for name, args in github.calls if name == "create_comment" and args["number"] == number]

    def assert_superseded(self, pull, github):
        """Asserts that `pull` is closed as superseded, its content is publishable again and nothing else changed."""
        self.assertEqual(pull["state"], "closed")
        self.assertIn(MARKER, pull["labels"])
        self.assertIn(SUPERSEDED, pull["labels"])
        self.assertEqual(json.dumps(self.untouched), self.untouched_before)
        branch = pull["head"]["label"].removeprefix("DataDog:")
        self.assertEqual(subject.find_report(github, branch), "publish")

    def replace_failing(self, call, effect=None):
        """Opens #99 and fails the first `call` made for #5, after running `effect` if given."""
        github = self.github()
        github.responses[call] = fail_once(503, 5, effect and (lambda: effect(github)))

        with self.assertRaises(subject.GitHubError):
            subject.open_pull_request(github, **OpenPullRequestTest.INPUTS)

        self.assertEqual(self.current["labels"], self.CURRENT_LABELS)
        return github

    def test_the_next_run_finishes_replacing_the_older_pull_request(self):
        def close(github):
            github._update(5, lambda pull: pull.update(state="closed"))

        cases = {
            "labelling it fails": ("add_labels", None, 1),
            "closing it fails": ("close_pull", None, 1),
            # The comment comes after closing, so it is lost along with the response.
            "closing it takes effect but its response is lost": ("close_pull", close, 0),
        }
        for name, (call, effect, comments) in cases.items():
            with self.subTest(name):
                github = self.replace_failing(call, effect)

                self.assertEqual(run_main(github, "find-report", "--branch", BRANCH), "reported\n")

                self.assertEqual((self.current["state"], self.current["labels"]), ("open", self.CURRENT_LABELS))
                self.assertEqual(self.stale["labels"], [MARKER, SUPERSEDED])
                bodies = self.comments_on(github, 5)
                self.assertEqual(len(bodies), comments)
                for body in bodies:
                    self.assertIn("https://example.test/pull/99", body)

                # Repeating the run changes nothing more.
                github.calls.clear()
                self.assertEqual(run_main(github, "find-report", "--branch", BRANCH), "reported\n")
                self.assertEqual(github.names(), ["list_pulls", "add_labels", "list_pulls"])

                self.assert_superseded(self.stale, github)

    def test_the_next_run_finishes_closing_updates_when_upstream_matches_the_embedded_mappings(self):
        github = self.github()
        self.current["labels"] = [MARKER]
        github.responses["close_pull"] = fail_once(503, 5)

        with self.assertRaises(subject.GitHubError):
            run_main(github, "close-superseded")

        self.assertEqual((self.stale["state"], self.stale["labels"]), ("open", [MARKER, SUPERSEDED]))

        self.assertEqual(run_main(github, "close-superseded"), "")

        self.assertEqual([len(self.comments_on(github, number)) for number in (99, 5)], [1, 1])
        self.assert_superseded(self.current, github)
        self.assert_superseded(self.stale, github)

    def test_the_next_run_finishes_replacing_even_when_a_reviewer_declined_the_replacement(self):
        github = self.replace_failing("close_pull")
        self.current["state"] = "closed"

        self.assertEqual(run_main(github, "find-report", "--branch", BRANCH), "reported\n")

        self.assertEqual((self.current["state"], self.current["labels"]), ("closed", self.CURRENT_LABELS))
        self.assert_superseded(self.stale, github)
        self.assertEqual(subject.find_report(github, BRANCH), "reported")

    def test_a_pull_request_left_open_is_current_again_when_upstream_returns_to_it(self):
        github = self.replace_failing("close_pull")
        self.assertEqual(self.stale["labels"], [MARKER, SUPERSEDED])

        self.assertEqual(run_main(github, "find-report", "--branch", self.STALE_BRANCH), "reported\n")

        self.assertEqual((self.stale["state"], self.stale["labels"]), ("open", self.CURRENT_LABELS))
        self.assert_superseded(self.current, github)

        # The pull request is an ordinary one again, so a reviewer closing it declines its change.
        self.stale["state"] = "closed"
        self.assertEqual(subject.find_report(github, self.STALE_BRANCH), "reported")


class SupersededLabelTest(QuietTestCase):
    """The superseded label is missing from the repository until the first run that closes a pull request creates it."""

    def test_creates_the_label_once_and_only_when_a_pull_request_needs_it(self):
        github = FakeGitHub(pulls=[pull_request(99, labels=[MARKER])])

        subject.close_superseded(github, github.pulls[0])

        self.assertEqual(github.names(), ["list_pulls"])
        self.assertNotIn(SUPERSEDED["name"], github.labels)

        github.pulls.append(older_pull_request(5))
        subject.close_superseded(github, github.pulls[0])

        [create] = [args for name, args in github.calls if name == "create_label"]
        self.assertEqual(
            create,
            {"name": SUPERSEDED["name"], "color": "c5def5", "description": subject.SUPERSEDED_DESCRIPTION},
        )
        self.assertLessEqual(len(create["description"]), 100)

        github.pulls.append(older_pull_request(6))
        github.calls.clear()
        subject.close_superseded(github, github.pulls[0])

        self.assertEqual(github.names(), ["list_pulls", "get_label", "add_labels", "close_pull", "create_comment"])
        self.assertEqual([pull["state"] for pull in github.pulls], ["open", "closed", "closed"])

    def test_closes_nothing_until_the_label_exists_and_a_later_run_finishes(self):
        def create(github):
            github.labels.add(SUPERSEDED["name"])

        cases = {
            "looking it up fails": ("get_label", None, 1),
            "creating it fails": ("create_label", None, 2),
            "creating it takes effect but its response is lost": ("create_label", create, 1),
        }
        for name, (call, effect, creates) in cases.items():
            with self.subTest(name):
                stale = older_pull_request(5)
                github = FakeGitHub(pulls=[stale])
                github.responses[call] = fail_once(503, effect=effect and (lambda: effect(github)))

                with self.assertRaises(subject.GitHubError):
                    run_main(github, "close-superseded")

                self.assertEqual((stale["state"], stale["labels"]), ("open", [MARKER]))
                self.assertNotIn("add_labels", github.names())

                run_main(github, "close-superseded")

                self.assertEqual((stale["state"], stale["labels"]), ("closed", [MARKER, SUPERSEDED]))
                self.assertEqual(github.names().count("create_label"), creates)


class WorkflowTest(unittest.TestCase):
    """Checks that the workflow hands each outcome of the script to the pull request module."""

    def step(self, name):
        [step] = [step for step in WORKFLOW.read_text().split("\n      - name: ") if step.startswith(f"{name}\n")]
        return step

    def test_a_run_with_no_change_closes_superseded_pull_requests_with_the_write_token(self):
        refresh = self.step("Refresh the mappings")
        close = self.step("Close superseded pull requests")

        self.assertIn('echo "needs_update=false" >> "$GITHUB_OUTPUT"', refresh)
        self.assertIn("if: steps.update.outputs.needs_update == 'false'\n", close)
        self.assertIn('GITHUB_TOKEN: "${{ steps.octo-sts.outputs.token }}"', close)
        self.assertIn("run: python3 ci/tooling/update_apm_semantics_pr.py close-superseded\n", close)


class GitHubClientTest(unittest.TestCase):
    """Drives the real client with `urlopen` replaced, to check the requests it builds."""

    def client(self, *responses):
        github = subject.GitHub("DataDog/saluki", "secret")
        requests = []
        pending = list(responses)

        def urlopen(request, timeout):
            requests.append(request)
            response = pending.pop(0)
            if isinstance(response, Exception):
                raise response
            return io.BytesIO(json.dumps(response).encode())

        patcher = unittest.mock.patch.object(subject.urllib.request, "urlopen", urlopen)
        patcher.start()
        self.addCleanup(patcher.stop)
        return github, requests

    def test_encodes_the_label_but_not_the_branch_path(self):
        github, requests = self.client(None, None)

        github.remove_label(5, "ci/update-apm-semantics")
        github.delete_branch("ci/update-apm-semantics-0123456789ab")

        self.assertEqual(
            [(r.get_method(), r.full_url) for r in requests],
            [
                ("DELETE", "https://api.github.com/repos/DataDog/saluki/issues/5/labels/ci%2Fupdate-apm-semantics"),
                (
                    "DELETE",
                    "https://api.github.com/repos/DataDog/saluki/git/refs/heads/ci/update-apm-semantics-0123456789ab",
                ),
            ],
        )

    def test_gets_and_creates_the_label_by_its_encoded_name(self):
        github, requests = self.client({"name": "ci/update-apm-semantics-superseded"}, {})

        github.get_label("ci/update-apm-semantics-superseded")
        github.create_label("ci/update-apm-semantics-superseded", "c5def5", "Closed as superseded.")

        get, create = requests
        self.assertEqual(
            (get.get_method(), get.full_url),
            ("GET", "https://api.github.com/repos/DataDog/saluki/labels/ci%2Fupdate-apm-semantics-superseded"),
        )
        self.assertEqual(
            (create.get_method(), create.full_url), ("POST", "https://api.github.com/repos/DataDog/saluki/labels")
        )
        self.assertEqual(
            json.loads(create.data),
            {"name": "ci/update-apm-semantics-superseded", "color": "c5def5", "description": "Closed as superseded."},
        )

    def test_sends_the_token_and_a_json_body(self):
        github, requests = self.client({"number": 3})

        github.create_pull("title", "branch", "body")

        [request] = requests
        self.assertEqual(request.get_header("Authorization"), "Bearer secret")
        self.assertEqual(request.get_header("Content-type"), "application/json")
        self.assertEqual(
            json.loads(request.data), {"title": "title", "head": "branch", "base": "main", "body": "body"}
        )

    def test_lists_pages_until_a_short_one(self):
        full_page = [{"number": n} for n in range(subject.PAGE_SIZE)]
        github, requests = self.client(full_page, [{"number": 1000}])

        pulls = github.list_pulls(state="open", base="main")

        self.assertEqual(len(pulls), subject.PAGE_SIZE + 1)
        self.assertEqual(
            [r.full_url.split("?")[1] for r in requests],
            [
                "state=open&per_page=100&base=main&page=1",
                "state=open&per_page=100&base=main&page=2",
            ],
        )

    def test_reports_the_http_status(self):
        error = urllib.error.HTTPError("https://api.github.com", 404, "Not Found", {}, None)
        github, _ = self.client(error)

        with self.assertRaises(subject.GitHubError) as raised:
            github.delete_branch("gone")

        self.assertEqual(raised.exception.status, 404)


class CommandLineTest(QuietTestCase):
    def test_find_report_prints_only_the_decision(self):
        self.assertEqual(run_main(FakeGitHub(), "find-report", "--branch", BRANCH), "publish\n")

    def test_close_superseded_closes_every_open_update(self):
        github = FakeGitHub(pulls=[older_pull_request(5), older_pull_request(6)])

        self.assertEqual(run_main(github, "close-superseded"), "")

        self.assertEqual([args for name, args in github.calls if name == "close_pull"], [{"number": 5}, {"number": 6}])

    def test_open_pr_reads_the_commit_body_from_a_file(self):
        github = FakeGitHub()
        body_file = Path(tempfile.mkdtemp(prefix="update-apm-semantics-")) / "body.txt"
        self.addCleanup(shutil.rmtree, body_file.parent, ignore_errors=True)
        body_file.write_text("Added concepts:\n- added.concept\n")

        run_main(
            github,
            "open-pr",
            "--branch", BRANCH,
            "--title", "the title",
            "--commit-body-file", str(body_file),
            "--previous", OLD_COMMIT,
            "--source", NEW_COMMIT,
        )

        self.assertEqual(github.calls[0][0], "create_pull")
        self.assertEqual(github.calls[0][1]["title"], "the title")
        self.assertIn("Added concepts:\n- added.concept\n\nChanges since", github.calls[0][1]["body"])


if __name__ == "__main__":
    unittest.main()
