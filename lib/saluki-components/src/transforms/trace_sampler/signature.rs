//! Trace signature computation utilities.
//!
//! This module currently provides:
//! - a small FNV-1a 32-bit helper (used by probabilistic sampling)
//! - a signature newtype + compute helper (for score/TPS samplers)

use saluki_core::data_model::event::trace::{Span, Trace};

use crate::common::datadog::get_trace_env;

const OFFSET_32: u32 = 2166136261;
const PRIME_32: u32 = 16777619;
const KEY_HTTP_STATUS_CODE: &str = "http.status_code";
const KEY_ERROR_TYPE: &str = "error.type";

fn write_hash(mut hash: u32, bytes: &[u8]) -> u32 {
    for &b in bytes {
        hash ^= b as u32;
        hash = hash.wrapping_mul(PRIME_32);
    }
    hash
}

pub(super) fn fnv1a_32(seed: &[u8], bytes: &[u8]) -> u32 {
    let hash = write_hash(OFFSET_32, seed);
    write_hash(hash, bytes)
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub(super) struct Signature(pub(super) u64);

/// Service identifier for sampling rate lookups.
///
/// Represents a unique (service name, environment) pair used as a key
/// for storing and retrieving sampling rates in distributed sampling.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub(super) struct ServiceSignature {
    pub name: String,
    pub env: String,
}

impl ServiceSignature {
    /// Creates a new ServiceSignature from name and environment.
    pub fn new(name: impl Into<String>, env: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            env: env.into(),
        }
    }

    /// Computes FNV-1a hash matching Go's ServiceSignature.Hash().
    ///
    /// The hash is computed over: `name + "," + env`
    pub fn hash(&self) -> Signature {
        let mut h = OFFSET_32;
        h = write_hash(h, self.name.as_bytes());
        h = write_hash(h, b",");
        h = write_hash(h, self.env.as_bytes());
        Signature(h as u64)
    }
}

impl std::fmt::Display for ServiceSignature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "service:{},env:{}", self.name, self.env)
    }
}

pub(super) fn compute_signature_with_root_and_env(trace: &Trace, root_idx: usize) -> Signature {
    // Mirrors datadog-agent/pkg/trace/sampler/signature.go:computeSignatureWithRootAndEnv.
    //
    // Signature based on the hash of (env, service, name, resource, is_error) for the root, plus the set of
    // (env, service, name, is_error) of each span.
    let spans = trace.spans();
    let Some(root) = spans.get(root_idx) else {
        return Signature(0);
    };

    let env = get_trace_env(trace, root_idx).map(|v| v.as_ref()).unwrap_or("");
    let root_hash = compute_span_hash(root, env, true);
    let mut span_hashes: Vec<u32> = spans.iter().map(|span| compute_span_hash(span, env, false)).collect();

    if span_hashes.is_empty() {
        return Signature(root_hash as u64);
    }

    // Sort, dedupe then merge all the hashes to build the signature.
    span_hashes.sort_unstable();
    span_hashes.dedup();

    let mut trace_hash = span_hashes[0] ^ root_hash;
    for &h in span_hashes.iter().skip(1) {
        trace_hash ^= h;
    }

    Signature(trace_hash as u64)
}

fn compute_span_hash(span: &Span, env: &str, with_resource: bool) -> u32 {
    let mut h = OFFSET_32;
    h = write_hash(h, env.as_bytes());
    h = write_hash(h, span.service().as_bytes());
    h = write_hash(h, span.name().as_bytes());
    h = write_hash(h, &[span.error() as u8]);
    if with_resource {
        h = write_hash(h, span.resource().as_bytes());
    }
    if let Some(code) = span.meta().get(KEY_HTTP_STATUS_CODE) {
        h = write_hash(h, code.as_ref().as_bytes());
    }
    if let Some(typ) = span.meta().get(KEY_ERROR_TYPE) {
        h = write_hash(h, typ.as_ref().as_bytes());
    }
    h
}
