use super::sink::Sink;
use super::{Mapping, Result};

pub type InitCfg = Mapping;
pub type InstanceCfg = Mapping;

use async_trait::async_trait;

#[async_trait]
pub trait Check: Send + Sync {
    type Snk: Sink + Send + Sync;

    // TODO async?
    fn build(sink: Self::Snk, init_cfg: InitCfg, instance_cfg: InstanceCfg) -> Result<Self>
    where
        Self: Sized;

    async fn run(&self) -> Result<()>;
}
