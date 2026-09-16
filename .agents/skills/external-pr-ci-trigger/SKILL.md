---
name: external-pr-ci-trigger
description: >
  Trigger CI for an external contributor's pull request by mirroring their fork branch onto
  DataDog/saluki under an `<owner>/<branch>` name. External forks don't run CI by default for
  security reasons; this skill lets an authorized maintainer push the fork's branch up to the
  main repo so the PR's CI checks attach to a trusted branch. TRIGGER when: user asks to run CI
  on an external/contributor PR, push an external branch for CI, mirror a fork branch, or
  invokes `/external-pr-ci-trigger`. DO NOT TRIGGER when: user wants to run CI on their own
  branch or on an internal PR — those run CI automatically.
argument-hint: <pr-number> | <owner>:<branch>   e.g. `1234`, `octocat:fix/something`, `octocat:main`
disable-model-invocation: true
allowed-tools: Bash, Read
---

# /external-pr-ci-trigger

Mirror an external contributor's fork branch onto `DataDog/saluki` as `<owner>/<branch>` so CI
runs against the contributor's PR. The contributor's PR keeps its check status updated by GitHub
because the head commit of the mirrored branch matches the PR's head commit.

This skill performs destructive-ish local-git operations (adding a remote, creating a branch,
pushing to `origin`). Read the steps carefully, validate preconditions, and stop on the first
failure with the printed recovery commands.

## Argument

A single positional argument in one of two forms:

**Form A — PR number (preferred):** an all-digits string like `1234`. The skill resolves the
contributor's owner, branch, and fork repo name from the PR via `gh`. See Step 0.

**Form B — explicit owner/branch:** `<owner>:<branch>`, for cases where you don't have a PR
number handy (for example, you want to mirror a branch that doesn't have a PR yet).

- `<owner>` matches `[a-zA-Z0-9_-]+`
- `<branch>` matches `[a-zA-Z0-9.#_/-]+` (dots are allowed for branches like `release/1.2.3`)
- The whole string MUST NOT contain a single quote (`'`).

Examples: `1234`, `octocat:fix-thing`, `octocat:feature/cool-thing`, `octocat:user#123/wip`.

Detection: if the argument is purely digits, treat it as Form A. Otherwise it must contain a
`:`; split once on the first `:` into `OWNER` and `BRANCH` (Form B). If neither shape matches,
or it contains a `'`, stop and ask the user to re-supply it.

The pushed branch on `origin` will be named `OWNER/BRANCH` (with a `/` separator) in both forms.

## Step 0: Resolve PR (Form A only — skip if Form B)

For a PR-number argument, fetch the PR's head ref in one call:

```bash
gh pr view <PR-NUMBER> --repo DataDog/saluki \
  --json headRefName,headRepositoryOwner,headRepository,isCrossRepository
```

From the JSON:

- `isCrossRepository` MUST be `true`. If `false`, the PR's branch already lives on
  `DataDog/saluki` and CI is running normally — abort and tell the user this skill isn't needed.
- `headRepositoryOwner.login` → `OWNER`
- `headRefName` → `BRANCH`
- `headRepository.name` → `FORK_NAME` (used in Step 4a; this lets you skip Step 2 entirely for
  Form A)

If `gh pr view` fails (PR doesn't exist, no access, etc.), surface the error and stop.

Validate the resolved `OWNER` and `BRANCH` against the same character-class rules from the
Argument section — a malicious or unexpected PR head ref shouldn't be allowed to inject shell
metacharacters. If either fails validation, abort.

## Step 1: Preconditions

Run each check. If any fails, stop immediately and tell the user what's wrong — do not attempt
to "fix" their working tree.

```bash
# 1a. Working tree must be clean. Any output here aborts.
git status --porcelain

# 1b. The target local branch must not already exist.
git branch --list "OWNER/BRANCH"

# 1c. A remote named OWNER must not already exist.
git remote | grep -Fx "OWNER" || true

# 1d. Capture the current branch so we can restore it at the end.
git branch --show-current

# 1e. origin MUST point at DataDog/saluki. A clone where origin is a personal fork would
#     silently push to the wrong repo and the contributor's PR check status would never
#     update. Accept either SSH or HTTPS form, with or without a trailing `.git`.
git remote get-url origin | grep -Ex '(git@github\.com:|https://github\.com/)DataDog/saluki(\.git)?' || true
```

For 1a: any non-empty output means the working tree is dirty — abort and ask the user to commit
or stash first.

For 1b and 1c: any non-empty output means a stale leftover exists from a prior run. Show the
user the recovery commands below and ask them to clean up before retrying.

Save the output of 1d as `CURR_BRANCH` for use in Step 5.

For 1e: empty grep output (no match) means `origin` is not `DataDog/saluki` — abort and tell the
user. Do not attempt to "fix" their remotes.

## Step 2: Discover the fork's repository name (Form B only — skip if Form A)

For Form A, `FORK_NAME` was resolved in Step 0; skip this step entirely.

For Form B, the contributor's fork is *usually* named `saluki`, but GitHub allows fork renames.
Resolve the real name via the GitHub API.

```bash
# Fast path: assume the fork is named "saluki" and verify its parent is DataDog/saluki.
gh repo view "OWNER/saluki" --json parent --jq '.parent.nameWithOwner' 2>/dev/null
```

If that prints `DataDog/saluki`, set `FORK_NAME=saluki` and continue.

Otherwise, search the user's forks via GraphQL:

```bash
gh api graphql -f query='
  query($login: String!) {
    user(login: $login) {
      repositories(isFork: true, first: 100, ownerAffiliations: OWNER) {
        nodes { name parent { nameWithOwner } }
      }
    }
  }' -F login=OWNER \
  --jq '.data.user.repositories.nodes[] | select(.parent.nameWithOwner == "DataDog/saluki") | .name'
```

If exactly one name is returned, use it as `FORK_NAME`. If zero are returned, abort and tell the
user the owner has no fork of `DataDog/saluki`. If more than one is returned (rare — usually
means a fork-of-a-fork situation), show the list and ask the user which one to use.

## Step 3: Recovery commands (print these now)

Before running anything destructive, surface the cleanup commands so the user can copy them if
something goes wrong mid-flow:

```bash
git remote remove 'OWNER'
git checkout 'CURR_BRANCH'
git branch -d 'OWNER/BRANCH'
```

If any command in Step 4 fails, print these and stop. Do not try to continue past a failure or
auto-retry — the user needs to inspect state.

## Step 4: Fetch, branch, push

Run sequentially. If any command exits non-zero, stop and print the recovery commands.

```bash
# 4a. Add the contributor's fork as a remote.
git remote add OWNER git@github.com:OWNER/FORK_NAME.git

# 4b. Fetch only the branch we care about.
git fetch 'OWNER' 'BRANCH'

# 4c. Detached checkout of the fetched commit, then create a local branch from it.
#     The two-step is intentional: the first checkout uses the remote ref and leaves HEAD
#     detached; the second creates the named branch at that commit.
git checkout 'OWNER/BRANCH'
git checkout -b 'OWNER/BRANCH'

# 4d. Push to origin (DataDog/saluki), setting upstream.
git push --set-upstream origin 'OWNER/BRANCH'
```

If the user passes `--no-verify` as an extra argument, append ` --no-verify` to the push only.
Otherwise do not skip hooks.

## Step 5: Restore local state

These run unconditionally after a successful push (and are also the recovery commands from
Step 3).

```bash
git remote remove 'OWNER'
git checkout 'CURR_BRANCH'
git branch -d 'OWNER/BRANCH'
```

The local `OWNER/BRANCH` is safe to delete because we just pushed it; `origin/OWNER/BRANCH` is
the source of truth from here on.

## Anti-patterns

- **Don't `git reset --hard` or `git clean` to "make the working tree clean."** If preconditions
  fail, surface the failure — the user's uncommitted work is theirs to handle.
- **Don't retry on failure.** A failed `git fetch` or `git push` usually means the branch
  doesn't exist on the fork, or the user lacks push access to `DataDog/saluki`. Surface the
  error.
- **Don't push to anywhere other than `origin`.** This skill assumes `origin` is
  `DataDog/saluki`. If `git remote get-url origin` returns something else, abort.
- **Don't run on a dirty tree even if "it looks safe."** The branch-switching in Step 4 will
  carry uncommitted changes into the contributor's branch and may corrupt the push.
