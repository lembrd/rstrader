use serde::Deserialize;

use crate::xcommons::types::{ExchangeId as Exchange, StreamType};

#[derive(Debug, Clone, Deserialize)]
pub struct MdCollectorConfig {
    pub runtime: RuntimeSection,
    pub sink: SinkSection,
    pub subscriptions: Vec<SubscriptionItem>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RuntimeSection {
    #[serde(default = "default_channel_capacity")] 
    pub channel_capacity: usize,
}

fn default_channel_capacity() -> usize { 10_000 }

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SinkSection { Parquet { output_dir: std::path::PathBuf }, Questdb }

#[derive(Debug, Clone, Deserialize)]
pub struct SubscriptionItem {
    pub exchange: Exchange,
    pub stream_type: StreamType,
    pub instrument: String,
    #[serde(default = "default_arb_streams_num")] 
    pub arb_streams_num: usize,
}

fn default_arb_streams_num() -> usize { 1 }


