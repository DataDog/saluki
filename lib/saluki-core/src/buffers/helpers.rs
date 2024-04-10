#[macro_export]
macro_rules! buffered {
	(struct $name:ident {
		$($field_name:ident: $field_type:ty,)*
	}$(,)?
	clear => $clear:expr) => {
		paste::paste! {
			#[derive(Default)]
			pub struct [<$name Inner>] {
				$($field_name: $field_type,)*
			}

			impl $crate::buffers::Clearable for [<$name Inner>] {
				fn clear(&mut self) {
					let clear_fn: &dyn Fn(&mut Self) = &$clear;
					clear_fn(self)
				}
			}

			pub struct $name {
				strategy_ref: ::std::sync::Arc<dyn $crate::buffers::Strategy<[<$name Inner>]> + Send + Sync>,
				data: ::std::mem::ManuallyDrop<[<$name Inner>]>,
			}

			impl $name {
				pub fn data(&self) -> &[<$name Inner>] {
					&self.data
				}

				pub fn data_mut(&mut self) -> &mut [<$name Inner>] {
					&mut self.data
				}
			}

			impl $crate::buffers::Bufferable for $name {
				type Data = [<$name Inner>];

				fn from_data(strategy_ref: ::std::sync::Arc<dyn $crate::buffers::Strategy<Self::Data> + Send + Sync>, data: Self::Data) -> Self {
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

#[macro_export]
macro_rules! buffered_newtype {
    (outer => $name:ident, inner => $inner_ty:ty, clear => $clear:expr) => {
        pub struct $name {
            strategy_ref: ::std::sync::Arc<dyn $crate::buffers::Strategy<$inner_ty> + Send + Sync>,
            data: ::std::mem::ManuallyDrop<$inner_ty>,
        }

        impl $name {
            fn data(&self) -> &$inner_ty {
                &self.data
            }

            fn data_mut(&mut self) -> &mut $inner_ty {
                &mut self.data
            }
        }

        impl $crate::buffers::Bufferable for $name {
            type Data = $inner_ty;

            fn from_data(
                strategy_ref: ::std::sync::Arc<dyn $crate::buffers::Strategy<Self::Data> + Send + Sync>,
                data: Self::Data,
            ) -> Self {
                Self {
                    strategy_ref,
                    data: ::std::mem::ManuallyDrop::new(data),
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
    };
}

pub use buffered;
pub use buffered_newtype;
