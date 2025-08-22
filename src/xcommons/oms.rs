use crate::xcommons::types::time::now_micros;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::borrow::Cow;

/// Side enumeration for orders and executions
#[repr(i8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub enum Side {
    Unknown = 0,
    Buy = 1,
    Sell = -1,
}

impl std::fmt::Display for Side {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Side::Buy => write!(f, "BUY"),
            Side::Sell => write!(f, "SELL"),
            Side::Unknown => write!(f, "UNKNOWN"),
        }
    }
}

/// ExecutionType enum
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionType {
    XUnknown = 0,

    XTrade = 10,

    XOrderNew = 20,
    XOrderCanceled = 30,
    XOrderReplaced = 40,

    XOrderPendingNew = 100,
    XOrderPendingCancel = 110,
    XOrderPendingReplace = 120,

    XOrderRejected = 400,

    XOrderTriggered = 600,

    XFunding = 710,
    XLiquidation = 720,
}

impl ExecutionType {
    /// Returns true if this execution affects current position
    #[inline]
    pub fn is_position_change(&self) -> bool {
        matches!(self, ExecutionType::XTrade | ExecutionType::XLiquidation)
    }
}

/// OrderStatus enum
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderStatus {
    OStatusUnknown = 0,

    OStatusNew = 10,
    OStatusPartiallyFilled = 20,
    OStatusReplaced = 30,

    OStatusPendingNew = 110,
    OStatusPendingCancel = 120,
    OStatusPendingReplace = 130,

    OStatusFilled = 230,
    OStatusCanceled = 240,
    OStatusRejected = 250,
}

impl OrderStatus {
    /// Returns true if the order is active on the book
    #[inline]
    pub fn is_alive(&self) -> bool {
        matches!(
            self,
            OrderStatus::OStatusNew
                | OrderStatus::OStatusPartiallyFilled
                | OrderStatus::OStatusReplaced
        )
    }
}

/// TimeInForce enum
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimeInForce {
    TifUnknown = 0,
    TifDay = 10,
    TifGoodTillCancel = 20,
    TifImmediateOrCancel = 30,
    TifFillOrKill = 40,
}

/// OrderMode enum
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderMode {
    MUnknown = 0,
    MMarket = 10,
    MLimit = 20,
    MStopMarket = 2,
    MTrailingLimit = 3,
    MStopLimit = 4,
    MMarketIfTouched = 5,
    MLimitIfTouched = 6,
    MMarketWithLeftOverAsLimit = 7,
    MPegged = 8,
}

/// Enumeration of response status for order requests
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderResponseStatus {
    Unknown = 0,
    Ok = 5,
    FailedPostOnly = 10,
    FailedRateLimit = 20,
    Failed500 = 30,
    Failed400 = 40,
    FailedOrderNotFound = 50,
}

/// Unified execution (execution report) structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XExecution {
    /// Exchange timestamp (microseconds UTC)
    pub timestamp: i64,
    /// Local timestamp when the message was received (microseconds UTC)
    pub rcv_timestamp: i64,

    /// XMarketId mapping (int64)
    pub market_id: i64,

    /// Exchange account ID mapping
    pub account_id: i64,

    /// Execution type
    pub exec_type: ExecutionType,

    /// Side: -1 = sell, 1 = buy, 0 = unknown or no side
    pub side: Side,

    /// Exchange native order ID
    pub native_ord_id: String,

    /// Client order ID, values: -1 = no clOrdId, otherwise our client order ID
    pub cl_ord_id: i64,

    /// Client order ID in case of chained replace (exchange-specific), -1 = none
    pub orig_cl_ord_id: i64,

    /// Order status
    pub ord_status: OrderStatus,

    /// Last executed quantity
    pub last_qty: f64,
    /// Last executed price
    pub last_px: f64,

    /// Remaining quantity on the order
    pub leaves_qty: f64,

    /// Order quantity
    pub ord_qty: f64,

    /// Order price
    pub ord_price: f64,

    /// Order mode
    pub ord_mode: OrderMode,

    /// Time-in-force
    pub tif: TimeInForce,

    /// Fee amount reported by the exchange; if positive it's a fee, if negative it's a rebate
    pub fee: f64,

    /// Exchange native ID for the execution
    pub native_execution_id: String,

    /// Optional metadata included in the request
    pub metadata: HashMap<String, String>,

    /// Flag indicating it aggressively took liquidity (order-initiated match)
    pub is_taker: bool,
}

pub const TRACE_MSG_L2: &str = "L2";
pub const TRACE_MSG_OBS: &str = "OBS";
pub const TRACE_MSG_EXEC: &str = "EXEC";
pub const TRACE_MSG_UNKNOWN: &str = "UNKNOWN";
pub const TRACE_MSG_MTRADE: &str = "MTRADE";
pub const TRACE_MSG_POSITION: &str = "POSITION";
pub const TRACE_MSG_ORDER_RESPONSE: &str = "ORDER_RESPONSE";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TracingTimeStamps {
    pub initial_message: Cow<'static, str>,
    pub initial_exchange_ts: i64,
    pub rcv_ts: i64,
    pub proc_ts: i64,
    pub send_ts: i64,
}
impl TracingTimeStamps {
    /// Create new tracing timestamps with initial exchange timestamp
    pub fn new(message: &'static str, initial_exchange_ts: i64, rcv_ts: i64) -> Self {
        Self {
            initial_message: Cow::Borrowed(message),
            initial_exchange_ts,
            rcv_ts,
            proc_ts: now_micros(),
            send_ts: 0,
        }
    }

    pub fn set_rcv_ts(&mut self, rcv_ts: i64) -> &mut Self {
        self.rcv_ts = rcv_ts;
        self
    }
    pub fn set_proc_ts(&mut self, proc_ts: i64) -> &mut Self {
        self.proc_ts = proc_ts;
        self
    }
    pub fn set_send_ts(&mut self, send_ts: i64) -> &mut Self {
        self.send_ts = send_ts;
        self
    }
}
/// New order request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostRequest {
    /// Unique request ID generated by MonoSeq
    pub req_id: i64,
    /// Local timestamp when the request was created (microseconds UTC)
    pub timestamp: i64,

    pub cl_ord_id: i64,

    pub market_id: i64,
    pub account_id: i64,

    pub side: Side,
    /// Must be > 0
    pub qty: f64,
    /// Can be 0 for market orders
    pub price: f64,
    /// Default: limit
    pub ord_mode: OrderMode,
    /// Default: Good-Till-Cancelled (GTC)
    pub tif: TimeInForce,
    /// Default: true
    pub post_only: bool,
    /// Default: false
    pub reduce_only: bool,
    /// Default: none
    pub metadata: HashMap<String, String>,

    pub tracing: Option<TracingTimeStamps>,
}

/// Cancel request (one of cl_ord_id or native_ord_id must be provided)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelRequest {
    /// Unique request ID generated by MonoSeq
    pub req_id: i64,
    /// Local timestamp when the request was created (microseconds UTC)
    pub timestamp: i64,

    pub market_id: i64,
    pub account_id: i64,

    /// Optional client order ID
    pub cl_ord_id: Option<i64>,
    /// Optional exchange native order ID
    pub native_ord_id: Option<String>,

    pub tracing: Option<TracingTimeStamps>,
}

/// Amend request (one of cl_ord_id or native_ord_id must be provided; at least one of new_qty/new_price)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AmendRequest {
    /// Unique request ID generated by MonoSeq
    pub req_id: i64,
    /// Local timestamp when the request was created (microseconds UTC)
    pub timestamp: i64,

    pub market_id: i64,
    pub account_id: i64,

    /// Optional client order ID
    pub cl_ord_id: Option<i64>,
    /// Optional exchange native order ID
    pub native_ord_id: Option<String>,

    pub new_price: Option<f64>,
    pub new_qty: Option<f64>,
}

/// Typical REST order response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderResponse {
    /// Unique request ID that was sent; used for matching request/response
    pub req_id: i64,

    /// Exchange timestamp (microseconds UTC)
    pub timestamp: i64,
    /// Local timestamp when the response was received (microseconds UTC)
    pub rcv_timestamp: i64,

    /// Optional client order ID
    pub cl_ord_id: Option<i64>,
    /// Optional exchange native order ID
    pub native_ord_id: Option<String>,

    pub status: OrderResponseStatus,

    /// Optional execution details if response includes resulting order
    pub exec: Option<XExecution>,
}

/// Helpers for the xcl_{int64} clOrdId string format used externally
pub mod clordid {
    /// Format internal numeric client order id as external string with `xcl_` prefix
    #[inline]
    pub fn format_xcl(cl_ord_id: i64) -> String {
        format!("xcl_{}", cl_ord_id)
    }

    /// Parse external `xcl_{int64}` into internal numeric id. Returns None if format doesn't match.
    #[inline]
    pub fn parse_xcl(s: &str) -> Option<i64> {
        let s = s.trim();
        if let Some(rest) = s.strip_prefix("xcl_") {
            rest.parse::<i64>().ok()
        } else {
            None
        }
    }
}

/// Cancel all open orders for a market request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelAllRequest {
    /// Unique request ID generated by MonoSeq
    pub req_id: i64,
    /// Local timestamp when the request was created (microseconds UTC)
    pub timestamp: i64,

    pub market_id: i64,
    pub account_id: i64,

    pub tracing: Option<TracingTimeStamps>,
}

/// Unified order request enum for strategy → account API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OrderRequest {
    Post(PostRequest),
    Cancel(CancelRequest),
    CancelAll(CancelAllRequest),
}

impl OrderRequest {
    pub fn get_market_id(&self) -> i64 {
        match self {
            OrderRequest::Post(x) => x.market_id,
            OrderRequest::Cancel(x) => x.market_id,
            OrderRequest::CancelAll(x) => x.market_id,
        }
    }
    pub fn with_tracing(mut self, tracing_context: TracingTimeStamps) -> Self {
        match &mut self {
            OrderRequest::Post(req) => req.tracing = Some(tracing_context),
            OrderRequest::Cancel(req) => req.tracing = Some(tracing_context),
            OrderRequest::CancelAll(req) => req.tracing = Some(tracing_context),
        }
        self
    }
}
