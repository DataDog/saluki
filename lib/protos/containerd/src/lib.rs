//! containerd Protocol Buffers definitions.
#![deny(warnings)]
#![allow(dead_code)]
#![allow(clippy::enum_variant_names)]
#![allow(clippy::doc_overindented_list_items)]
#![allow(rustdoc::invalid_html_tags)]

mod containerd_include {
    include!(concat!(env!("OUT_DIR"), "/containerd.mod.rs"));
}

pub use containerd_include::containerd::*;
