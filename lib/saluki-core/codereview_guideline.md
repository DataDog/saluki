## Core crate review checklist

Apply these when the pull request touches `lib/saluki-core`, which holds the data model, the component and topology
traits, and the runtime that every other crate builds on.

- The data model under `src/data_model/` is Saluki's own representation of events. Flag fields or methods on data
  model types that take or return protocol-specific types, such as OTLP or Datadog protobuf types. Translation
  between wire formats and the data model belongs in sources, decoders, and encoders. Recording where an event came
  from, for example an `Otlp` variant in an origin enum, is fine.
- Flag data model accessors that clone heavy values when a reference would do. Callers that need ownership should get
  an `into_parts(self)`-style method or a mutable accessor instead.
- Fixed strings in the data model should use `MetaString::from_static` rather than allocating.
- Changes to public traits and types here affect every component. Flag breaking changes that the pull request doesn't
  call out, and flag new public API that has only one caller and could live with that caller instead.
