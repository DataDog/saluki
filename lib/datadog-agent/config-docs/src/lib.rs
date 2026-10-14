//! Generates the Agent Data Plane configuration documentation.
//!
//! The build script renders `docs/agent-data-plane/configuration/configuration.md` from the schema overlay. The work
//! lives in a crate of its own, rather than in the configuration crate, because the document is a development and CI
//! artifact, not part of the production build.
