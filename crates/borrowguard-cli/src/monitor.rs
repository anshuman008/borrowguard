//! Read-only watch loop: snapshot the position, report what the controller
//! would do, and alert when that verdict changes. Never sends a transaction.

use std::{thread, time::Duration};

use anyhow::{Context, Result};
use borrowguard_core::{Action, Params, Snapshot, Venue};
use chrono::Utc;

use crate::alerts::Notifier;

pub struct Settings {
    pub interval: Duration,
    pub once: bool,
}

/// What one tick concluded.
struct Verdict {
    status: &'static str,
    line: String,
}

/// `params` builds the controller for the venue's current liquidation leverage.
pub fn run(
    venue: &dyn Venue,
    params: impl Fn(f64) -> Params,
    notifier: &Notifier,
    settings: Settings,
) -> Result<()> {
    let mut last_status = "";
    let mut first = true;
    loop {
        let verdict = venue.snapshot().and_then(|snap| {
            let params = params(snap.liquidation_leverage());
            if first {
                print_header(venue.name(), &snap, &params);
                first = false;
            }
            evaluate(&snap, &params)
        });
        let now = Utc::now().format("%H:%M:%S");
        let (status, line) = match verdict {
            Ok(v) => (v.status, v.line),
            Err(err) if settings.once => return Err(err),
            Err(err) => ("error", format!("error: {err:#}")),
        };
        println!("{now}  {line}");
        if status != last_status && !(last_status.is_empty() && status == "hold") {
            notifier.send(&format!("[borrowguard {}] {line}", venue.name()));
        }
        last_status = status;

        if settings.once {
            return Ok(());
        }
        thread::sleep(settings.interval);
    }
}

fn print_header(venue: &str, snap: &Snapshot, params: &Params) {
    println!(
        "{venue}  {}/{}  liquidation at {:.2}× (LTV {:.1}%), borrow limit {:.2}× (LTV {:.1}%)",
        snap.collateral_symbol,
        snap.debt_symbol,
        snap.liquidation_leverage(),
        snap.liquidation_ltv * 100.0,
        snap.max_leverage(),
        snap.max_ltv * 100.0,
    );
    println!(
        "controller  target {}× band ±{} ripcord {}× (dry run: nothing is traded)",
        params.target, params.band, params.ripcord
    );
    if params.target + params.band >= snap.max_leverage() {
        println!(
            "warning: target + band reaches the venue's borrow limit; relevers would be rejected"
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
    fn rejects_ripcord_past_venue_liquidation() {
        let p = Params { liquidation: 3.5, ..params() };
        assert!(evaluate(&snap(30.0), &p).is_err());
    }
}
