use async_trait::async_trait;

mod builder;
mod context;

pub use self::builder::DestinationBuilder;
pub use self::context::DestinationContext;

#[async_trait]
pub trait Destination {
    async fn run(self: Box<Self>, context: DestinationContext) -> Result<(), ()>;
}
