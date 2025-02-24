# Releasing ADP

This page covers the overall release process for Agent Data Plane, including the steps required to create a new release,
as well as the technical details around how we determine the version of the release, what metadata is
collected/generated, and more.

## High-level overview

The release process for ADP roughly looks like this:

- create a Github release, which tags `main` at a particular point
- creation of the Git tag triggers a CI pipeline that additionally unlocks jobs to publish release artifacts
- the additional CI jobs, once manually triggered, will publish ADP container images to various public container image registries

## Quick steps

- Ensure that `main` is up-to-date with all of the intended changes that should be present, and that CI tests are
  passing cleanly (this will get checked in the actual release CI pipeline, but easier to catch problems early)
- Go to the [Releases](https://github.com/DataDog/saluki/releases) page on Github and click the `Draft a new release`
  button.
- Fill in the appropriate version tag (see ["Determining the version"](#determining-the-version) below) and click `Create new tag ... on publish`.
- Leave `Previous tag` at `auto`, and change `Release title` to `Agent Data Plane <version>`, where `<version>` is the
  new version. (i.e., `Agent Data Plane 0.2.0`)
- Click the `Generate release notes` to automatically populate the relevant changes since the last release.
- If there are any additional notes that should be included, add a new section, called `Additional Notes`, at the top of
  the release notes (above the auto-generated `What's Changed` section)
- Click `Publish release`
- Go to the [Gitlab CI pipelines dashboard](https://gitlab.ddbuild.io/DataDog/saluki/-/pipelines) for the repository and
  find the pipeline that was triggered for the newly-created Git tag. It may take a minute or two for the repository
  sync and pipeline creation to occur.
- The pipeline should progress through the `test`, `build`, `correctness`, `benchmark`, and `release` stages without
  issue.
- Once all stages have completed, the `release` stage will have two blocked jobs: `publish-bundled-agent-adp-image` and
  `publish-bundled-agent-adp-image-jmx`. These jobs perform the actual publishing of the container images to our public
  container image registries, and must be manually triggered.
- Once triggered, they should run to completion.
- Once complete, you should be able to navigate to a public container image registry that we use, such as [Docker
  Hub](https://hub.docker.com/r/datadog/agent/tags), and find the resulting images: if the tag was `0.2.0`, you should
  be able to see a recently-published image with a tag that looks like `<Agent version>-v0.2.0-adp-beta-jmx`. (See
  "Determining the version" below for more information on the container image tag format.)

## Determining the version

For ADP, we determine the version of the release solely from the Git tag that is created, which also drives the CI
pipeline used to publish the resulting container images. We _do not_ use the `version` field of the `agent-data-plane`
crate itself.

> [!NOTE]
> The Git tag **must** be in the format of `vX.Y.Z`.

When deciding what version to use, we follow the [Semantic Versioning](https://semver.org/) specification. This means,
in a nutshell:

- prior to v1.0.0:
    * breaking changes are indicated by incrementing the minor version
    * new features and bug fixes are generally indicated by incrementing the patch version
    * if there are enough new features/bug fixes, we may instead opt to increment the minor version just for simplicity
- after v1.0.0:
    * breaking changes are indicated by incrementing the major version (this should generally never happen after v1)
    * new features are indicated by incrementing the minor version
    * bug fixes are indicated by incrementing the patch version

Additionally, for the container images we build, there is an additional component in the image tag that specifies the
version of the Datadog Agent that the image is based on. The Datadog Agent version is controlled in `.gitlab-ci.yml`, via the `BASE_DD_AGENT_VERSION`
variable. We pull the Datadog Agent image from a public container image registry (Google Container Registry) and layer
on the ADP binary, and supporting files, which results in our final "bundled" ADP container image.

## Build metadata

We calculate a number of values that are used to populate what we call "build metadata", which is information passed in
during the build process and is used to drive a number of behaviors:

- outputting the version of ADP, when it was built, the build architecture, etc, as a log at startup
- special constant identifiers that are used to populate things like HTTP request headers (user agent, etc)

This build metadata is _normally_ calculated with a Make target — `emit-build-metadata` — which populates it during
local builds or regular CI builds. However, for release builds, it is set manually when invoking the container image
builds. These settings can be found in the `.gitlab/release.yml` file, under the `build-release-adp-image` job.

The relevant build arguments are all prefixed with `APP_` and are as follows:

- `APP_FULL_NAME`: the full name of the application (hard-coded to `agent-data-plane`)
- `APP_SHORT_NAME`: the short name of the application (hard-coded to `data-plane`)
- `APP_IDENTIFIER`: a short identifier for the application (hard-coded to `adp`)
- `APP_VERSION`: the version of the application (set to the Git tag)
- `APP_BUILD_DATE`: the date the build was performed (set to the creation time of the Gitlab CI pipeline)

This build metadata should not need to be manually changed on a per-release basis, and so this section is mostly
informational and not relevant to the release process itself.
