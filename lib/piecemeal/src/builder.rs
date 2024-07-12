#![allow(missing_docs)]

use std::marker::PhantomData;

use crate::{helpers::*, types::ProtobufValue, ProtoResult, ScratchBuffer, ScratchWriter, Writer};

pub struct GenericMapBuilder<'w, S, K, V>
where
    S: ScratchBuffer,
    K: MapScalar + ?Sized,
    V: MapScalar + ?Sized,
{
    field_tag: u32,
    writer: &'w mut ScratchWriter<S>,
    _key_type: PhantomData<K>,
    _value_type: PhantomData<V>,
}

impl<'w, S, K, V> GenericMapBuilder<'w, S, K, V>
where
    S: ScratchBuffer,
    K: MapScalar + ?Sized,
    V: MapScalar + ?Sized,
{
    pub fn new(field_tag: u32, writer: &'w mut ScratchWriter<S>) -> Self {
        Self {
            field_tag,
            writer,
            _key_type: PhantomData,
            _value_type: PhantomData,
        }
    }

    pub fn write_entry<K2, V2>(&mut self, key: K2, value: V2) -> ProtoResult<()>
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
    fn write_scalar<W: Writer>(&self, field_number: u32, writer: &mut W) -> ProtoResult<()>;
}

macro_rules! map_scalar_impl {
    (deref, from => $ty:ty, to => $scaled_ty:ty, $sizeof_fn:ident, $write_fn:ident) => {
        impl MapScalar for $ty {
            fn write_size(&self) -> usize {
                $sizeof_fn(<$scaled_ty>::from(*self))
            }

            fn write_scalar<W: Writer>(&self, field_number: u32, writer: &mut W) -> ProtoResult<()> {
                writer.write_with_tag(tag(field_number, WireType::Varint), |w| {
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

            fn write_scalar<W: Writer>(&self, field_number: u32, writer: &mut W) -> ProtoResult<()> {
                writer.write_with_tag(tag(field_number, WireType::Varint), |w| w.$write_fn(*self))
            }
        }
    };

    (from => $ty:ty, $sizeof_fn:ident, $write_fn:ident) => {
        impl MapScalar for $ty {
            fn write_size(&self) -> usize {
                $sizeof_fn(self)
            }

            fn write_scalar<W: Writer>(&self, field_number: u32, writer: &mut W) -> ProtoResult<()> {
                writer.write_with_tag(tag(field_number, WireType::Varint), |w| w.$write_fn(self))
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

pub struct RepeatedBuilder<'w, S, T, V: ?Sized> {
    field_number: u32,
    writer: &'w mut ScratchWriter<S>,
    _value_type: PhantomData<(T, V)>,
}

impl<'w, S, T, V> RepeatedBuilder<'w, S, T, V>
where
    S: ScratchBuffer,
    T: ProtobufValue<V>,
    V: ?Sized,
{
    pub fn new(field_number: u32, writer: &'w mut ScratchWriter<S>) -> Self {
        Self {
            field_number,
            writer,
            _value_type: PhantomData,
        }
    }

    pub fn add(&mut self, value: &V) -> ProtoResult<()> {
        self.writer.write_tag(tag(self.field_number, T::wire_type()))?;
        T::write_value(self.writer, value)
    }

    pub fn add_many_mapped<'a, I, F, R>(&mut self, values: impl IntoIterator<Item = &'a I>, map: F) -> ProtoResult<()>
    where
        I: 'a,
        F: Fn(&'a I) -> R,
        R: std::borrow::Borrow<V> + 'a,
    {
        if T::packable() {
            self.writer
                .write_tag(tag(self.field_number, WireType::LengthDelimited))?;
            self.writer.track_message(|writer| {
                for value in values {
                    let value = map(value);
                    T::write_value(writer, value.borrow())?;
                }
                Ok(())
            })
        } else {
            for value in values {
                let value = map(value);
                self.add(value.borrow())?;
            }
            Ok(())
        }
    }
}
