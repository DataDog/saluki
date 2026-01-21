//! Trace signature computation utilities.
//!
//! This module currently provides:
//! - a small FNV-1a 32-bit helper (used by probabilistic sampling)
//! - a signature newtype + compute helper (for score/TPS samplers)

#![allow(dead_code)]

use saluki_core::data_model::event::trace::{Span, Trace};
use stringtheory::MetaString;

const OFFSET_32: u32 = 2166136261;
const PRIME_32: u32 = 16777619;

fn write_hash(mut hash: u32, bytes: &[u8]) -> u32 {
    for &b in bytes {
        hash ^= b as u32;
        hash = hash.wrapping_mul(PRIME_32);
    }
    hash
}

pub(super) fn fnv1a_32(seed: &[u8], bytes: &[u8]) -> u32 {
    const OFFSET_32: u32 = 0x811c_9dc5;
    let hash = write_hash(OFFSET_32, seed);
    write_hash(hash, bytes)
}

#[allow(dead_code)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub(super) struct Signature(pub(super) u64);

const KEY_HTTP_STATUS_CODE: &str = "http.status_code";
const KEY_ERROR_TYPE: &str = "error.type";

fn get_trace_env(trace: &Trace, root_span_idx: usize) -> Option<&MetaString> {
    // logic taken from here: https://github.com/DataDog/datadog-agent/blob/main/pkg/trace/traceutil/trace.go#L19-L20
    let env = trace.spans().get(root_span_idx).and_then(|span| span.meta().get("env"));
    match env {
        Some(env) => Some(env),
        None => {
            for span in trace.spans().iter() {
                if let Some(env) = span.meta().get("env") {
                    return Some(env);
                }
            }
            None
        }
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
