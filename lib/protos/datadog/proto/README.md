# Protocol Buffers Definitions

This crate/directory contains Protocol Buffers definitions for interacting with the Datadog Agent and public Datadog APIs,
along with transitive dependencies on third-party definitions.

All definitions are sourced from official, upstream repositories. Some of them are able to be updated from their upstream repositories
by specifying the target version through corresponding `PROTOBUF_SRC_REPO_*` variables in `Makefile`, followed by running `make update-protos`.
Some other definitions have been manually vendored, such as in the case of definitions where they are no longer updated or have been deprecated.

Check the `README.md` file in the various subdirectories for more information on each set of definitions.
