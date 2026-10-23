## Configuration system review checklist

Apply these when the pull request touches the Datadog configuration source model.

- Support for Datadog schema keys is classified in `schema/schema_overlay.yaml`. Flag changes that alter which keys
  are supported without updating the overlay, and flag code generated from the schema and overlay that was edited by
  hand instead of regenerated with `make build-schema-overlay`.
- Files under `schema/core/` are a vendored upstream copy of the Datadog schema. Flag any hand edit to them.
- A modeled key arrives in the Datadog Agent's canonical shape, and the source model must read that shape. Flag new
  `#[serde(rename)]` or `#[serde(alias)]` attributes on source model fields.
- Whether a key is a Datadog key or a Saluki-only key depends on whether it appears in the schema, not on its name.
  Flag reasoning or code that infers the class of a key from its prefix, such as `data_plane.`.
