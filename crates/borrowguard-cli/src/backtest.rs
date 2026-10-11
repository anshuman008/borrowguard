//! Replays the controller over historical candles.
//!
//! Model (deliberately conservative):
//! - The bot only acts at each bar's close.
//! - Liquidation is checked first, at each bar's low, so an intrabar crash
//!   the bot could not react to still counts. Use short bars (1m/5m) to
//!   approximate a bot that watches continuously.
//! - Liquidation is total: the lender repays all debt and keeps a bonus of
//!   `liq_bonus × debt` from collateral. Real Kamino liquidations are
//!   partial, so this overstates the loss of a single breach.
//! - Interest accrues per bar at constant APRs.

use anyhow::{Context, Result};
use borrowguard_core::{Action, Params, Position};
use serde::Serialize;

use crate::prices::Candle;

const MS_PER_YEAR: f64 = 365.25 * 24.0 * 3_600.0 * 1_000.0;

#[derive(Debug, Clone, Copy)]
pub struct SimConfig {
    pub params: Params,
    /// `false` opens at target and never trades — the baseline to beat.
    pub managed: bool,
    pub equity: f64,
    /// Annual borrow rate on debt, as a fraction.
    pub borrow_apr: f64,
    /// Annual yield on collateral (e.g. LST staking), as a fraction.
    pub supply_apr: f64,
    /// Liquidation bonus the lender takes, as a fraction of debt.
    pub liq_bonus: f64,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Trade {
    pub time: i64,
    pub price: f64,
    pub kind: &'static str,
    /// Quote value sold (delever) or borrowed (relever).
    pub size: f64,
    pub leverage_before: f64,
    pub leverage_after: f64,
}

#[derive(Debug, Clone)]
pub struct Report {
    pub final_equity: f64,
    pub max_leverage: f64,
    pub liquidated_at: Option<i64>,
    pub trades: Vec<Trade>,
    /// Equity lost to execution costs.
    pub costs: f64,
    /// Borrow interest paid.
    pub interest: f64,
}

impl Report {
    pub fn count(&self, kind: &str) -> usize {
        self.trades.iter().filter(|t| t.kind == kind).count()
    }
}

pub fn run(cfg: &SimConfig, candles: &[Candle]) -> Result<Report> {
    let (first, rest) = candles.split_first().context("no candles to backtest")?;
    let params = &cfg.params;
    let mut pos = Position::open(cfg.equity, params.target, first.close);
    let mut report = Report {
        final_equity: cfg.equity,
        max_leverage: params.target,
        liquidated_at: None,
        trades: Vec::new(),
        costs: 0.0,
        interest: 0.0,
    };
    let mut last_time = first.time;

    for bar in rest {
        let years = (bar.time - last_time) as f64 / MS_PER_YEAR;
        last_time = bar.time;
        let interest = pos.debt * cfg.borrow_apr * years;
        pos.debt += interest;
        pos.collateral *= 1.0 + cfg.supply_apr * years;
        report.interest += interest;

        // Worst point of the bar for a long position is its low.
        let worst = pos.leverage(bar.low);
        report.max_leverage = report.max_leverage.max(worst.unwrap_or(f64::INFINITY));
        if worst.is_none_or(|lev| lev >= params.liquidation) {
            report.liquidated_at = Some(bar.time);
            report.final_equity = (pos.equity(bar.low) - cfg.liq_bonus * pos.debt).max(0.0);
            return Ok(report);
        }

        if cfg.managed {
            let action = params.decide(&pos, bar.close);
            let kind = match action {
                Action::Delever { ripcord: true, .. } => "ripcord",
                Action::Delever { .. } => "delever",
                Action::Relever { .. } => "relever",
                // Insolvent at close implies liquidated at the low above.
                Action::Hold | Action::Insolvent => "",
            };
            if !kind.is_empty() {
                let after = params.after_trade(&pos, action, bar.close);
                report.costs += pos.equity(bar.close) - after.equity(bar.close);
                report.trades.push(Trade {
                    time: bar.time,
                    price: bar.close,
                    kind,
                    size: match action {
                        Action::Delever { sell, .. } => sell,
                        Action::Relever { borrow } => borrow,
                        _ => 0.0,
                    },
                    leverage_before: pos.leverage(bar.close).unwrap_or(f64::INFINITY),
                    leverage_after: after.leverage(bar.close).unwrap_or(f64::INFINITY),
                });
                pos = after;
            }
        }
        report.final_equity = pos.equity(bar.close);
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(managed: bool) -> SimConfig {
        SimConfig {
            params: Params {
                target: 3.0,
                band: 0.15,
                ripcord: 3.6,
                liquidation: 4.0,
                cost: 0.0,
                ripcord_cost: 0.0,
                max_trade: None,
                min_trade: 0.0,
            },
            managed,
            equity: 1_000.0,
            borrow_apr: 0.0,
            supply_apr: 0.0,
            liq_bonus: 0.05,
        }
    }

    /// Hourly bars that close (and bottom out) at each price.
    fn bars(prices: &[f64]) -> Vec<Candle> {
        prices
            .iter()
            .enumerate()
            .map(|(i, &p)| Candle {
                time: i as i64 * 3_600_000,
                open: p,
                high: p,
                low: p,
                close: p,
            })
            .collect()
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn flat_prices_never_trade() {
        let report = run(&cfg(true), &bars(&[100.0; 50])).unwrap();
        assert!(report.trades.is_empty());
        assert!(close(report.final_equity, 1_000.0));
    }

    #[test]
    fn three_drops_liquidate_unmanaged_but_not_managed() {
        // SOL falls 5% three times: ~14% total, past the ~11% 3× cushion.
        let prices = bars(&[100.0, 95.0, 90.25, 85.7375]);

        let unmanaged = run(&cfg(false), &prices).unwrap();
        assert_eq!(unmanaged.liquidated_at, Some(3 * 3_600_000));

        let managed = run(&cfg(true), &prices).unwrap();
        assert_eq!(managed.liquidated_at, None);
        assert_eq!(managed.count("delever"), 3);
        // Equity tracks 3× the move: 1000 × (1 - 0.15)^3 ≈ 614.
        assert!(close(managed.final_equity, 1_000.0 * 0.85_f64.powi(3)));
    }

    #[test]
    fn round_trip_shows_volatility_decay() {
        // -5% then back to the start: the rebalanced position ends lower.
        let report = run(&cfg(true), &bars(&[100.0, 95.0, 100.0])).unwrap();
        assert_eq!(report.trades.len(), 2);
        assert!(report.final_equity < 1_000.0);
        assert!(report.final_equity > 980.0);
    }

    #[test]
    fn interest_accrues_on_debt() {
        let mut c = cfg(false);
        c.borrow_apr = 0.10;
        // One year of flat hourly bars.
        let n = (MS_PER_YEAR / 3_600_000.0) as usize + 1;
        let report = run(&c, &bars(&vec![100.0; n])).unwrap();
        // ~10% simple interest per bar, compounded hourly on $2,000 debt.
        assert!(report.interest > 200.0 && report.interest < 211.0);
        assert!(close(report.final_equity, 1_000.0 - report.interest));
    }
}
