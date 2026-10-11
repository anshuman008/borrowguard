use crate::position::Position;

/// Controller settings. Every rebalance returns the position to `target`.
///
/// ```text
///   1× ── target-band ══ target ══ target+band ── ripcord ── liquidation
///         relever ◄─────── hold ───────► delever    emergency  lender acts
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Params {
    /// Leverage every rebalance returns to.
    pub target: f64,
    /// Half-width of the no-trade band around `target`.
    pub band: f64,
    /// Emergency delever level. Must sit below `liquidation`.
    pub ripcord: f64,
    /// Leverage at which the lender liquidates (from its liquidation LTV).
    pub liquidation: f64,
    /// Expected fraction of trade value lost to swap slippage and fees.
    pub cost: f64,
    /// Same as `cost`, for ripcord trades (wider slippage allowed).
    pub ripcord_cost: f64,
    /// Max quote value per routine trade. Ripcord trades ignore it.
    pub max_trade: Option<f64>,
    /// Routine trades smaller than this (quote) are skipped as dust.
    pub min_trade: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
pub enum ParamsError {
    #[error("band {band} must be positive and keep target - band above 1x (target {target})")]
    Band { target: f64, band: f64 },
    #[error("need target + band < ripcord < liquidation, got {upper} / {ripcord} / {liquidation}")]
    Order { upper: f64, ripcord: f64, liquidation: f64 },
    #[error("cost {0} must be in [0, 1 / target)")]
    Cost(f64),
    #[error("max_trade must be positive")]
    MaxTrade,
    #[error("min_trade must not be negative")]
    MinTrade,
}

/// What the controller wants done at the current price.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    Hold,
    /// Sell collateral worth `sell` quote and repay debt with the proceeds.
    Delever {
        sell: f64,
        ripcord: bool,
    },
    /// Borrow `borrow` quote and buy collateral with it.
    Relever {
        borrow: f64,
    },
    /// Equity is gone; nothing left to rebalance.
    Insolvent,
}

impl Params {
    pub fn validate(&self) -> Result<(), ParamsError> {
        if self.band <= 0.0 || self.target - self.band <= 1.0 {
            return Err(ParamsError::Band { target: self.target, band: self.band });
        }
        let upper = self.target + self.band;
        if upper >= self.ripcord || self.ripcord >= self.liquidation {
            return Err(ParamsError::Order {
                upper,
                ripcord: self.ripcord,
                liquidation: self.liquidation,
            });
        }
        // Delever sizing divides by (1 - target * cost).
        for cost in [self.cost, self.ripcord_cost] {
            if !(0.0..1.0 / self.target).contains(&cost) {
                return Err(ParamsError::Cost(cost));
            }
        }
        if self.max_trade.is_some_and(|max| max <= 0.0) {
            return Err(ParamsError::MaxTrade);
        }
        if self.min_trade < 0.0 {
            return Err(ParamsError::MinTrade);
        }
        Ok(())
    }

    /// Decide the trade that brings `pos` back to `target` at `price`.
    ///
    /// Sizes account for execution cost so the position lands on target
    /// after paying it: selling `x` of collateral repays only `x * (1 - cost)`
    /// of debt, and borrowing `b` buys only `b * (1 - cost)` of collateral.
    pub fn decide(&self, pos: &Position, price: f64) -> Action {
        let Some(leverage) = pos.leverage(price) else {
            return Action::Insolvent;
        };
        let value = pos.collateral_value(price);
        let target_value = self.target * pos.equity(price);

        if leverage >= self.ripcord {
            // Get out of danger in one trade: no size cap, no dust filter.
            let sell = (value - target_value) / (1.0 - self.target * self.ripcord_cost);
            return Action::Delever { sell, ripcord: true };
        }
        if leverage > self.target + self.band {
            let sell = (value - target_value) / (1.0 - self.target * self.cost);
            return self
                .routine_size(sell)
                .map_or(Action::Hold, |sell| Action::Delever { sell, ripcord: false });
        }
        if leverage < self.target - self.band {
            let borrow = (target_value - value) / (1.0 + self.cost * (self.target - 1.0));
            return self
                .routine_size(borrow)
                .map_or(Action::Hold, |borrow| Action::Relever { borrow });
        }
        Action::Hold
    }

    /// Position expected after executing `action` at `price`, net of costs.
    pub fn after_trade(&self, pos: &Position, action: Action, price: f64) -> Position {
        match action {
            Action::Delever { sell, ripcord } => {
                let cost = if ripcord { self.ripcord_cost } else { self.cost };
                Position {
                    collateral: pos.collateral - sell / price,
                    debt: pos.debt - sell * (1.0 - cost),
                }
            }
            Action::Relever { borrow } => Position {
                collateral: pos.collateral + borrow * (1.0 - self.cost) / price,
                debt: pos.debt + borrow,
            },
            Action::Hold | Action::Insolvent => *pos,
        }
    }

    fn routine_size(&self, size: f64) -> Option<f64> {
        let size = self.max_trade.map_or(size, |max| size.min(max));
        (size >= self.min_trade).then_some(size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> Params {
        Params {
            target: 3.0,
            band: 0.15,
            ripcord: 3.6,
            liquidation: 4.0,
            cost: 0.0,
            ripcord_cost: 0.0,
            max_trade: None,
            min_trade: 0.0,
        }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn holds_inside_band() {
        let pos = Position::open(1_000.0, 3.0, 100.0);
        assert_eq!(params().decide(&pos, 100.0), Action::Hold);
        assert_eq!(params().decide(&pos, 101.0), Action::Hold);
    }

    #[test]
    fn delevers_after_drop() {
        // SOL -5%: $2,850 collateral, $850 equity → sell $300, repay $300.
        let p = params();
        let pos = Position::open(1_000.0, 3.0, 100.0);
        let action = p.decide(&pos, 95.0);
        let Action::Delever { sell, ripcord: false } = action else {
            panic!("expected routine delever, got {action:?}");
        };
        assert!(close(sell, 300.0));
        let after = p.after_trade(&pos, action, 95.0);
        assert!(close(after.leverage(95.0).unwrap(), 3.0));
        assert!(close(after.debt, 1_700.0));
    }

    #[test]
    fn relevers_after_rise() {
        // SOL +5%: $3,150 collateral, $1,150 equity → borrow $300 more.
        let p = params();
        let pos = Position::open(1_000.0, 3.0, 100.0);
        let action = p.decide(&pos, 105.0);
        let Action::Relever { borrow } = action else {
            panic!("expected relever, got {action:?}");
        };
        assert!(close(borrow, 300.0));
        let after = p.after_trade(&pos, action, 105.0);
        assert!(close(after.leverage(105.0).unwrap(), 3.0));
    }

    #[test]
    fn lands_on_target_after_costs() {
        let p = Params { cost: 0.01, ripcord_cost: 0.02, ..params() };
        let pos = Position::open(1_000.0, 3.0, 100.0);
        for price in [95.0, 105.0, 91.0] {
            let action = p.decide(&pos, price);
            assert_ne!(action, Action::Hold, "price {price}");
            let after = p.after_trade(&pos, action, price);
            assert!(close(after.leverage(price).unwrap(), 3.0), "price {price}");
            // Costs come out of equity.
            assert!(after.equity(price) < pos.equity(price));
        }
    }

    #[test]
    fn ripcord_fires_and_ignores_cap() {
        // SOL -9%: leverage ≈ 3.85× ≥ 3.6× ripcord.
        let p = Params { max_trade: Some(10.0), min_trade: 50.0, ..params() };
        let pos = Position::open(1_000.0, 3.0, 100.0);
        let action = p.decide(&pos, 91.0);
        let Action::Delever { sell, ripcord: true } = action else {
            panic!("expected ripcord, got {action:?}");
        };
        assert!(sell > 10.0);
        assert!(close(p.after_trade(&pos, action, 91.0).leverage(91.0).unwrap(), 3.0));
    }

    #[test]
    fn routine_trades_are_capped() {
        let p = Params { max_trade: Some(100.0), ..params() };
        let pos = Position::open(1_000.0, 3.0, 100.0);
        assert_eq!(p.decide(&pos, 95.0), Action::Delever { sell: 100.0, ripcord: false });
    }

    #[test]
    fn dust_trades_are_skipped() {
        let p = Params { max_trade: Some(5.0), min_trade: 10.0, ..params() };
        let pos = Position::open(1_000.0, 3.0, 100.0);
        assert_eq!(p.decide(&pos, 95.0), Action::Hold);
    }

    #[test]
    fn insolvent_when_equity_gone() {
        let pos = Position::open(1_000.0, 3.0, 100.0);
        assert_eq!(params().decide(&pos, 60.0), Action::Insolvent);
    }

    #[test]
    fn validate_rejects_bad_params() {
        assert!(params().validate().is_ok());
        let bad = [
            Params { band: 0.0, ..params() },
            Params { band: 2.0, ..params() },
            Params { ripcord: 3.1, ..params() },
            Params { ripcord: 4.0, ..params() },
            Params { cost: 0.5, ..params() },
            Params { ripcord_cost: -0.1, ..params() },
            Params { max_trade: Some(0.0), ..params() },
            Params { min_trade: -1.0, ..params() },
        ];
        for p in bad {
            assert!(p.validate().is_err(), "{p:?}");
        }
    }
}
