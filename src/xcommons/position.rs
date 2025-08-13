#![allow(dead_code)]

/// Tracks a trading position with average price, realized PnL, fees, and volumes.
/// Mirrors the logic from `docs/position.md` with the following intentional fixes:
/// - Uses an epsilon tolerance for near-zero checks to avoid floating-point dust
/// - Removes undefined `checkDD` call present in the Python snippet
/// - Corrects the zero-crossing branch to snap to exact flat when `amount + qty0` is ~0
#[derive(Debug, Clone)]
pub struct Position {
    pub amount: f64,
    pub avp: f64,
    pub realized: f64,
    pub fees: f64,
    pub fee_percent: f64,
    pub quote_volume: f64,
    pub quote_volume2: f64,
    pub trades_count: u32,
    pub contract_size: f64,
}

impl Default for Position {
    fn default() -> Self {
        Self::new(0.0, 1.0)
    }
}

impl Position {
    const EPSILON: f64 = 1e-8;

    pub fn new(fee_percent: f64, contract_size: f64) -> Self {
        let mut p = Self {
            amount: 0.0,
            avp: 0.0,
            realized: 0.0,
            fees: 0.0,
            fee_percent,
            quote_volume: 0.0,
            quote_volume2: 0.0,
            trades_count: 0,
            contract_size,
        };
        p.reset();
        p
    }

    pub fn reset(&mut self) {
        self.amount = 0.0;
        self.avp = 0.0;
        self.realized = 0.0;
        self.fees = 0.0;
        self.quote_volume = 0.0;
        self.quote_volume2 = 0.0;
        self.trades_count = 0;
    }

    #[inline]
    fn increment_average_price(&self, trade_quantity: f64, trade_price: f64) -> f64 {
        if self.amount.abs() < Self::EPSILON {
            trade_price
        } else {
            let weighted_notional = self.amount.abs() * self.avp + trade_price * trade_quantity.abs();
            let new_abs_amount = trade_quantity.abs() + self.amount.abs();
            weighted_notional / new_abs_amount
        }
    }

    #[inline]
    fn increase_position(&mut self, trade_quantity: f64, trade_price: f64) {
        self.avp = self.increment_average_price(trade_quantity, trade_price);
        assert!(self.avp.is_finite() && self.avp > 0.0, "average price must be positive and finite");
        self.amount += trade_quantity;
    }

    #[inline]
    fn trade_pnl(&self, quantity: f64, price: f64) -> f64 {
        (price - self.avp) * quantity * self.contract_size
    }

    #[inline]
    fn decrease_position(&mut self, trade_quantity: f64, trade_price: f64) {
        assert!(self.amount.abs() >= Self::EPSILON, "should have non-zero amount");
        // Realized PnL for the portion being reduced
        self.realized += self.trade_pnl(-trade_quantity, trade_price);
        self.amount += trade_quantity;
    }

    #[inline]
    pub fn notional(&self) -> f64 {
        self.amount * self.avp * self.contract_size
    }

    pub fn trade_to(&mut self, target_quantity: f64, trade_price: f64) {
        let diff = target_quantity - self.amount;
        if diff.abs() > Self::EPSILON {
            self.trade(diff, trade_price);
        }
    }

    pub fn trade(&mut self, requested_quantity: f64, trade_price: f64) {
        self.trades_count = self.trades_count.saturating_add(1);

        let abs_qty = requested_quantity.abs();
        assert!(abs_qty > 0.0, "trade quantity should be non-zero");

        self.quote_volume2 += abs_qty;
        self.quote_volume += abs_qty * trade_price * self.contract_size;

        // Snap to flat when the resulting position would be effectively zero.
        let mut trade_quantity = requested_quantity;
        if (self.amount + requested_quantity).abs() < Self::EPSILON {
            trade_quantity = -self.amount;
        }

        let pending_position = self.amount + trade_quantity;

        if self.amount.abs() < Self::EPSILON {
            self.increase_position(trade_quantity, trade_price);
        } else if pending_position.abs() < Self::EPSILON {
            self.decrease_position(trade_quantity, trade_price);
        } else if self.amount.signum() == pending_position.signum() {
            if pending_position.abs() > self.amount.abs() {
                self.increase_position(trade_quantity, trade_price);
            } else {
                self.decrease_position(trade_quantity, trade_price);
            }
        } else {
            // Crossed through zero: fully close existing then open the remainder
            let close_quantity = -self.amount;
            self.decrease_position(close_quantity, trade_price);
            let open_quantity = pending_position; // same sign as trade_quantity now
            self.increase_position(open_quantity, trade_price);
        }

        let fee = self.fee_percent * abs_qty * trade_price * self.contract_size;
        self.fees += fee;
        self.realized -= fee;

        if pending_position.abs() < Self::EPSILON && self.amount != 0.0 {
            self.amount = 0.0;
        }
    }

    pub fn trade_quote(&mut self, bid: f64, ask: f64, quantity: f64) {
        assert!(bid < ask, "BID={}, ASK={}", bid, ask);
        let price = if quantity > 0.0 { ask } else { bid };
        self.trade(quantity, price);
    }

    pub fn close_quote(&mut self, bid: f64, ask: f64) {
        assert!(bid < ask, "BID={}, ASK={}", bid, ask);
        if self.amount != 0.0 {
            self.trade_quote(bid, ask, -self.amount);
        }
    }

    pub fn close(&mut self, price: f64) {
        if self.amount != 0.0 {
            self.trade(-self.amount, price);
        }
    }

    pub fn mark_to_market_pnl(&self, bid: f64, ask: f64) -> f64 {
        assert!(bid <= ask, "BID={}, ASK={}", bid, ask);
        let mut mmpnl = self.realized;
        if self.amount != 0.0 {
            let price = if self.amount < 0.0 { ask } else { bid };
            mmpnl += self.trade_pnl(self.amount, price);
        }
        mmpnl
    }

    pub fn current_pnl(&self, price: f64) -> f64 {
        if self.amount != 0.0 {
            self.realized + self.trade_pnl(self.amount, price)
        } else {
            self.realized
        }
    }

    pub fn unrealized_pnl(&self, price: f64) -> f64 {
        self.trade_pnl(self.amount, price)
    }

    pub fn bps(&self) -> f64 {
        if self.quote_volume > 0.0 {
            self.realized / self.quote_volume * 10_000.0
        } else {
            0.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Position;

    fn approx_eq(a: f64, b: f64, eps: f64) -> bool { (a - b).abs() <= eps }

    #[test]
    fn test_long_increase_decrease_and_mtm() {
        let mut p = Position::new(0.0, 1.0);
        p.trade(10.0, 100.0); // long 10 @ 100
        assert!(approx_eq(p.avp, 100.0, 1e-12));
        p.trade(10.0, 110.0); // long 20, avp 105
        assert!(approx_eq(p.amount, 20.0, 1e-12));
        assert!(approx_eq(p.avp, 105.0, 1e-12));

        p.trade(-5.0, 120.0); // sell 5 → realized (120-105)*5 = 75
        assert!(approx_eq(p.amount, 15.0, 1e-12));
        assert!(approx_eq(p.realized, 75.0, 1e-12));

        let mtm = p.mark_to_market_pnl(119.0, 120.0); // long → use bid=119
        let expected_mtm = 75.0 + (119.0 - 105.0) * 15.0;
        assert!(approx_eq(mtm, expected_mtm, 1e-9));
    }

    #[test]
    fn test_short_increase_decrease() {
        let mut p = Position::new(0.0, 1.0);
        p.trade(-10.0, 100.0); // short 10 @ 100
        assert!(approx_eq(p.amount, -10.0, 1e-12));
        assert!(approx_eq(p.avp, 100.0, 1e-12));

        p.trade(-5.0, 110.0); // add to short → 15 @ avp 103.3333
        assert!(approx_eq(p.amount, -15.0, 1e-12));
        assert!(approx_eq(p.avp, (10.0*100.0 + 5.0*110.0)/15.0, 1e-12));

        // cover 6 at 90 → realized should be (avp - 90)*6
        let avp_before = p.avp;
        p.trade(6.0, 90.0);
        let expected_realized = (avp_before - 90.0) * 6.0;
        assert!(approx_eq(p.realized, expected_realized, 1e-9));
        assert!(approx_eq(p.amount, -9.0, 1e-12));
    }

    #[test]
    fn test_snap_to_flat_with_epsilon() {
        let mut p = Position::new(0.0, 1.0);
        p.trade(1.0, 100.0);
        // Request to close almost all (tiny residual below EPS should snap to 0)
        let tiny = 5e-9; // < EPS=1e-8
        p.trade(-(1.0 - tiny), 100.0);
        assert!(approx_eq(p.amount, 0.0, 1e-12));
    }

    #[test]
    fn test_fees_and_bps() {
        let mut p = Position::new(0.001, 1.0); // 10 bps fee
        p.trade(10.0, 100.0);
        // volume = 100 * 10 = 1000, fees = 0.001 * 1000 = 1
        assert!(approx_eq(p.quote_volume, 1000.0, 1e-12));
        assert!(approx_eq(p.fees, 1.0, 1e-12));
        assert!(approx_eq(p.realized, -1.0, 1e-12));
        assert!(approx_eq(p.bps(), -10.0, 1e-12));
    }
}


