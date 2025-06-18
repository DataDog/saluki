# Releasing ADP

This page covers the overall release process for Agent Data Plane, including the steps required to create a new release,
as well as the technical details around how we determine the version of the release, what metadata is
collected/generated, and more.

## High-level overview

The release process for ADP roughly looks like this:

- create a Github release, which tags `main` at a particular point
- creation of the Git tag triggers a CI pipeline that additionally unlocks jobs to publish release artifacts
- the additional CI jobs, once manually triggered, will publish ADP container images to various public container image registries
- bump the version of ADP to the next development version

## Quick steps

- Ensure that `main` is up-to-date with all of the intended changes that should be present, and that CI tests are
  passing cleanly (this will get checked in the actual release CI pipeline, but easier to catch problems early)
- Go to the [Releases](https://github.com/DataDog/saluki/releases) page on Github and click the `Draft a new release`
  button.
- Fill in the appropriate Git tag (see ["Determining the version"](#determining-the-version) below) and click
  `Create new tag ... on publish`.
- Leave `Previous tag` at `auto`, and change `Release title` to `Agent Data Plane <version>`, where `<version>` is the
  Git tag being created. (e.g., `Agent Data Plane 0.2.0`)
- Click the `Generate release notes` to automatically populate the relevant changes since the last release.
- If there are any additional notes that should be included, add a new section, called `Additional Notes`, at the top of
  the release notes. (above the auto-generated `What's Changed` section)
- Click `Publish release`.
- Go to the [Gitlab CI pipelines dashboard](https://gitlab.ddbuild.io/DataDog/saluki/-/pipelines) for the repository and
  find the pipeline that was triggered for the newly-created Git tag. It may take a minute or two for the repository
  sync and pipeline creation to occur.
- The pipeline should progress through the `test`, `build`, `correctness`, and `release` stages without
  issue.
- Once all stages have completed, the `release` stage will have two blocked jobs: `publish-standalone-adp-image-linux` and
  `publish-standalone-adp-image-linux-fips`. These jobs perform the actual publishing of the container images to our public
  container image registries, and must be manually triggered.
- Once triggered, they should run to completion.
- Once complete, you should be able to navigate to a public container image registry that we use, such as
  [Docker Hub](https://hub.docker.com/r/datadog/agent-data-plane/tags), and find the resulting images: if the tag was
  `0.2.0`, you should be able to see a recently-published image with a tag that looks like `0.2.0` and `0.2.0-fips`.
- Finally, create a PR that bumps the version of ADP to the next development version. Again, see
  ["Determining the version"](#determining-the-version) below for information on how we version ADP.

## Determining the version

> [!NOTE]
> Git tags **must** be in the format of `X.Y.Z`.

For ADP, we use the Git tag to drive the versioning of the release artifacts themselves, but the tag is expected to match the
version of the `agent-data-plane` crate. This is due to how we use the version of the `agent-data-plane` crate itself for
embedding static metadata in the ADP binaries. The version of the ADP binary can be found in the
`bin/agent-data-plane/Cargo.toml` file, under the `version` field.

In some cases, we may need to bump the version of ADP _prior_ to release if there are any breaking changes that will be included.
See the subsection below on what constitutes a breaking change. In many cases, it will be sufficient to simply use the current
version of the `agent-data-plane` crate.

### Determining the _next_ version

In some cases, there be looking to release a new version with breaking changes, or we may be bumping ADP to the next development
version and know that we will be introducing breaking changes. When deciding what version to bump to, we follow the
[Semantic Versioning](https://semver.org/) specification. This means, in a nutshell:

- prior to v1.0.0:
    - breaking changes are indicated by incrementing the minor version
    - new features and bug fixes are generally indicated by incrementing the patch version
    - if there are enough new features/bug fixes, we may instead opt to increment the minor version just for simplicity
- after v1.0.0:
    - breaking changes are indicated by incrementing the major version (this should generally never happen after v1)
    - new features are indicated by incrementing the minor version
    - bug fixes are indicated by incrementing the patch version

## Build metadata

We calculate a number of values that are used to populate what we call "build metadata", which is information passed in
during the build process and is used to drive a number of behaviors:

- outputting the version of ADP, when it was built, the build architecture, etc, as a log at startup
- special constant identifiers that are used to populate things like HTTP request headers (user agent, etc)

This build metadata is calculated with a Make target — `emit-build-metadata` — which populates it during local builds or
regular CI builds. The relevant build arguments are all prefixed with `APP_` and are as follows:

- `APP_FULL_NAME`: the full name of the application (hard-coded to `agent-data-plane`)
- `APP_SHORT_NAME`: the short name of the application (hard-coded to `data-plane`)
- `APP_IDENTIFIER`: a short identifier for the application (hard-coded to `adp`)
- `APP_VERSION`: the version of the application (set to `version` field in `bin/agent-data-plane/Cargo.toml`)
- `APP_BUILD_DATE`: the date the build was performed (set to the creation time of the Gitlab CI pipeline)

This build metadata should not need to be manually changed on a per-release basis, and so this section is mostly
informational and not relevant to the release process itself.
