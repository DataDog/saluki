//! Datadog tracer payload data model backed by `MetaString`.
//!
//! Provides Rust-native representations of `TraceChunk` and `TracerPayload` from the Datadog trace protobuf
//! definitions, using `MetaString` for efficient string storage and `FastHashMap` for collections.

use saluki_common::collections::FastHashMap;
use stringtheory::MetaString;

use super::trace::Span;

/// A list of spans with the same trace ID (a chunk of a trace).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TraceChunk {
    /// Sampling priority of the trace.
    pub priority: i32,
    /// Origin product ("lambda", "rum", etc.) of the trace.
    pub origin: MetaString,
    /// List of containing spans.
    pub spans: Vec<Span>,
    /// Tags common to all spans in this chunk.
    pub tags: FastHashMap<MetaString, MetaString>,
    /// Whether the trace was dropped by samplers or not.
    pub dropped_trace: bool,
}

impl TraceChunk {
    /// Creates a new trace chunk for the provided spans.
    pub fn new(spans: Vec<Span>) -> Self {
        Self {
            spans,
            ..Self::default()
        }
    }

    /// Sets the sampling priority flag.
    pub fn with_priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }

    /// Sets the trace origin tag.
    pub fn with_origin(mut self, origin: impl Into<MetaString>) -> Self {
        self.origin = origin.into();
        self
    }

    /// Replaces the spans contained by the chunk.
    pub fn with_spans(mut self, spans: Vec<Span>) -> Self {
        self.spans = spans;
        self
    }

    /// Replaces the chunk tags.
    pub fn with_tags(mut self, tags: impl Into<Option<FastHashMap<MetaString, MetaString>>>) -> Self {
        self.tags = tags.into().unwrap_or_default();
        self
    }

    /// Sets whether the tracer dropped the trace.
    pub fn with_dropped_trace(mut self, dropped: bool) -> Self {
        self.dropped_trace = dropped;
        self
    }

    /// Returns the stored sampling priority.
    pub fn priority(&self) -> i32 {
        self.priority
    }

    /// Returns the origin tag.
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// Returns the spans contained in the chunk.
    pub fn spans(&self) -> &[Span] {
        &self.spans
    }

    /// Returns the spans mutably.
    pub fn spans_mut(&mut self) -> &mut Vec<Span> {
        &mut self.spans
    }

    /// Returns the chunk tags.
    pub fn tags(&self) -> &FastHashMap<MetaString, MetaString> {
        &self.tags
    }

    /// Returns the chunk tags mutably.
    pub fn tags_mut(&mut self) -> &mut FastHashMap<MetaString, MetaString> {
        &mut self.tags
    }

    /// Returns whether the chunk indicates a dropped trace.
    pub fn dropped_trace(&self) -> bool {
        self.dropped_trace
    }
}

/// A payload the trace agent receives from tracers.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TracerPayload {
    /// ID of the container where the tracer is running on.
    pub container_id: MetaString,
    /// Language of the tracer.
    pub language_name: MetaString,
    /// Language version of the tracer.
    pub language_version: MetaString,
    /// Version of the tracer.
    pub tracer_version: MetaString,
    /// V4 UUID representation of a tracer session.
    pub runtime_id: MetaString,
    /// List of containing trace chunks.
    pub chunks: Vec<TraceChunk>,
    /// Tags common to all chunks.
    pub tags: FastHashMap<MetaString, MetaString>,
    /// `env` tag set with the tracer.
    pub env: MetaString,
    /// Hostname of where the tracer is running.
    pub hostname: MetaString,
    /// `version` tag set with the tracer (application version).
    pub app_version: MetaString,
}

impl TracerPayload {
    /// Creates an empty tracer payload.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the container identifier.
    pub fn with_container_id(mut self, container_id: impl Into<MetaString>) -> Self {
        self.container_id = container_id.into();
        self
    }

    /// Sets the language name reported by the tracer.
    pub fn with_language_name(mut self, language_name: impl Into<MetaString>) -> Self {
        self.language_name = language_name.into();
        self
    }

    /// Sets the language version.
    pub fn with_language_version(mut self, language_version: impl Into<MetaString>) -> Self {
        self.language_version = language_version.into();
        self
    }

    /// Sets the tracer version string.
    pub fn with_tracer_version(mut self, tracer_version: impl Into<MetaString>) -> Self {
        self.tracer_version = tracer_version.into();
        self
    }

    /// Sets the runtime identifier.
    pub fn with_runtime_id(mut self, runtime_id: impl Into<MetaString>) -> Self {
        self.runtime_id = runtime_id.into();
        self
    }

    /// Replaces the payload trace chunks.
    pub fn with_chunks(mut self, chunks: Vec<TraceChunk>) -> Self {
        self.chunks = chunks;
        self
    }

    /// Replaces the payload tags.
    pub fn with_tags(mut self, tags: impl Into<Option<FastHashMap<MetaString, MetaString>>>) -> Self {
        self.tags = tags.into().unwrap_or_default();
        self
    }

    /// Sets the environment reported by the tracer.
    pub fn with_env(mut self, env: impl Into<MetaString>) -> Self {
        self.env = env.into();
        self
    }

    /// Sets the hostname reported by the tracer.
    pub fn with_hostname(mut self, hostname: impl Into<MetaString>) -> Self {
        self.hostname = hostname.into();
        self
    }

    /// Sets the application version tag.
    pub fn with_app_version(mut self, app_version: impl Into<MetaString>) -> Self {
        self.app_version = app_version.into();
        self
    }

    /// Returns the container identifier.
    pub fn container_id(&self) -> &str {
        &self.container_id
    }

    /// Returns the language name.
    pub fn language_name(&self) -> &str {
        &self.language_name
    }

    /// Returns the language version.
    pub fn language_version(&self) -> &str {
        &self.language_version
    }

    /// Returns the tracer version string.
    pub fn tracer_version(&self) -> &str {
        &self.tracer_version
    }

    /// Returns the runtime identifier.
    pub fn runtime_id(&self) -> &str {
        &self.runtime_id
    }

    /// Returns the payload chunks.
    pub fn chunks(&self) -> &[TraceChunk] {
        &self.chunks
    }

    /// Returns the payload chunks mutably.
    pub fn chunks_mut(&mut self) -> &mut Vec<TraceChunk> {
        &mut self.chunks
    }

    /// Returns the payload tags.
    pub fn tags(&self) -> &FastHashMap<MetaString, MetaString> {
        &self.tags
    }

    /// Returns the payload tags mutably.
    pub fn tags_mut(&mut self) -> &mut FastHashMap<MetaString, MetaString> {
        &mut self.tags
    }

    /// Returns the environment string.
    pub fn env(&self) -> &str {
        &self.env
    }

    /// Returns the hostname.
    pub fn hostname(&self) -> &str {
        &self.hostname
    }

    /// Returns the application version.
    pub fn app_version(&self) -> &str {
        &self.app_version
    }
}
