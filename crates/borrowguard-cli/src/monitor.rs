//! Read-only watch loop: snapshot each position, report what the controller
//! would do, and alert when that verdict changes. Never sends a transaction.

use std::{thread, time::Duration};

use anyhow::{bail, Context, Result};
use borrowguard_core::{Action, Params, Snapshot, Venue};
use chrono::Utc;

use crate::alerts::Notifier;

pub struct Settings {
    pub interval: Duration,
    pub once: bool,
}

/// Controller settings for one position. The venue supplies the liquidation
/// leverage at each check, completing the `Params`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Controller {
    pub target: f64,
    pub band: f64,
    pub ripcord: f64,
    pub cost_bps: f64,
    pub ripcord_cost_bps: f64,
    pub max_trade: Option<f64>,
    pub min_trade: f64,
}

impl Controller {
    pub const DEFAULT: Controller = Controller {
        target: 3.0,
        band: 0.15,
        ripcord: 3.6,
        cost_bps: 30.0,
        ripcord_cost_bps: 100.0,
        max_trade: None,
        min_trade: 1.0,
    };

    pub fn params(&self, liquidation: f64) -> Params {
        Params {
            target: self.target,
            band: self.band,
            ripcord: self.ripcord,
            liquidation,
            cost: self.cost_bps / 10_000.0,
            ripcord_cost: self.ripcord_cost_bps / 10_000.0,
            max_trade: self.max_trade,
            min_trade: self.min_trade,
        }
    }
}

/// One position under watch.
pub struct Watched {
    pub label: String,
    pub venue: Box<dyn Venue>,
    pub controller: Controller,
    last_status: &'static str,
    header_shown: bool,
}

impl Watched {
    pub fn new(label: String, venue: Box<dyn Venue>, controller: Controller) -> Self {
        Self { label, venue, controller, last_status: "", header_shown: false }
    }
}

/// What one check concluded.
struct Verdict {
    status: &'static str,
    line: String,
}

pub fn run(watched: &mut [Watched], notifier: &Notifier, settings: Settings) -> Result<()> {
    let multi = watched.len() > 1;
    loop {
        let mut failed = 0;
        for w in watched.iter_mut() {
            let prefix = if multi { format!("[{}] ", w.label) } else { String::new() };
            let verdict = w.venue.snapshot().and_then(|snap| {
                let params = w.controller.params(snap.liquidation_leverage());
                if !w.header_shown {
                    print_header(&prefix, w.venue.name(), &snap, &params);
                    w.header_shown = true;
                }
                evaluate(&snap, &params)
            });
            let now = Utc::now().format("%H:%M:%S");
            let (status, line) = match verdict {
                Ok(v) => (v.status, v.line),
                Err(err) => {
                    failed += 1;
                    ("error", format!("error: {err:#}"))
                }
            };
            println!("{now}  {prefix}{line}");
            if should_alert(w.last_status, status) {
                notifier.send(&format!("[borrowguard {}] {prefix}{line}", w.venue.name()));
            }
            w.last_status = status;
        }

        if settings.once {
            if failed > 0 {
                bail!("{failed} of {} position(s) failed", watched.len());
            }
            return Ok(());
        }
        thread::sleep(settings.interval);
    }
}

/// Alert when the verdict changes, but stay quiet when a watch starts healthy.
fn should_alert(last: &str, now: &str) -> bool {
    now != last && !(last.is_empty() && now == "hold")
}

fn print_header(prefix: &str, venue: &str, snap: &Snapshot, params: &Params) {
    println!(
        "{prefix}{venue}  {}/{}  liquidation at {:.2}× (LTV {:.1}%), borrow limit {:.2}× (LTV {:.1}%)",
        snap.collateral_symbol,
        snap.debt_symbol,
        snap.liquidation_leverage(),
        snap.liquidation_ltv * 100.0,
        snap.max_leverage(),
        snap.max_ltv * 100.0,
    );
    println!(
        "{prefix}controller  target {}× band ±{} ripcord {}× (dry run: nothing is traded)",
        params.target, params.band, params.ripcord
    );
    if params.target + params.band >= snap.max_leverage() {
        println!(
            "{prefix}warning: target + band reaches the venue's borrow limit; relevers would be rejected"
        );
    }
}

fn evaluate(snap: &Snapshot, params: &Params) -> Result<Verdict> {
    params.validate().with_context(|| {
        format!("controller settings vs venue liquidation at {:.2}×", params.liquidation)
    })?;
    let pos = snap.position();
    let price = snap.price();
    let (coll, debt) = (&snap.collateral_symbol, &snap.debt_symbol);

    let (status, action) = match params.decide(&pos, price) {
        Action::Hold => ("hold", "HOLD".to_string()),
        Action::Delever { sell, ripcord } => (
            if ripcord { "ripcord" } else { "delever" },
            format!(
                "{} sell {sell:.2} {debt} of {coll}, repay debt",
                if ripcord { "RIPCORD" } else { "DELEVER" }
            ),
        ),
        Action::Relever { borrow } => {
            ("relever", format!("RELEVER borrow {borrow:.2} {debt}, buy {coll}"))
        }
        Action::Insolvent => ("insolvent", "INSOLVENT: debt exceeds collateral".to_string()),
    };

    let leverage = pos.leverage(price).map_or("∞".to_string(), |l| format!("{l:.3}×"));
    let trigger = |level: f64| {
        pos.price_at_leverage(level)
            .map_or("n/a".to_string(), |p| format!("{p:.4} ({:+.1}%)", (p / price - 1.0) * 100.0))
    };
    let mut line = format!(
        "{coll}/{debt} {price:.4}  lev {leverage}  LTV {:.1}%  ripcord@ {}  liq@ {}  → {action}",
        pos.ltv(price) * 100.0,
        trigger(params.ripcord),
        trigger(params.liquidation),
    );
    if !snap.prices_ok {
        line.push_str("  [venue prices not fresh]");
    }
    Ok(Verdict { status, line })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(collateral: f64) -> Snapshot {
        Snapshot {
            collateral_symbol: "SOL".into(),
            debt_symbol: "USDC".into(),
            collateral,
            debt: 2_000.0,
            collateral_price: 100.0,
            debt_price: 1.0,
            liquidation_ltv: 0.80,
            max_ltv: 0.75,
            prices_ok: true,
            slot: 1,
        }
    }

    fn params() -> Params {
        Params {
            target: 3.0,
            band: 0.15,
            ripcord: 3.6,
            liquidation: 5.0,
            cost: 0.0,
            ripcord_cost: 0.0,
            max_trade: None,
            min_trade: 0.0,
        }
    }

    #[test]
    fn reports_hold_with_trigger_prices() {
        let v = evaluate(&snap(30.0), &params()).unwrap();
        assert_eq!(v.status, "hold");
        assert!(v.line.contains("lev 3.000×"), "{}", v.line);
        // 3× → 5× liquidation needs a ~16.7% drop.
        assert!(v.line.contains("liq@ 83.3333 (-16.7%)"), "{}", v.line);
    }

    #[test]
    fn reports_delever_and_ripcord() {
        // 28 SOL: $2,800 / $800 equity = 3.5×.
        assert_eq!(evaluate(&snap(28.0), &params()).unwrap().status, "delever");
        // 26 SOL: $2,600 / $600 equity ≈ 4.33× ≥ ripcord.
        let v = evaluate(&snap(26.0), &params()).unwrap();
        assert_eq!(v.status, "ripcord");
        assert!(v.line.contains("RIPCORD"));
    }

    #[test]
    fn alerts_on_changes_but_not_a_healthy_start() {
        assert!(!should_alert("", "hold"));
        assert!(should_alert("", "delever"));
        assert!(should_alert("", "error"));
        assert!(should_alert("hold", "ripcord"));
        assert!(!should_alert("ripcord", "ripcord"));
        assert!(should_alert("error", "hold"));
    }

    #[test]
    fn default_controller_matches_backtest_defaults() {
        let p = Controller::DEFAULT.params(4.0);
        assert!(p.validate().is_ok());
        assert_eq!((p.target, p.band, p.ripcord), (3.0, 0.15, 3.6));
    }

    #[test]
    fn rejects_ripcord_past_venue_liquidation() {
        let p = Params { liquidation: 3.5, ..params() };
        assert!(evaluate(&snap(30.0), &p).is_err());
    }
}
