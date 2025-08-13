use serde::Deserialize;
use serde::de::{Deserializer, Error as DeError};
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};

#[derive(Debug, Clone, Deserialize)]
pub struct NaiveMmConfig {
    pub runtime: RuntimeSection,
    pub subscriptions: Vec<SubscriptionItem>,
    pub binance: BinanceSection,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RuntimeSection { pub channel_capacity: usize }

#[derive(Debug, Clone, Deserialize)]
pub struct SubscriptionItem {
    pub exchange: crate::cli::Exchange,
    pub stream_type: crate::cli::StreamType,
    pub instrument: String,
    #[serde(default = "default_arb_streams_num")]
    pub arb_streams_num: usize,
}

fn default_arb_streams_num() -> usize { 1 }

#[derive(Debug, Clone, Deserialize)]
pub struct BinanceSection {
    pub api_key: String,
    pub secret: String,
    pub account_id: i64,
    #[serde(deserialize_with = "deserialize_start_epoch_ts")]
    pub start_epoch_ts: i64,
    pub fee_bps: f64,
    pub contract_size: f64,
    pub symbols: Vec<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StartEpochInput {
    I(i64),
    S(String),
}

fn deserialize_start_epoch_ts<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: Deserializer<'de>,
{
    let v = StartEpochInput::deserialize(deserializer)?;
    match v {
        StartEpochInput::I(i) => Ok(i),
        StartEpochInput::S(s) => {
            // Try RFC3339 first
            if let Ok(dt) = DateTime::parse_from_rfc3339(s.trim()) {
                return Ok(dt.timestamp_micros());
            }
            // Try "YYYY-MM-DD HH:MM:SS"
            if let Ok(ndt) = NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M:%S") {
                let dt = DateTime::<Utc>::from_naive_utc_and_offset(ndt, Utc);
                return Ok(dt.timestamp_micros());
            }
            // Try "YYYY-MM-DD"
            if let Ok(nd) = NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d") {
                let ndt = nd.and_hms_opt(0, 0, 0).ok_or_else(|| D::Error::custom("invalid time components"))?;
                let dt = DateTime::<Utc>::from_naive_utc_and_offset(ndt, Utc);
                return Ok(dt.timestamp_micros());
            }
            // Try plain integer string
            if let Ok(i) = s.trim().parse::<i64>() {
                return Ok(i);
            }
            Err(D::Error::custom("Invalid start_epoch_ts. Use microseconds integer or a date: RFC3339, 'YYYY-MM-DD HH:MM:SS', or 'YYYY-MM-DD'"))
        }
    }
}


