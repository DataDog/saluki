use async_trait::async_trait;

mod builder;
mod context;

use crate::topology::interconnect::EventBuffer;

pub use self::builder::{SynchronousTransformBuilder, TransformBuilder};
pub use self::context::TransformContext;

#[async_trait]
pub trait Transform {
    async fn run(self: Box<Self>, context: TransformContext) -> Result<(), ()>;
}

pub trait SynchronousTransform {
    fn transform_buffer(&self, buffer: &mut EventBuffer);
}
