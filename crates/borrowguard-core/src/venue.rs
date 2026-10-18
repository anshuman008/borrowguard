use crate::position::{leverage_from_ltv, Position};

/// Point-in-time view of a one-collateral, one-debt position on a lending venue.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub collateral_symbol: String,
    pub debt_symbol: String,
    /// Collateral in collateral-token units.
    pub collateral: f64,
    /// Debt in debt-token units.
    pub debt: f64,
    /// USD price of one collateral token, as the venue values it.
    pub collateral_price: f64,
    /// USD price of one debt token, as the venue values it.
    pub debt_price: f64,
    /// Debt / collateral value at which the venue liquidates.
    pub liquidation_ltv: f64,
    /// Highest debt / collateral value the venue lets you borrow up to.
    pub max_ltv: f64,
    /// The venue accepted its oracle prices as fresh and usable.
    pub prices_ok: bool,
    /// Chain slot the snapshot was taken at.
    pub slot: u64,
}

impl Snapshot {
    /// Collateral price in debt-token units — the price the controller uses.
    pub fn price(&self) -> f64 {
        self.collateral_price / self.debt_price
    }

    pub fn position(&self) -> Position {
        Position { collateral: self.collateral, debt: self.debt }
    }

    pub fn liquidation_leverage(&self) -> f64 {
        leverage_from_ltv(self.liquidation_ltv)
    }

    pub fn max_leverage(&self) -> f64 {
        leverage_from_ltv(self.max_ltv)
    }
}

/// A lending protocol BorrowGuard can watch (and later, rebalance on).
pub trait Venue {
    /// Short label for logs and alerts, e.g. "kamino".
    fn name(&self) -> &str;

    /// Fresh snapshot of the guarded position.
    fn snapshot(&self) -> anyhow::Result<Snapshot>;

    /// Accounts whose changes can move the position's health: the position
    /// itself and its price feeds. A streaming watcher re-checks the position
    /// when any of them changes. Empty means "poll only".
    fn watch_accounts(&self) -> anyhow::Result<Vec<[u8; 32]>> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn price_and_position_use_debt_units() {
        let snap = Snapshot {
            collateral_symbol: "SOL".into(),
            debt_symbol: "USDC".into(),
            collateral: 30.0,
            debt: 2_000.0,
            collateral_price: 100.0,
            debt_price: 1.0,
            liquidation_ltv: 0.75,
            max_ltv: 0.7,
            prices_ok: true,
            slot: 1,
        };
        assert_eq!(snap.price(), 100.0);
        assert_eq!(snap.position().leverage(snap.price()), Some(3.0));
        assert!((snap.liquidation_leverage() - 4.0).abs() < 1e-12);
    }
}
