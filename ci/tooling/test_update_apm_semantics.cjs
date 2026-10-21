'use strict';

// Tests for `update-apm-semantics.sh` and `update-apm-semantics-pr.cjs`. The script runs for real inside a throwaway
// git repository, with a fake `curl` standing in for the network. The module runs against fake GitHub clients.

const assert = require('node:assert/strict');
const { spawnSync } = require('node:child_process');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { afterEach, describe, test } = require('node:test');

const { findReport, openPullRequest } = require('./update-apm-semantics-pr.cjs');

const SCRIPT = path.join(__dirname, 'update-apm-semantics.sh');
const SEMANTICS_DIR = 'lib/datadog-agent/apm/src/semantics';
const MAPPINGS = `${SEMANTICS_DIR}/mappings.json`;
const VERSION_FILE = `${SEMANTICS_DIR}/_version.txt`;

const OLD_COMMIT = 'a'.repeat(40);
const NEW_COMMIT = 'b1c2d3e4f5a6'.padEnd(40, '0');

// Stands in for `curl -fsSL --retry 3 [-H header] -o <file> <url>`. It logs the URL and the header sent, if any, then
// copies the response named by the URL. A missing response is an HTTP failure.
const FAKE_CURL = `#!/bin/sh
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
`;

function mappings(concepts, contentHash = 'sha256:abc') {
  return JSON.stringify({ metadata: { content_hash: contentHash }, concepts }, null, 2) + '\n';
}

const OLD_MAPPINGS = mappings({
  'kept.same': { fallbacks: [1] },
  'kept.changed': { fallbacks: [1] },
  'dropped.concept': { fallbacks: [1] },
});

const NEW_MAPPINGS = mappings({
  'kept.same': { fallbacks: [1] },
  'kept.changed': { fallbacks: [1, 2] },
  'added.concept': { fallbacks: [1] },
});

// Keeps the developer's own git configuration (commit signing, hooks, templates) out of the fixture repositories.
const ISOLATED_GIT_ENV = { GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_NOSYSTEM: '1' };

function git(dir, ...args) {
  const env = { ...process.env, ...ISOLATED_GIT_ENV };
  const result = spawnSync('git', ['-C', dir, ...args], { encoding: 'utf8', env });
  assert.equal(result.status, 0, `git ${args.join(' ')} failed: ${result.stderr}`);
  return result.stdout;
}

const cleanups = [];
afterEach(() => {
  while (cleanups.length > 0) {
    fs.rmSync(cleanups.pop(), { recursive: true, force: true });
  }
});

/**
 * Builds a git repository holding the real script, a vendored snapshot and a seed commit, plus a directory for the
 * fake `curl`. `api` and `raw` are the contents of the two responses, and a missing one makes `curl` fail.
 */
function fixture({ api, raw, seedVersion = true, oldMappings = OLD_MAPPINGS } = {}) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'update-apm-semantics-'));
  cleanups.push(root);

  const repo = path.join(root, 'repo');
  const bin = path.join(root, 'bin');
  const curlDir = path.join(root, 'curl');
  for (const dir of [path.join(repo, SEMANTICS_DIR), path.join(repo, 'ci/tooling'), bin, curlDir]) {
    fs.mkdirSync(dir, { recursive: true });
  }

  fs.copyFileSync(SCRIPT, path.join(repo, 'ci/tooling/update-apm-semantics.sh'));
  fs.chmodSync(path.join(repo, 'ci/tooling/update-apm-semantics.sh'), 0o755);
  fs.writeFileSync(path.join(repo, MAPPINGS), oldMappings);
  if (seedVersion) {
    fs.writeFileSync(path.join(repo, VERSION_FILE), `${OLD_COMMIT}\n`);
  }
  fs.writeFileSync(path.join(repo, 'tracked.txt'), 'original\n');

  fs.writeFileSync(path.join(bin, 'curl'), FAKE_CURL, { mode: 0o755 });
  if (api !== undefined) {
    fs.writeFileSync(path.join(curlDir, 'api.json'), api);
  }
  if (raw !== undefined) {
    fs.writeFileSync(path.join(curlDir, 'raw.json'), raw);
  }

  git(repo, 'init', '--quiet', '-b', 'main');
  git(repo, 'config', 'user.name', 'Test');
  git(repo, 'config', 'user.email', 'test@example.com');
  git(repo, 'add', '--all');
  git(repo, 'commit', '--quiet', '-m', 'seed');

  const run = (args = [], extraEnv = {}) =>
    spawnSync(path.join(repo, 'ci/tooling/update-apm-semantics.sh'), args, {
      cwd: repo,
      encoding: 'utf8',
      env: {
        PATH: `${bin}:${process.env.PATH}`,
        HOME: root,
        ...ISOLATED_GIT_ENV,
        FAKE_CURL_DIR: curlDir,
        ...extraEnv,
      },
    });

  const curlLog = () => {
    const log = path.join(curlDir, 'curl.log');
    return fs.existsSync(log) ? fs.readFileSync(log, 'utf8').split('\n').filter(Boolean) : [];
  };

  return { repo, run, curlLog, head: () => git(repo, 'rev-parse', 'HEAD').trim() };
}

const apiResponse = (sha = NEW_COMMIT) => JSON.stringify([{ sha }]);

// Asserts that a run changed nothing: no commit, a clean tree and the original contents.
function assertUntouched(fx, seedHead) {
  assert.equal(fx.head(), seedHead);
  assert.equal(git(fx.repo, 'status', '--porcelain'), '');
  assert.equal(fs.readFileSync(path.join(fx.repo, MAPPINGS), 'utf8'), OLD_MAPPINGS);
}

describe('update-apm-semantics.sh', () => {
  test('does nothing when the bytes are the same, even if the upstream commit differs', () => {
    const fx = fixture({ api: apiResponse(), raw: OLD_MAPPINGS });
    const seedHead = fx.head();

    const result = fx.run();

    assert.equal(result.status, 0, result.stderr);
    assertUntouched(fx, seedHead);
    assert.equal(fs.readFileSync(path.join(fx.repo, VERSION_FILE), 'utf8'), `${OLD_COMMIT}\n`);
  });

  test('commits only the two paths and leaves unrelated changes where they were', () => {
    const fx = fixture({ api: apiResponse(), raw: NEW_MAPPINGS });
    const seedHead = fx.head();
    fs.writeFileSync(path.join(fx.repo, 'staged.txt'), 'staged\n');
    git(fx.repo, 'add', 'staged.txt');
    fs.writeFileSync(path.join(fx.repo, 'tracked.txt'), 'edited\n');

    const result = fx.run();

    assert.equal(result.status, 0, result.stderr);
    assert.equal(git(fx.repo, 'rev-list', '--count', `${seedHead}..HEAD`).trim(), '1');
    assert.deepEqual(git(fx.repo, 'show', '--name-only', '--format=', 'HEAD').split('\n').filter(Boolean).sort(), [
      VERSION_FILE,
      MAPPINGS,
    ].sort());
    assert.equal(fs.readFileSync(path.join(fx.repo, MAPPINGS), 'utf8'), NEW_MAPPINGS);
    assert.equal(fs.readFileSync(path.join(fx.repo, VERSION_FILE), 'utf8'), `${NEW_COMMIT}\n`);

    const message = git(fx.repo, 'log', '-1', '--format=%B');
    assert.match(message, /^chore\(apm\): refresh semantic mappings from datadog-agent b1c2d3e4f5a6\n/);
    assert.match(message, /Added concepts:\n- added\.concept\n/);
    assert.match(message, /Removed concepts:\n- dropped\.concept\n/);
    assert.match(message, /Changed concepts:\n- kept\.changed\n/);
    assert.match(message, new RegExp(`/commit/${NEW_COMMIT}`));

    assert.equal(git(fx.repo, 'diff', '--cached', '--name-only'), 'staged.txt\n');
    assert.equal(git(fx.repo, 'diff', '--name-only'), 'tracked.txt\n');
    assert.equal(fs.readFileSync(path.join(fx.repo, 'tracked.txt'), 'utf8'), 'edited\n');
  });

  test('says "none" for a list with nothing in it', () => {
    const fx = fixture({
      api: apiResponse(),
      raw: mappings({ ...JSON.parse(OLD_MAPPINGS).concepts, 'added.concept': { fallbacks: [1] } }),
    });

    const result = fx.run();

    assert.equal(result.status, 0, result.stderr);
    const message = git(fx.repo, 'log', '-1', '--format=%B');
    assert.match(message, /Removed concepts: none\n/);
    assert.match(message, /Changed concepts: none\n/);
  });

  test('passes REF to the commit query', () => {
    const fx = fixture({ api: apiResponse(), raw: OLD_MAPPINGS });

    assert.equal(fx.run(['feature/x-1.2']).status, 0);

    assert.match(fx.curlLog()[0], /pkg\/trace\/semantics\/mappings\.json&sha=feature\/x-1\.2&per_page=1$/);
  });

  test('defaults REF to main, including when it is empty', () => {
    for (const args of [[], ['']]) {
      const fx = fixture({ api: apiResponse(), raw: OLD_MAPPINGS });

      assert.equal(fx.run(args).status, 0);

      assert.match(fx.curlLog()[0], /&sha=main&/);
    }
  });

  for (const ref of ['a&b', '-x', 'with space', 'semi;colon', 'new\nline']) {
    test(`rejects REF ${JSON.stringify(ref)} before any network call`, () => {
      const fx = fixture({ api: apiResponse(), raw: NEW_MAPPINGS });
      const seedHead = fx.head();

      const result = fx.run([ref]);

      assert.notEqual(result.status, 0);
      assert.deepEqual(fx.curlLog(), []);
      assertUntouched(fx, seedHead);
    });
  }

  const badUpstream = {
    'invalid JSON': { api: apiResponse(), raw: 'not json' },
    'empty concepts': { api: apiResponse(), raw: mappings({}) },
    'missing content_hash': { api: apiResponse(), raw: JSON.stringify({ metadata: {}, concepts: { a: {} } }) },
    'an empty commit list': { api: '[]', raw: NEW_MAPPINGS },
    'a SHA that is not hex': { api: apiResponse('Z'.repeat(40)), raw: NEW_MAPPINGS },
    'a SHA that is too short': { api: apiResponse('abc123'), raw: NEW_MAPPINGS },
    'a failing commit query': { raw: NEW_MAPPINGS },
    'a failing download': { api: apiResponse() },
  };
  for (const [name, responses] of Object.entries(badUpstream)) {
    test(`fails and leaves no trace on ${name}`, () => {
      const fx = fixture(responses);
      const seedHead = fx.head();

      const result = fx.run();

      assert.notEqual(result.status, 0);
      assertUntouched(fx, seedHead);
      assert.deepEqual(fs.readdirSync(path.join(fx.repo, SEMANTICS_DIR)).sort(), ['_version.txt', 'mappings.json']);
    });
  }

  test('fails rather than describe the change wrongly when the vendored copy is not JSON', () => {
    const fx = fixture({ api: apiResponse(), raw: NEW_MAPPINGS, oldMappings: 'not json\n' });
    const seedHead = fx.head();

    const result = fx.run();

    assert.notEqual(result.status, 0);
    assert.equal(fx.head(), seedHead);
    assert.equal(git(fx.repo, 'status', '--porcelain'), '');
  });

  const dirtyTargets = {
    'an unstaged change to mappings.json': (fx) => fs.appendFileSync(path.join(fx.repo, MAPPINGS), ' '),
    'a staged change to mappings.json': (fx) => {
      fs.appendFileSync(path.join(fx.repo, MAPPINGS), ' ');
      git(fx.repo, 'add', MAPPINGS);
    },
    'an untracked _version.txt': (fx) => fs.writeFileSync(path.join(fx.repo, VERSION_FILE), 'x\n'),
  };
  for (const [name, dirty] of Object.entries(dirtyTargets)) {
    test(`refuses to run with ${name}`, () => {
      const fx = fixture({ api: apiResponse(), raw: NEW_MAPPINGS, seedVersion: name !== 'an untracked _version.txt' });
      const seedHead = fx.head();
      dirty(fx);
      const before = git(fx.repo, 'status', '--porcelain');

      const result = fx.run();

      assert.notEqual(result.status, 0);
      assert.deepEqual(fx.curlLog(), []);
      assert.equal(fx.head(), seedHead);
      assert.equal(git(fx.repo, 'status', '--porcelain'), before);
    });
  }

  test('sends GITHUB_TOKEN to the API and nowhere else', () => {
    const fx = fixture({ api: apiResponse(), raw: NEW_MAPPINGS });

    const result = fx.run([], { GITHUB_TOKEN: 'secret' });

    assert.equal(result.status, 0, result.stderr);
    const [api, raw] = fx.curlLog();
    assert.match(api, /^auth=Authorization: Bearer secret https:\/\/api\.github\.com\//);
    assert.match(raw, /^auth=none https:\/\/raw\.githubusercontent\.com\//);
    assert.ok(!result.stdout.includes('secret') && !result.stderr.includes('secret'));
  });

  test('sends no header without GITHUB_TOKEN', () => {
    const fx = fixture({ api: apiResponse(), raw: OLD_MAPPINGS });

    assert.equal(fx.run().status, 0);

    assert.ok(fx.curlLog().every((line) => line.startsWith('auth=none ')));
  });
});

// Records every call as `[name, args]`, in order. `responses` maps a call name to the value it returns, or to a
// function that returns it or throws. `pulls.list` filters `pullRequests` by `head`, `state` and `base` as GitHub does.
function fakeClients({ responses = {}, pullRequests = [] } = {}) {
  const calls = [];
  const respond = (name, args, fallback) => {
    calls.push([name, args]);
    const response = name in responses ? responses[name] : fallback;
    return typeof response === 'function' ? response(args) : response;
  };

  const list = (args) =>
    respond('pulls.list', args, {
      data: pullRequests.filter(
        (pr) =>
          (args.head === undefined || pr.head.label === args.head) &&
          (args.state === 'all' || pr.state === (args.state ?? 'open')) &&
          (args.base === undefined || pr.base.ref === args.base),
      ),
    });
  const github = {
    calls,
    rest: {
      pulls: {
        list,
        create: async (args) =>
          respond('pulls.create', args, { data: { number: 99, html_url: 'https://example.test/pull/99' } }),
        update: async (args) => respond('pulls.update', args, {}),
      },
      issues: {
        addLabels: async (args) => respond('issues.addLabels', args, {}),
        removeLabel: async (args) => respond('issues.removeLabel', args, {}),
        createComment: async (args) => respond('issues.createComment', args, {}),
      },
      git: { deleteRef: async (args) => respond('git.deleteRef', args, {}) },
    },
    paginate: async (method, args) => (await method(args)).data,
  };
  const context = { repo: { owner: 'DataDog', repo: 'saluki' } };
  const core = { info: () => {} };
  return { github, context, core, names: () => calls.map(([name]) => name) };
}

const httpError = (status) => () => {
  throw Object.assign(new Error(`HTTP ${status}`), { status });
};

const BRANCH = 'ci/update-apm-semantics-0123456789ab';
const MARKER = { name: 'ci/update-apm-semantics' };
const LABELS = ['ci/update-apm-semantics', 'automated'];

// An open, unlabelled pull request from `BRANCH` into `main`, unless `fields` says otherwise.
const pullRequest = (number, fields) => ({
  number,
  html_url: `https://example.test/pull/${number}`,
  head: { label: `DataDog:${BRANCH}` },
  base: { ref: 'main' },
  state: 'open',
  merged_at: null,
  labels: [],
  ...fields,
});
const olderPullRequest = (number, fields) =>
  pullRequest(number, { head: { label: `DataDog:ci/update-apm-semantics-old${number}` }, labels: [MARKER], ...fields });

describe('findReport', () => {
  test('deletes the branch when there is no pull request, tolerating a missing branch', async () => {
    const clients = fakeClients({ responses: { 'git.deleteRef': httpError(404) } });

    assert.equal(await findReport({ ...clients, branch: BRANCH }), 'publish');

    assert.deepEqual(clients.names(), ['pulls.list', 'git.deleteRef']);
    assert.equal(clients.github.calls[0][1].head, `DataDog:${BRANCH}`);
    assert.equal(clients.github.calls[0][1].state, 'all');
    assert.equal(clients.github.calls[1][1].ref, `heads/${BRANCH}`);
  });

  test('repairs the labels and closes older pull requests when a pull request is open', async () => {
    const clients = fakeClients({
      pullRequests: [pullRequest(7), olderPullRequest(5), olderPullRequest(6, { labels: [] })],
    });

    assert.equal(await findReport({ ...clients, branch: BRANCH }), 'reported');

    assert.deepEqual(clients.names(), [
      'pulls.list',
      'issues.addLabels',
      'pulls.list',
      'issues.removeLabel',
      'pulls.update',
      'issues.createComment',
    ]);
    assert.deepEqual(clients.github.calls[1][1], { owner: 'DataDog', repo: 'saluki', issue_number: 7, labels: LABELS });
    assert.equal(clients.github.calls[4][1].pull_number, 5);
    assert.ok(clients.github.calls[5][1].body.includes('https://example.test/pull/7'));
  });

  test('does nothing when a reviewer closed the pull request', async () => {
    const clients = fakeClients({ pullRequests: [pullRequest(7, { state: 'closed', labels: [MARKER] })] });

    assert.equal(await findReport({ ...clients, branch: BRANCH }), 'reported');

    assert.deepEqual(clients.names(), ['pulls.list']);
  });

  for (const [name, fields] of Object.entries({
    merged: { state: 'closed', merged_at: '2026-01-01T00:00:00Z', labels: [MARKER] },
    superseded: { state: 'closed' },
  })) {
    test(`publishes again when the only pull request was ${name}`, async () => {
      const clients = fakeClients({ pullRequests: [pullRequest(7, fields)] });

      assert.equal(await findReport({ ...clients, branch: BRANCH }), 'publish');

      assert.deepEqual(clients.names(), ['pulls.list', 'git.deleteRef']);
    });
  }

  test('tolerates an unprocessable branch deletion', async () => {
    const clients = fakeClients({ responses: { 'git.deleteRef': httpError(422) } });

    assert.equal(await findReport({ ...clients, branch: BRANCH }), 'publish');
  });

  test('rethrows any other branch deletion failure', async () => {
    const clients = fakeClients({ responses: { 'git.deleteRef': httpError(500) } });

    await assert.rejects(findReport({ ...clients, branch: BRANCH }), { status: 500 });
  });
});

describe('openPullRequest', () => {
  const inputs = {
    branch: BRANCH,
    title: 'chore(apm): refresh semantic mappings from datadog-agent b1c2d3e4f5a6',
    commitBody: 'Added concepts:\n- added.concept',
    previous: OLD_COMMIT,
    source: NEW_COMMIT,
  };

  test('creates and labels the pull request, then closes older labelled ones only', async () => {
    const clients = fakeClients({
      pullRequests: [
        pullRequest(99, { labels: [MARKER] }),
        olderPullRequest(5),
        olderPullRequest(6, { labels: [{ name: 'automated' }, { name: 'unrelated' }] }),
        olderPullRequest(8, { labels: [] }),
        olderPullRequest(9, { state: 'closed' }),
        olderPullRequest(10, { base: { ref: 'release' } }),
      ],
    });

    const created = await openPullRequest({ ...clients, ...inputs });

    assert.equal(created.number, 99);
    assert.deepEqual(clients.names(), [
      'pulls.create',
      'issues.addLabels',
      'pulls.list',
      'issues.removeLabel',
      'pulls.update',
      'issues.createComment',
    ]);

    const [[, createArgs], [, labelArgs], , [, unlabelArgs], [, closeArgs], [, commentArgs]] = clients.github.calls;
    assert.equal(createArgs.title, inputs.title);
    assert.equal(createArgs.head, BRANCH);
    assert.equal(createArgs.base, 'main');
    assert.ok(createArgs.body.includes(inputs.commitBody));
    assert.ok(createArgs.body.includes(`compare/${OLD_COMMIT}...${NEW_COMMIT}`));
    assert.deepEqual(labelArgs.labels, LABELS);
    assert.equal(labelArgs.issue_number, 99);
    assert.deepEqual([unlabelArgs.issue_number, unlabelArgs.name], [5, MARKER.name]);
    assert.equal(closeArgs.pull_number, 5);
    assert.equal(closeArgs.state, 'closed');
    assert.equal(commentArgs.issue_number, 5);
    assert.ok(commentArgs.body.includes('https://example.test/pull/99'));
  });

  test('closes nothing when no older pull request exists', async () => {
    const clients = fakeClients({ pullRequests: [pullRequest(99, { labels: [MARKER] })] });

    await openPullRequest({ ...clients, ...inputs });

    assert.deepEqual(clients.names(), ['pulls.create', 'issues.addLabels', 'pulls.list']);
  });

  test('closes nothing when creating the pull request fails', async () => {
    const clients = fakeClients({ responses: { 'pulls.create': httpError(422) } });

    await assert.rejects(openPullRequest({ ...clients, ...inputs }), { status: 422 });

    assert.deepEqual(clients.names(), ['pulls.create']);
  });
});
