//! A library to read binary protobuf files
//!
//! This reader is developed similarly to a pull reader

#![deny(missing_docs)]
#![allow(dead_code)]

pub mod errors;
pub mod message;
pub mod reader;
pub mod sizeofs;
pub mod writer;
pub mod builder;

pub use crate::errors::{Error, Result};
pub use crate::message::{MessageInfo, MessageRead, MessageWrite};
pub use crate::reader::{
    deserialize_from_slice, BytesReader, PackedFixed, PackedFixedIntoIter, PackedFixedRefIter,
};
pub use crate::writer::{serialize_into_slice, BytesWriter, Writer, WriterBackend};

pub use crate::builder::GenericMapBuilder;
pub use crate::reader::Reader;
pub use crate::writer::serialize_into_vec;
