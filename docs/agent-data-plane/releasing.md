# Releasing ADP

This page covers the overall release process for Agent Data Plane, including the steps required to create a new release,
as well as the technical details around how we determine the version of the release, what metadata is
collected/generated, and more.

## High-level overview

Each ADP minor version is a **release line**, developed on a `releases/X.Y.x` branch. A release line supports one
Datadog Agent minor, its **target**, for its whole life. The release process roughly looks like this:

- for a new release line, cut `releases/X.Y.x` from `main`
- open a PR into the release branch to select the correct Agent version in `.datadog-agent-version.json`
- create a GitHub release, which tags the release branch at a particular point
- creation of the Git tag triggers a CI pipeline that checks the tag and additionally unlocks jobs to publish release
  artifacts
- the additional CI jobs, once manually triggered, will publish ADP container images to various public container image
  registries
- bump the version of ADP to the next development version
- post a release announcement in the `#agent-data-plane` Slack channel

## Release lines and their target

ADP major.minor is (Agent major - 6).(Agent minor - 78). For example, Agent 7.86 uses ADP 1.8, and Agent 7.87 uses ADP
1.9.

The target lives in the release branch's `.datadog-agent-version.json`, as `"target": "7.87"`. `main` has no target.
The Linux ADP build on a release branch compiles with the target's patch zero, such as `DD_AGENT_VERSION=7.87.0`, so
version gates select the target's behavior even when CI tests a later RC or patch. [Datadog Agent
version](../development/agent-version.md#what-target-changes) lists everything else the target changes.

The target is set once, by the release-cut commit. After the cut, no pull request may add, remove, or change it. Adding
it is allowed only when the release branch doesn't have it yet: at the cut, or in a reviewed migration of an older line.

## Cutting a new release line

Use this order. The target must exist in the commit you tag, so the branch comes first. The examples cut ADP 1.9 for
Agent 7.87.

Cut the line before the Agent cuts its release branch for the target minor, so that branch starts with this ADP line.
Until the next cut, Agent `main` uses this line, one minor behind its own version. The rule of one Agent minor per ADP
line applies to Agent release branches, RCs and stable releases.

1. Determine the target Agent minor, normally the current milestone on datadog-agent `main`. Check that it maps to the
   ADP version on `main`, in `bin/agent-data-plane/Cargo.toml`: ADP 1.9.0 needs target 7.87.
2. Create `releases/1.9.x` from the chosen `main` commit. The branch inherits `main`'s `.datadog-agent-version.json`,
   which holds a nightly and no target.
3. Open a pull request into `releases/1.9.x` that selects the line's Agent. This is the release-cut commit. It adds
   `"target": "7.87"` to `.datadog-agent-version.json`, and it sets the Integration version to an Agent build from
   7.87:
   - If 7.87 has a complete RC, let the discovery tool write the whole file. It reads the Agent's 7.87 tags from
     GitHub, resolves the newest complete RC's images to digests, downloads and hashes its packages, and writes a file
     that includes the target. On the branch, run:

     ```shell
     python3 ci/tooling/update-agent-version/discover_agent_version.py release --target 7.87 \
       --base-branch releases/1.9.x --output-dir /tmp/release-cut
     cp /tmp/release-cut/datadog-agent-version.json .datadog-agent-version.json
     ```

     If no RC is complete, it writes only `/tmp/release-cut/summary.json`, which says why.
   - If 7.87 has no complete RC, see [Before the target minor has an RC](#before-the-target-minor-has-an-rc).

   Run `make check-agent-version` before you push. It fails when the Integration version is outside the target.
4. Wait for the pull request's CI to pass, then merge it. Check the target, the Integration version, and the ADP crate
   version.
5. Tag `1.9.0` on the release branch (see [Quick steps](#quick-steps)), publish from the tag, and let the **Bump ADP
   Version** workflow open the development-version bumps for `main` and the release branch.
6. Add `1.9.x` to `MAINTAINED_LINES` in `.github/workflows/update-agent-version.yml` on `main`, so the line gets newer
   7.87 RCs and stable releases.

### Before the target minor has an RC

The branch inherits `main`'s nightly. If that nightly is from the target minor, such as
`7.87.0-devel+git.394.462b537.pipeline.151347634` for target 7.87, keep it and add only the `target` field by hand:

```json
{
  "schema_version": 1,
  "target": "7.87",
  "integration": { "kind": "nightly", "version": "7.87.0-devel+git.394.462b537.pipeline.151347634", ... }
}
```

If Agent `main` has already moved to the next minor, the inherited nightly is outside the target. Take the last
nightly from the target minor out of `main`'s history of the file, then add `target`:

```shell
git log --oneline origin/main -- .datadog-agent-version.json
git show <commit>:.datadog-agent-version.json > .datadog-agent-version.json
```

If `main` never had a nightly from the target minor, wait for a complete RC, or wait to cut. Agent release-branch
nightlies don't publish the packages the file needs. Don't borrow an RC from another minor, and don't change the target
to fit the available artifacts.

## Patch releases

Tag `X.Y.Z` on `releases/X.Y.x`. The release checks are the same as for a new line, and the **Bump ADP Version**
workflow bumps the release branch to the next patch development version.

## Agent updates within a release line

The **Update Agent Release Version** workflow proposes newer Agent releases for each maintained line that has adopted
`.datadog-agent-version.json`. It changes only the file's `integration`, never `target`, version gates, or code, and the
branch's CI tests the new Agent. It:

- accepts only exact `X.Y.Z` and `X.Y.Z-rc.N` Agent tags within the target minor
- lets the first complete RC replace the inherited nightly
- moves through RCs, then the stable release, then stable patches, and never backwards
- stays on stable patches once the line reaches stable, without proposing patch RCs
- keeps the newest complete version when a newer one is missing artifacts

The workflow skips maintained lines that haven't adopted the file. Those lines still count when you decide whether a
version gate can be removed.

## Release checks

These checks guard a release. A tag-triggered workflow can't undo a tag, so branch CI and this page's steps prevent a
bad tag, and the GitLab check prevents a bad tag from shipping:

- the **Agent version** pull request check keeps each release branch's file valid and its target unchanged
- the **Bump ADP Version** workflow checks that an `X.Y.0` tag is on `releases/X.Y.x` and declares the mapped target,
  before it opens the version bumps and creates the backport label
- the `check-release-agent-version` job in the tag pipeline checks that the tag is on its release branch, that the ADP
  version matches the tag, and that the file declares the mapped target with an Integration version inside it. Every
  publish job depends on it.
- the **Release notes** workflow checks that an `X.Y.0` tag is on its release branch before it renders the notes

## Release notes for a new line

Reno, the release-note tool, decides which release a note belongs to by walking back through the Git history of the
tag being released. A note belongs to the first release tag after the commit that added it. Notes added before an
earlier release tag in that history belong to that earlier release, so Reno leaves them out.

Each line is cut to `releases/X.Y.x` before its first tag, and tagged there, so no `X.Y.0` tag is in `main`'s history.
In this example, `T1` and `T2` are the release-cut commits:

```text
main:              A --- B --- C --- D
                          \           \
releases/1.9.x:            T1 (1.9.0)  \
releases/1.10.x:                        T2 (1.10.0)
```

The notes added in `A` and `B` shipped in `1.9.0`. When Reno walks back from `1.10.0`, through `T2`, `D`, `C`, `B` and
`A`, it never meets the `1.9.0` tag, so on its own it would list those notes again under `1.10.0`.

`ci/tooling/release_notes.py render` corrects for that. When the newest earlier release, `1.9.0`, isn't in the tag's
history, it takes the notes present at `B`, where the two histories meet, and drops the ones Reno already gives to a
release in the tag's history. It passes the rest to Reno as `ignore_notes`, in a temporary copy of the configuration;
`releasenotes/config.yaml` doesn't change. Patch releases, and older lines that were tagged on `main`, have their
newest earlier release in their history and render as before. A change merged to `main` after the cut and backported
to `1.9.x` isn't in `B`, so its note appears in both `1.9.x` and `1.10.0`.

## Older release lines

Release lines cut before `.datadog-agent-version.json` existed keep their existing process until someone migrates them:
their checks skip lines without the file, and published tags are never rewritten. Migrating a line means auditing its
intended Agent minor and its version gates, then adding the file with its `target` in a reviewed pull request. Once a
line has the file, removing the file or `target` doesn't exempt it from the checks.

## Quick steps

- Ensure that the release branch is up-to-date with all of the intended changes that should be present, and that CI
  tests are passing cleanly (this will get checked in the actual release CI pipeline, but easier to catch problems
  early). For a new release line, cut the branch and commit its target first (see
  [Cutting a new release line](#cutting-a-new-release-line)).
- Go to the [Releases](https://github.com/DataDog/saluki/releases) page on GitHub and click the `Draft a new release`
  button.
- Fill in the appropriate Git tag (see ["Determining the version"](#determining-the-version) below), set the target
  to the `releases/X.Y.x` branch, and click `Create new tag ... on publish`.
- Leave `Previous tag` at `auto`, and change `Release title` to `Agent Data Plane <version>`, where `<version>` is the
  Git tag being created. (for example, `Agent Data Plane 0.2.0`)
- Click the `Generate release notes` to automatically populate the relevant changes since the last release.
- If there are any additional notes that should be included, add a new section, called `Additional Notes`, at the top of
  the release notes. (above the auto-generated `What's Changed` section)
- Click `Publish release`.
- Confirm that the GitHub `Release notes` workflow for the tag succeeds. It adds curated release notes above GitHub's
  generated `What's Changed` pull request list. Do this before publishing release artifacts or announcing the release.
  The workflow cannot repair releases whose tags predate `releasenotes/config.yaml`; these releases do not receive
  curated notes for prior releases.
- Go to the [GitLab CI pipelines dashboard](https://gitlab.ddbuild.io/DataDog/saluki/-/pipelines) for the repository and
  find the pipeline that was triggered for the newly created Git tag. It may take a minute or two for the repository
  sync and pipeline creation to occur.
- The pipeline should progress through the `test`, `build`, `correctness`, and `release` stages without issue,
  including the `check-release-agent-version` job.
- Once all stages have completed, the `release` stage will have a number of blocked jobs: standalone image publishing,
  release tarball publishing, and so on. These jobs perform any necessary final packaging and publish ADP release
  artifacts, in their various forms, to the relevant container registries and object storage. They _must_ be triggered
  manually.
- Once triggered, they should run to completion.
- Once complete, you should be able to navigate to a public container image registry that we use, such as [Docker
  Hub](https://hub.docker.com/r/datadog/agent-data-plane/tags), and find the resulting images: if the tag was `0.2.0`,
  you should be able to see a recently published image with a tag that looks like `0.2.0` and `0.2.0-fips`.
- Post a release announcement in the `#agent-data-plane` Slack channel
  in the following format:
  ```
  Agent Data Plane vX.Y.Z: <link to GitHub release>
  CI pipeline: <link to GitLab CI pipeline for the git tag>
  ```

## Determining the version

> [!NOTE]
> Git tags **must** be in the format of `X.Y.Z`.

For ADP, we use the Git tag to drive the versioning of the release artifacts themselves, but the tag is expected to match the
version of the `agent-data-plane` crate. This is due to how we use the version of the `agent-data-plane` crate itself for
embedding static metadata in the ADP binaries. The version of the ADP binary can be found in the
`bin/agent-data-plane/Cargo.toml` file, under the `version` field.

### Determining the _next_ version

Normally, a CI job will run to bump the patch release of ADP after the Git tag is generated for the current release. In
some cases, we may need to bump the minor or major version due to major/breaking changes. When deciding what minor/major
version to bump to, we follow the [Semantic Versioning](https://semver.org/) specification. This means, in a nutshell:

- prior to v1.0.0:
    - breaking changes are indicated by incrementing the minor version
    - new features and bug fixes are generally indicated by incrementing the patch version
    - if there are enough new features/bug fixes, we may instead opt to increment the minor version just for simplicity
- after v1.0.0:
    - breaking changes are indicated by incrementing the major version (this should generally never happen after v1)
    - new features are indicated by incrementing the minor version
    - bug fixes are indicated by incrementing the patch version

## Build metadata

We calculate a number of values that are used to populate what we call _build metadata_, which is information passed in
during the build process and is used to drive a number of behaviors:

- outputting the version of ADP, when it was built, the build architecture, etc, as a log at startup
- special constant identifiers that are used to populate things like HTTP request headers (user agent, etc)

This build metadata is calculated with a Make target—`emit-adp-build-metadata`—which populates it during local builds or
regular CI builds. The relevant build arguments are all prefixed with `APP_` and are as follows:

- `APP_GIT_HASH`: the Git commit the build was performed from (set to the GitLab CI commit SHA)
- `APP_BUILD_TIME`: the time the build was performed (set to the creation time of the GitLab CI pipeline)
- `APP_DEV_BUILD`: whether this is a development build (set to `false` only for tag pipelines)

Note that `APP_DEV_BUILD` also acts as a safety check: when it is `false`, `saluki-metadata`'s build script fails the
build if any of the values above are missing or still hold their placeholder defaults. This keeps a release binary from
silently shipping metadata that reports `unknown`.

This build metadata shouldn't need to be manually changed on a per-release basis, and so this section is mostly
informational and not relevant to the release process itself.

## Software bill of materials (SBOM)

Every ADP binary produced in CI -- both the internal/dev images built on each pipeline and the published release
artifacts -- embeds a software bill of materials (SBOM): a compact JSON record of the exact dependency tree the binary was
built from. This lets you audit a shipped binary for vulnerable dependencies without access to its source tree or
`Cargo.lock`.

The SBOM is produced by [`cargo-auditable`](https://github.com/rust-secure-code/cargo-auditable), used during CI to
build ADP in a way that captures the exact dependencies used, both the name and version. The dependency list lives in a
dedicated linker section of the binary, adds only a few kilobytes, survives stripping, and is reproducible-build-safe
(no timestamps; sorted output). It is also natively supported by tools like `cargo-audit` and Trivy, making it possible
for customers to audit ADP binaries for vulnerable dependencies without access to the source code or bespoke tooling.

> [!NOTE]
> Local developer builds (`make build-adp`, `make build-adp-host`, `make build-adp-image`) do **not** embed an SBOM. The
> wrapper is enabled only in CI, where GitLab sets `CI=true` (host builds) or the build passes `USE_CARGO_AUDITABLE=true`
> (Linux Docker builds). The SBOM only matters for artifacts we ship, so local builds stay on plain `cargo build`.

To inspect the SBOM embedded in a binary, use `rust-audit-info` or `cargo-audit`'s `bin` subcommand:

```shell
# Print the embedded dependency tree as JSON.
rust-audit-info /usr/local/bin/agent-data-plane

# Or scan it directly against the RustSec advisory database.
cargo audit bin /usr/local/bin/agent-data-plane
```

The `cargo-auditable` version is pinned in the `Makefile` (the `CARGO_TOOL_VERSION_cargo-auditable` variable), which the
host and Linux Docker builds share. The Windows build pins the same version and its archive checksum separately in
`ci/tooling/windows-build-adp.ps1`; bump both together.
