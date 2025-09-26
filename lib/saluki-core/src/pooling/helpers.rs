//! Helpers for creating and working with poolable objects.

use std::{marker::PhantomData, sync::Arc};

use super::{Poolable, ReclaimStrategy};

/// Creates a struct that can be stored in an object pool, based on an inline struct definition.
///
/// In order to store a value in an object pool, the item must implement the [`Poolable`] trait. This trait, and overall
/// design of [`ObjectPool`][super::ObjectPool], dictates that a pooled data type actually holds an inner value, which
/// is the value that is actually pooled, while the outer struct is simply a wrapper around the data that ensures it is
/// returned to the object pool when no longer in use.
///
/// In practice, this means that if you wanted to create some struct that could be pooled (for example,
/// `SimpleBuffer`), you would need to create that struct and bake in all of the boilerplate logic and
/// implementations for `Poolable`/`Clearable`. Instead, `pooled!` can be used to define the desired struct inline,
/// while the wrapper type that contains the relevant pooling logic and trait implementations is generated
/// automatically.
///
/// ## Limitations
///
/// This macro is most appropriate when the desired struct is simple, such as only requiring control over the fields and
/// not the presence of any methods or trait implementations. If more control is required, consider using
/// [`pooled_newtype!`] which can wrap over an existing struct defined outside of the macro.
///
/// ## Clearing
///
/// All poolable types must provide logic for "clearing" the pooled item before it is returned to the object pool.
/// This is passed in as the `clear` parameter to the macro, and must be a closure that takes a mutable reference to
/// the inner struct.
///
/// Note that due to macro hygiene, the closure's only parameter _cannot_ be named `self`. In the usage example below, you can
/// see how this is named `this` instead to avoid conflicts.
///
/// ## Usage
///
/// ```rust
/// use saluki_core::pooling::helpers::pooled;
///
/// pooled! {
///    /// A simple Poolable struct.
///   struct SimpleBuffer {
///      value: u32,
///   }
///
///   clear => |this| this.value = 0
/// }
///
/// // This creates a new struct called `SimpleBufferInner` based on the definition of `SimpleBuffer`,
/// // and `SimpleBuffer` contains the necessary logic/pointers to be stored in an object pool.
/// //
/// // Two helper methods are provided on the wrapper struct (`SimpleBuffer`, in this case), for accessing
/// // the inner data: `data` and `data_mut`. We can see them in use below:
/// impl SimpleBuffer {
///     pub fn value(&self) -> u32 {
///         self.data().value
///     }
///
///     pub fn multiply_by_two(&mut self) {
///         self.data_mut().value *= 2;
///     }
/// }
///
/// fn use_simple_buffer(mut buf: SimpleBuffer) {
///     let original_value = buf.value();
///     buf.multiply_by_two();
///
///     let doubled_value = buf.value();
///     assert_eq!(doubled_value, original_value * 2);
/// }
/// ```
#[macro_export]
macro_rules! pooled {
    ($(#[$outer:meta])* struct $name:ident {
        $($field_name:ident: $field_type:ty,)*
    }$(,)?
    clear => $clear:expr) => {
        $crate::reexport::paste! {
            #[doc = "Inner representation of " $name "."]
            #[derive(Default)]
            pub struct [<$name Inner>] {
                $($field_name: $field_type,)*
            }

            impl $crate::pooling::Clearable for [<$name Inner>] {
                fn clear(&mut self) {
                    let clear_fn: &dyn Fn(&mut Self) = &$clear;
                    clear_fn(self)
                }
            }

            $(#[$outer])*
            pub struct $name {
                strategy_ref: ::std::sync::Arc<dyn $crate::pooling::ReclaimStrategy<$name> + Send + Sync>,
                data: ::std::mem::ManuallyDrop<[<$name Inner>]>,
            }

            impl $name {
                /// Gets a reference to the inner data.
                #[allow(dead_code)]
                pub fn data(&self) -> &[<$name Inner>] {
                    &self.data
                }

                /// Gets a mutable reference to the inner data.
                #[allow(dead_code)]
                pub fn data_mut(&mut self) -> &mut [<$name Inner>] {
                    &mut self.data
                }
            }

            impl $crate::pooling::Poolable for $name {
                type Data = [<$name Inner>];

                fn from_data(strategy_ref: ::std::sync::Arc<dyn $crate::pooling::ReclaimStrategy<Self> + Send + Sync>, data: Self::Data) -> Self {
                    Self {
                        strategy_ref,
                        data: ::std::mem::ManuallyDrop::new(data),
                    }
                }
            }
        }

        impl Drop for $name {
            fn drop(&mut self) {
                // SAFETY: We never use `self.data` again since we're already dropping `self`.
                let data = unsafe { ::std::mem::ManuallyDrop::take(&mut self.data) };
                self.strategy_ref.reclaim(data);
            }
        }
    }
}

/// Creates a struct that can be stored in an object pool, based on an existing struct definition.
///
/// In order to store a value in an object pool, the item must implement the [`Poolable`] trait. This trait, and overall
/// design of [`ObjectPool`][super::ObjectPool], dictates that a pooled data type actually holds an inner value, which
/// is the value that is actually pooled, while the outer struct is simply a wrapper around the data that ensures it is
/// returned to the object pool when no longer in use.
///
/// In many cases, the "inner value" might be either an existing type that cannot be modified, or there might be a need
/// to define that struct further, such as defining struct methods or trait implementations which would be
/// cumbersome/confusing to do on the auto-generated inner type from [`pooled!`]. In these cases, `pooled_newtype!`
/// provides the simplest possible wrapper over an existing struct definition to create a Poolable version.
///
/// Implementors are required to define their data struct, including an implementation of
/// [`Clearable`][super::Clearable], and then use `pooled_newtype!` to wrap it.
///
/// ## Usage
///
/// ```rust
/// use saluki_core::{pooled_newtype, pooling::Clearable};
///
/// pub struct PreallocatedByteBuffer {
///     data: Vec<u8>,
/// }
///
/// impl PreallocatedByteBuffer {
///     pub fn new() -> Self {
///         Self {
///             data: Vec::with_capacity(1024)
///         }
///     }
/// }
///
/// impl Clearable for PreallocatedByteBuffer {
///    fn clear(&mut self) {
///       self.data.clear();
///    }
/// }
///
/// pooled_newtype! {
///    outer => ByteBuffer,
///    inner => PreallocatedByteBuffer,
/// }
///
/// // This creates a new struct called `ByteBuffer` which simply wraps over `PreallocatedByteBuffer`. We can
/// // define some helper methods on `ByteBuffer` to make it easier to work with:
/// impl ByteBuffer {
///     pub fn len(&self) -> usize {
///         self.data().data.len()
///     }
///
///     pub fn write_buf(&mut self, buf: &[u8]) {
///         self.data_mut().data.extend_from_slice(buf);
///     }
/// }
///
/// fn use_byte_buffer(mut buf: ByteBuffer) {
///     assert_eq!(buf.len(), 0);
///
///     buf.write_buf(b"Hello, world!");
///     assert_eq!(buf.len(), 13);
/// }
/// ```
#[macro_export]
macro_rules! pooled_newtype {
    (outer => $name:ident, inner => $inner_ty:ty $(,)?) => {
        $crate::reexport::paste! {
            #[doc = "Poolable version of `" $inner_ty "`."]
            pub struct $name {
                strategy_ref: ::std::sync::Arc<dyn $crate::pooling::ReclaimStrategy<$name> + Send + Sync>,
                data: ::std::option::Option<$inner_ty>,
            }
        }

        impl $name {
            /// Gets a reference to the inner data.
            #[allow(dead_code)]
            pub fn data(&self) -> &$inner_ty {
                self.data.as_ref().unwrap()
            }

            /// Gets a mutable reference to the inner data.
            #[allow(dead_code)]
            pub fn data_mut(&mut self) -> &mut $inner_ty {
                self.data.as_mut().unwrap()
            }
        }

        impl $crate::pooling::Poolable for $name {
            type Data = $inner_ty;

            fn from_data(
                strategy_ref: ::std::sync::Arc<dyn $crate::pooling::ReclaimStrategy<Self> + Send + Sync>,
                data: Self::Data,
            ) -> Self {
                Self {
                    strategy_ref,
                    data: ::std::option::Option::Some(data),
                }
            }
        }

        impl Drop for $name {
            fn drop(&mut self) {
                // SAFETY: We never use `self.data` again since we're already dropping `self`.

                if let ::std::option::Option::Some(data) = self.data.take() {
                    self.strategy_ref.reclaim(data);
                }
            }
        }
    };
}

pub use pooled;
pub use pooled_newtype;

/// An object pool strategy that performs no pooling.
struct NoopStrategy<T> {
    _t: PhantomData<T>,
}

impl<T> NoopStrategy<T> {
    const fn new() -> Self {
        Self { _t: PhantomData }
    }
}

impl<T> ReclaimStrategy<T> for NoopStrategy<T>
where
    T: Poolable,
{
    fn reclaim(&self, _: T::Data) {}
}

/// Creates an poolable object (of type `T`) when `T::Data` implements `Default`.
#[allow(dead_code)]
pub fn get_pooled_object_via_default<T>() -> T
where
    T: Poolable + Send + Sync + 'static,
    T::Data: Default + Sync,
{
    T::from_data(Arc::new(NoopStrategy::<_>::new()), T::Data::default())
}

/// Creates an poolable object (of type `T`) when `T::Data` implements `Default`.
#[allow(dead_code)]
pub fn get_pooled_object_via_builder<F, T>(f: F) -> T
where
    F: FnOnce() -> T::Data,
    T: Poolable + Send + Sync + 'static,
    T::Data: Sync,
{
    T::from_data(Arc::new(NoopStrategy::<_>::new()), f())
}
