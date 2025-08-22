//
use crate::xcommons::error::{AppError, Result};
use crate::xcommons::types::{OrderBookL2Update, OrderBookSnapshot, PriceLevel};
use crate::xcommons::oms::Side;
use std::collections::BTreeMap; // stay with BTreeMap for price ordering semantics

/// Order book manager for maintaining L2 state
pub struct OrderBookManager {
    symbol: String,
    market_id: Option<i64>,
    bids: BTreeMap<OrderedFloat, f64>, // price -> quantity
    asks: BTreeMap<OrderedFloat, f64>, // price -> quantity
    last_update_id: i64,
    sequence_counter: i64,
}

/// Wrapper for f64 to implement Ord for BTreeMap
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
struct OrderedFloat(f64);

impl Eq for OrderedFloat {}

impl Ord for OrderedFloat {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0
            .partial_cmp(&other.0)
            .unwrap_or(std::cmp::Ordering::Equal)
    }
}

impl From<f64> for OrderedFloat {
    fn from(f: f64) -> Self {
        OrderedFloat(f)
    }
}

impl OrderBookManager {
    pub fn new(symbol: String) -> Self {
        Self {
            symbol,
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            last_update_id: 0,
            sequence_counter: 0,
            market_id: None,
        }
    }

    /// Initialize order book from snapshot
    pub fn initialize_from_snapshot(&mut self, snapshot: OrderBookSnapshot) -> Result<()> {
        // Symbol verification skipped after migration to market_id; caller ensures mapping

        // Clear existing state
        self.bids.clear();
        self.asks.clear();

        // Load bids (descending order - highest price first)
        for bid in snapshot.bids {
            if bid.qty > 0.0 {
                self.bids.insert(OrderedFloat::from(bid.price), bid.qty);
            }
        }

        // Load asks (ascending order - lowest price first)
        for ask in snapshot.asks {
            if ask.qty > 0.0 {
                self.asks.insert(OrderedFloat::from(ask.price), ask.qty);
            }
        }

        self.last_update_id = snapshot.last_update_id;

        log::info!(
            "Order book initialized: {} bids, {} asks, last_update_id: {}",
            self.bids.len(),
            self.asks.len(),
            self.last_update_id
        );

        Ok(())
    }

    /// Apply L2 update to order book
    pub fn apply_update(&mut self, update: &OrderBookL2Update) -> Result<()> {
        // Optional: validate market id once known
        if let Some(mid) = self.market_id {
            if update.market_id != mid {
                return Err(AppError::pipeline("Market id mismatch".to_string()));
            }
        } else {
            self.market_id = Some(update.market_id);
        }

        // Validate update sequence
        if update.update_id < self.last_update_id {
            log::trace!(
                "Skipping old update: {} < {}",
                update.update_id,
                self.last_update_id
            );
            return Ok(());
        }

        // Apply the update
        let price_key = OrderedFloat::from(update.price);

        match update.side {
            Side::Buy => {
                if update.qty == 0.0 {
                    // Remove price level
                    self.bids.remove(&price_key);
                } else {
                    // Update price level
                    self.bids.insert(price_key, update.qty);
                }
            }
            Side::Sell => {
                if update.qty == 0.0 {
                    // Remove price level
                    self.asks.remove(&price_key);
                } else {
                    // Update price level
                    self.asks.insert(price_key, update.qty);
                }
            }
            _ => {}
        }

        self.last_update_id = update.update_id;

        Ok(())
    }

    /// Get best bid (highest price)
    pub fn best_bid(&self) -> Option<PriceLevel> {
        self.bids
            .iter()
            .rev()
            .next()
            .map(|(price, qty)| PriceLevel {
                price: price.0,
                qty: *qty,
            })
    }

    /// Get best ask (lowest price)
    pub fn best_ask(&self) -> Option<PriceLevel> {
        self.asks.iter().next().map(|(price, qty)| PriceLevel {
            price: price.0,
            qty: *qty,
        })
    }

    /// Get bid-ask spread
    pub fn spread(&self) -> Option<f64> {
        match (self.best_bid(), self.best_ask()) {
            (Some(bid), Some(ask)) => Some(ask.price - bid.price),
            _ => None,
        }
    }

    /// Get top N bid levels
    pub fn top_bids(&self, n: usize) -> Vec<PriceLevel> {
        self.bids
            .iter()
            .rev()
            .take(n)
            .map(|(price, qty)| PriceLevel {
                price: price.0,
                qty: *qty,
            })
            .collect()
    }

    /// Get top N ask levels
    pub fn top_asks(&self, n: usize) -> Vec<PriceLevel> {
        self.asks
            .iter()
            .take(n)
            .map(|(price, qty)| PriceLevel {
                price: price.0,
                qty: *qty,
            })
            .collect()
    }

    /// Get order book statistics
    pub fn stats(&self) -> OrderBookStats {
        let bid_count = self.bids.len();
        let ask_count = self.asks.len();

        let total_bid_qty: f64 = self.bids.values().sum();
        let total_ask_qty: f64 = self.asks.values().sum();

        let bid_volume = self.bids.iter().map(|(price, qty)| price.0 * qty).sum();

        let ask_volume = self.asks.iter().map(|(price, qty)| price.0 * qty).sum();

        OrderBookStats {
            symbol: self.symbol.clone(),
            last_update_id: self.last_update_id,
            bid_levels: bid_count,
            ask_levels: ask_count,
            total_bid_qty,
            total_ask_qty,
            bid_volume,
            ask_volume,
            spread: self.spread(),
            best_bid: self.best_bid(),
            best_ask: self.best_ask(),
        }
    }

    /// Get current sequence counter and increment
    pub fn next_sequence(&mut self) -> i64 {
        self.sequence_counter += 1;
        self.sequence_counter
    }

    /// Validate order book integrity
    pub fn validate(&self) -> Result<()> {
        // Check that bids are in descending order
        let mut prev_bid_price = f64::INFINITY;
        for (price, qty) in self.bids.iter().rev() {
            if price.0 >= prev_bid_price {
                return Err(AppError::pipeline(
                    "Bid prices not in descending order".to_string(),
                ));
            }
            if *qty <= 0.0 {
                return Err(AppError::pipeline("Invalid bid quantity <= 0".to_string()));
            }
            prev_bid_price = price.0;
        }

        // Check that asks are in ascending order
        let mut prev_ask_price = 0.0;
        for (price, qty) in &self.asks {
            if price.0 <= prev_ask_price {
                return Err(AppError::pipeline(
                    "Ask prices not in ascending order".to_string(),
                ));
            }
            if *qty <= 0.0 {
                return Err(AppError::pipeline("Invalid ask quantity <= 0".to_string()));
            }
            prev_ask_price = price.0;
        }

        // Check spread is positive
        if let (Some(bid), Some(ask)) = (self.best_bid(), self.best_ask()) {
            if ask.price <= bid.price {
                return Err(AppError::pipeline(format!(
                    "Invalid spread: ask {} <= bid {}",
                    ask.price, bid.price
                )));
            }
        }

        Ok(())
    }
}

/// Order book statistics
#[derive(Debug, Clone)]
pub struct OrderBookStats {
    pub symbol: String,
    pub last_update_id: i64,
    pub bid_levels: usize,
    pub ask_levels: usize,
    pub total_bid_qty: f64,
    pub total_ask_qty: f64,
    pub bid_volume: f64,
    pub ask_volume: f64,
    pub spread: Option<f64>,
    pub best_bid: Option<PriceLevel>,
    pub best_ask: Option<PriceLevel>,
}
