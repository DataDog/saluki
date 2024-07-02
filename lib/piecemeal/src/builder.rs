
#![allow(missing_docs)]

use std::marker::PhantomData;

use crate::{sizeofs::*, Writer, WriterBackend};

pub struct GenericMapBuilder<'write, W, K, V>
where 
	W: WriterBackend,
	K: MapScalar + ?Sized,
	V: MapScalar + ?Sized,
{
	field_tag: u32,
	writer: &'write mut Writer<W>,
	_key_type: PhantomData<K>,
	_value_type: PhantomData<V>,
}

impl<'write, W, K, V> GenericMapBuilder<'write, W, K, V>
where
	W: WriterBackend,
	K: MapScalar + ?Sized,
	V: MapScalar + ?Sized,
{
	pub fn new(field_tag: u32, writer: &'write mut Writer<W>) -> Self {
		Self {
			field_tag,
			writer,
			_key_type: PhantomData,
			_value_type: PhantomData,
		}
	}

	pub fn write_entry<K2, V2>(&mut self, key: K2, value: V2) -> crate::Result<()>
	where
		K2: AsRef<K>,
		V2: AsRef<V>,
	{
		self.writer.write_tag(self.field_tag)?;

		let kv_len = (key.as_ref().write_size() + value.as_ref().write_size()) as u64;
		self.writer.write_varint(kv_len)?;

		key.as_ref().write_scalar(1, self.writer)?;
		value.as_ref().write_scalar(2, self.writer)
	}
}

pub trait MapScalar {
	fn write_size(&self) -> usize;
	fn write_scalar<W: WriterBackend>(&self, field_number: u32, writer: &mut Writer<W>) -> crate::Result<()>;
}

macro_rules! map_scalar_impl {
	(deref, from => $ty:ty, to => $scaled_ty:ty, $sizeof_fn:ident, $write_fn:ident) => {
		impl MapScalar for $ty {
			fn write_size(&self) -> usize {
				$sizeof_fn(<$scaled_ty>::from(*self))
			}

			fn write_scalar<W: WriterBackend>(&self, field_number: u32, writer: &mut Writer<W>) -> crate::Result<()> {
				writer.write_with_tag(get_varint_field_tag(field_number), |w| {
					w.$write_fn(<$scaled_ty>::from(*self))
				})
			}
		}
	};

	(deref, from => $ty:ty, $sizeof_fn:ident, $write_fn:ident) => {
		impl MapScalar for $ty {
			fn write_size(&self) -> usize {
				$sizeof_fn(*self)
			}

			fn write_scalar<W: WriterBackend>(&self, field_number: u32, writer: &mut Writer<W>) -> crate::Result<()> {
				writer.write_with_tag(get_varint_field_tag(field_number), |w| {
					w.$write_fn(*self)
				})
			}
		}
	};

	(from => $ty:ty, $sizeof_fn:ident, $write_fn:ident) => {
		impl MapScalar for $ty {
			fn write_size(&self) -> usize {
				$sizeof_fn(self)
			}

			fn write_scalar<W: WriterBackend>(&self, field_number: u32, writer: &mut Writer<W>) -> crate::Result<()> {
				writer.write_with_tag(get_varint_field_tag(field_number), |w| {
					w.$write_fn(self)
				})
			}
		}
	};
}

map_scalar_impl!(deref, from => u8, to => u32, sizeof_uint32, write_uint32);
map_scalar_impl!(deref, from => u16, to => u32, sizeof_uint32, write_uint32);
map_scalar_impl!(deref, from => u32, to => u32, sizeof_uint32, write_uint32);
map_scalar_impl!(deref, from => u64, to => u64, sizeof_uint64, write_uint64);
map_scalar_impl!(deref, from => i8, to => i32, sizeof_sint32, write_sint32);
map_scalar_impl!(deref, from => i16, to => i32, sizeof_sint32, write_sint32);
map_scalar_impl!(deref, from => i32, to => i32, sizeof_sint32, write_sint32);
map_scalar_impl!(deref, from => i64, to => i64, sizeof_sint64, write_sint64);
map_scalar_impl!(deref, from => f32, sizeof_f32, write_float);
map_scalar_impl!(deref, from => f64, sizeof_f64, write_double);
map_scalar_impl!(deref, from => bool, sizeof_bool, write_bool);
map_scalar_impl!(from => str, sizeof_str, write_string);
map_scalar_impl!(from => [u8], sizeof_bytes, write_bytes);

const fn get_field_tag(field_number: u32, wire_type: u32) -> u32 {
	(field_number << 3) | wire_type
}

const fn get_varint_field_tag(field_number: u32) -> u32 {
	// VARINT has a wire type of 0
	get_field_tag(field_number, 0)
}
