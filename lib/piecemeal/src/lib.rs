//! A library to read binary protobuf files
//!
//! This reader is developed similarly to a pull reader

#![deny(missing_docs)]
#![allow(dead_code)]

pub mod builder;
pub mod errors;
pub mod helpers;
pub mod io;
pub mod message;

pub use crate::builder::GenericMapBuilder;
pub use crate::errors::{Error, ProtoResult};
pub use crate::io::{
    reader::{deserialize_from_slice, BytesReader, PackedFixed, PackedFixedIntoIter, PackedFixedRefIter, Reader},
    scratch::{ScratchBuffer, ScratchWriter},
    writer::Writer,
};
pub use crate::message::{MessageInfo, MessageRead, MessageWrite};
