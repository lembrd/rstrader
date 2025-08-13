use async_trait::async_trait;

use crate::xcommons::error::Result as AppResult;
use crate::strats::api::{Strategy, StrategyContext, StrategyRegistrar};

use super::config::MdCollectorConfig;

pub struct MdCollector;

#[async_trait]
impl Strategy for MdCollector {
    type Config = MdCollectorConfig;

    fn name(&self) -> &'static str { "md_collector" }

    async fn configure(&mut self, _reg: &mut StrategyRegistrar, _ctx: &StrategyContext, _cfg: &Self::Config) -> AppResult<()> {
        // No-op: MD collector is orchestrated by runtime path today
        Ok(())
    }
}



