## Core crate review checklist

Apply these when the pull request touches `lib/saluki-core`, which holds the data model, the component and topology
traits, and the runtime that every other crate builds on.

- The event data model under `src/data_model/event/` is Saluki's own representation of events. Flag fields or methods
  on event types that take or return protocol-specific types, such as OTLP or wire-format protobuf types. Translation
  between wire formats and the data model belongs in sources, decoders, and encoders. Recording where an event came
  from, for example an `Otlp` variant in an origin enum, is fine. Payload types under `src/data_model/payload/` are
  transport-level by design and may carry protocol types such as `http::Request`.
- Flag data model accessors that clone heavy values when a reference would do. Callers that need ownership should get
  an `into_parts(self)`-style method or a mutable accessor instead.
- Changes to public traits and types here affect every component. Flag breaking changes that the pull request doesn't
  call out, and flag new public API that has only one caller and could live with that caller instead.
