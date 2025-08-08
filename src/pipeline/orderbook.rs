#![allow(dead_code)]
use crate::error::{AppError, Result};
use crate::types::{OrderBookL2Update, OrderBookSnapshot, OrderSide, PriceLevel};
use std::collections::BTreeMap;

/// Order book manager for maintaining L2 state
pub struct OrderBookManager {
    symbol: String,
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
        }
    }

    /// Initialize order book from snapshot
    pub fn initialize_from_snapshot(&mut self, snapshot: OrderBookSnapshot) -> Result<()> {
        if snapshot.symbol != self.symbol {
            return Err(AppError::pipeline(format!(
                "Symbol mismatch: expected {}, got {}",
                self.symbol, snapshot.symbol
            )));
        }

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
        // Validate symbol
        if update.ticker != self.symbol {
            return Err(AppError::pipeline(format!(
                "Symbol mismatch: expected {}, got {}",
                self.symbol, update.ticker
            )));
        }

        // Validate update sequence
        if update.update_id <= self.last_update_id {
            log::debug!(
                "Skipping old update: {} <= {}",
                update.update_id,
                self.last_update_id
            );
            return Ok(());
        }

        // Apply the update
        let price_key = OrderedFloat::from(update.price);

        match update.side {
            OrderSide::Bid => {
                if update.qty == 0.0 {
                    // Remove price level
                    self.bids.remove(&price_key);
                } else {
                    // Update price level
                    self.bids.insert(price_key, update.qty);
                }
            }
            OrderSide::Ask => {
                if update.qty == 0.0 {
                    // Remove price level
                    self.asks.remove(&price_key);
                } else {
                    // Update price level
                    self.asks.insert(price_key, update.qty);
                }
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ExchangeId, L2Action};

    #[test]
    fn test_order_book_initialization() {
        let mut ob = OrderBookManager::new("BTCUSDT".to_string());

        let snapshot = OrderBookSnapshot {
            symbol: "BTCUSDT".to_string(),
            last_update_id: 100,
            exchange_id: ExchangeId::BinanceFutures,
            timestamp: crate::types::time::now_millis(),
            sequence: 100,
            bids: vec![
                PriceLevel {
                    price: 50000.0,
                    qty: 1.0,
                },
                PriceLevel {
                    price: 49999.0,
                    qty: 2.0,
                },
            ],
            asks: vec![
                PriceLevel {
                    price: 50001.0,
                    qty: 1.5,
                },
                PriceLevel {
                    price: 50002.0,
                    qty: 2.5,
                },
            ],
        };

        ob.initialize_from_snapshot(snapshot).unwrap();

        assert_eq!(ob.best_bid().unwrap().price, 50000.0);
        assert_eq!(ob.best_ask().unwrap().price, 50001.0);
        assert_eq!(ob.spread().unwrap(), 1.0);
    }

    #[test]
    fn test_order_book_updates() {
        let mut ob = OrderBookManager::new("BTCUSDT".to_string());

        // Initialize with snapshot
        let snapshot = OrderBookSnapshot {
            symbol: "BTCUSDT".to_string(),
            last_update_id: 100,
            exchange_id: ExchangeId::BinanceFutures,
            timestamp: crate::types::time::now_millis(),
            sequence: 100,
            bids: vec![PriceLevel {
                price: 50000.0,
                qty: 1.0,
            }],
            asks: vec![PriceLevel {
                price: 50001.0,
                qty: 1.0,
            }],
        };
        ob.initialize_from_snapshot(snapshot).unwrap();

        // Apply bid update
        let update = OrderBookL2Update {
            timestamp: 0,
            rcv_timestamp: 0,
            exchange: ExchangeId::BinanceFutures,
            ticker: "BTCUSDT".to_string(),
            seq_id: 1,
            packet_id: 1,
            update_id: 101,
            first_update_id: 101,
            action: L2Action::Update,
            side: OrderSide::Bid,
            price: 50000.5,
            qty: 2.0,
        };

        ob.apply_update(&update).unwrap();

        assert_eq!(ob.best_bid().unwrap().price, 50000.5);
        assert_eq!(ob.best_bid().unwrap().qty, 2.0);
    }

    #[test]
    fn test_order_book_removal() {
        let mut ob = OrderBookManager::new("BTCUSDT".to_string());

        // Initialize with snapshot
        let snapshot = OrderBookSnapshot {
            symbol: "BTCUSDT".to_string(),
            last_update_id: 100,
            exchange_id: ExchangeId::BinanceFutures,
            timestamp: crate::types::time::now_millis(),
            sequence: 100,
            bids: vec![PriceLevel {
                price: 50000.0,
                qty: 1.0,
            }],
            asks: vec![PriceLevel {
                price: 50001.0,
                qty: 1.0,
            }],
        };
        ob.initialize_from_snapshot(snapshot).unwrap();

        // Remove bid level (qty = 0)
        let update = OrderBookL2Update {
            timestamp: 0,
            rcv_timestamp: 0,
            exchange: ExchangeId::BinanceFutures,
            ticker: "BTCUSDT".to_string(),
            seq_id: 1,
            packet_id: 1,
            update_id: 101,
            first_update_id: 101,
            action: L2Action::Update,
            side: OrderSide::Bid,
            price: 50000.0,
            qty: 0.0,
        };

        ob.apply_update(&update).unwrap();

        assert!(ob.best_bid().is_none());
    }
}
