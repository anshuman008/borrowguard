/// One-collateral, one-debt lending position (e.g. SOL collateral, USDC debt).
///
/// Values are expressed in the debt asset ("quote"), so with USDC debt every
/// value is in USD.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Position {
    /// Collateral in collateral-token units (e.g. SOL).
    pub collateral: f64,
    /// Debt in quote units (e.g. USDC).
    pub debt: f64,
}

impl Position {
    /// Position holding `equity` quote at `leverage`× with collateral at `price`.
    pub fn open(equity: f64, leverage: f64, price: f64) -> Self {
        Self { collateral: equity * leverage / price, debt: equity * (leverage - 1.0) }
    }

    pub fn collateral_value(&self, price: f64) -> f64 {
        self.collateral * price
    }

    pub fn equity(&self, price: f64) -> f64 {
        self.collateral_value(price) - self.debt
    }

    /// Collateral value / equity. `None` once equity is gone.
    pub fn leverage(&self, price: f64) -> Option<f64> {
        let equity = self.equity(price);
        (equity > 0.0).then(|| self.collateral_value(price) / equity)
    }

    /// Debt / collateral value — the ratio lenders liquidate on.
    pub fn ltv(&self, price: f64) -> f64 {
        self.debt / self.collateral_value(price)
    }

    /// Collateral price at which leverage reaches `level`. `None` without debt.
    pub fn price_at_leverage(&self, level: f64) -> Option<f64> {
        (self.debt > 0.0 && self.collateral > 0.0 && level > 1.0)
            .then(|| level * self.debt / ((level - 1.0) * self.collateral))
    }
}

/// Leverage at which a position's LTV equals `ltv`.
pub fn leverage_from_ltv(ltv: f64) -> f64 {
    1.0 / (1.0 - ltv)
}

/// LTV of a position running at `leverage`×.
pub fn ltv_from_leverage(leverage: f64) -> f64 {
    1.0 - 1.0 / leverage
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn open_hits_requested_leverage() {
        let pos = Position::open(1_000.0, 3.0, 146.0);
        assert!(close(pos.equity(146.0), 1_000.0));
        assert!(close(pos.debt, 2_000.0));
        assert!(close(pos.leverage(146.0).unwrap(), 3.0));
    }

    #[test]
    fn price_drop_raises_leverage() {
        // $3,000 SOL / $2,000 debt; SOL -5% → $2,850 / $850 equity ≈ 3.35×.
        let pos = Position::open(1_000.0, 3.0, 100.0);
        assert!(close(pos.leverage(95.0).unwrap(), 2_850.0 / 850.0));
    }

    #[test]
    fn leverage_is_none_without_equity() {
        let pos = Position::open(1_000.0, 3.0, 100.0);
        assert_eq!(pos.leverage(66.0), None);
    }

    #[test]
    fn price_at_leverage_round_trips() {
        let pos = Position::open(1_000.0, 3.0, 100.0);
        let liq = pos.price_at_leverage(4.0).unwrap();
        assert!(close(pos.leverage(liq).unwrap(), 4.0));
        // 3× → 4× needs roughly an 11.1% drop.
        assert!(close(liq, 100.0 * 8.0 / 9.0));
    }

    #[test]
    fn ltv_and_leverage_convert() {
        assert!(close(leverage_from_ltv(0.75), 4.0));
        assert!(close(ltv_from_leverage(4.0), 0.75));
        let pos = Position::open(1_000.0, 3.0, 100.0);
        assert!(close(pos.ltv(100.0), ltv_from_leverage(3.0)));
    }
}
