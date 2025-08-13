//

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use std::sync::Arc;

use crate::xcommons::error::Result as AppResult;
use crate::app::env::AppEnvironment;

#[async_trait]
pub trait Strategy: Send + Sync {
    type Config: DeserializeOwned + Send + Sync + 'static;

    fn name(&self) -> &'static str;

    async fn start(&self, ctx: StrategyContext, cfg: Self::Config) -> AppResult<()>;

    async fn stop(&self) -> AppResult<()>;
}

#[derive(Clone)]
pub struct StrategyContext {
    pub env: Arc<dyn AppEnvironment>,
}

pub trait StrategyFactory: Send + Sync {
    fn name(&self) -> &'static str;
    fn create(&self) -> Box<dyn ErasedStrategy>;
}

/// Object-safe erased strategy to allow heterogeneous registry.
#[async_trait]
pub trait ErasedStrategy: Send + Sync {
    fn name(&self) -> &'static str;
}

pub struct StrategyRegistry {
    factories: Vec<Box<dyn StrategyFactory>>,
}

impl StrategyRegistry {
    pub fn new() -> Self { Self { factories: Vec::new() } }
    pub fn register(mut self, factory: Box<dyn StrategyFactory>) -> Self { self.factories.push(factory); self }
    pub fn get(&self, name: &str) -> Option<&Box<dyn StrategyFactory>> { self.factories.iter().find(|f| f.name() == name) }
}


