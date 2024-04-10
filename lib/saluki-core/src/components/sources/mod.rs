use async_trait::async_trait;

mod builder;
mod context;

pub use self::builder::SourceBuilder;
pub use self::context::SourceContext;

#[async_trait]
pub trait Source {
    async fn run(self: Box<Self>, context: SourceContext) -> Result<(), ()>;
}
