//! Watch loop: snapshot each position, report what the controller would do,
//! and alert when that verdict changes. Positions with an [`AutoExec`] also
//! rebalance, behind a kill switch, a cooldown and a daily cap; they only send
//! transactions when their `ExecContext` says so (`watch --live`).

use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::mpsc::{Receiver, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use borrowguard_core::{Action, Params, Snapshot, Venue};
use borrowguard_exec::{ExecContext, Execute, Executed};
use chrono::Utc;
use solana_sdk::pubkey::Pubkey;

use crate::alerts::Notifier;

pub struct Settings {
    /// Every position is checked (and printed) at least this often.
    pub interval: Duration,
    pub once: bool,
    /// Least time between two stream-triggered checks of one position.
    pub min_check: Duration,
}

/// Account-change notifications, and the positions each account affects.
pub struct Feed {
    pub changes: Receiver<Pubkey>,
    pub positions: HashMap<Pubkey, Vec<usize>>,
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
    pub auto: Option<AutoExec>,
    last_status: &'static str,
    header_shown: bool,
    last_check: Option<Instant>,
    /// A watched account changed since the last check.
    dirty: bool,
    /// Stream-triggered checks since the last heartbeat.
    rechecks: usize,
}

impl Watched {
    pub fn new(label: String, venue: Box<dyn Venue>, controller: Controller) -> Self {
        Self {
            label,
            venue,
            controller,
            auto: None,
            last_status: "",
            header_shown: false,
            last_check: None,
            dirty: false,
            rechecks: 0,
        }
    }

    pub fn with_auto(mut self, auto: AutoExec) -> Self {
        self.auto = Some(auto);
        self
    }

    fn mode(&self) -> &'static str {
        match &self.auto {
            None => "monitor only: nothing is traded",
            Some(a) if a.ctx.send => "AUTO-REBALANCE LIVE: trades are sent",
            Some(_) => "auto-rebalance simulated: add --live to send",
        }
    }
}

/// A ripcord may retry sooner than a routine trade: it is the escape hatch.
const RIPCORD_COOLDOWN: Duration = Duration::from_secs(20);
const DAY: Duration = Duration::from_secs(86_400);

/// Automatic rebalancing for one position.
pub struct AutoExec {
    pub executor: Box<dyn Execute>,
    pub ctx: ExecContext,
    pub cooldown: Duration,
    pub max_trades_per_day: usize,
    /// While this file exists nothing is sent or simulated.
    pub kill_switch: PathBuf,
    last_attempt: Option<Instant>,
    sent: VecDeque<Instant>,
}

/// Whether a wanted trade may go out now.
#[derive(Debug, PartialEq)]
enum Gate {
    Go,
    KillSwitch,
    Cooldown,
    DailyCap,
}

impl AutoExec {
    pub fn new(
        executor: Box<dyn Execute>,
        ctx: ExecContext,
        cooldown: Duration,
        max_trades_per_day: usize,
        kill_switch: PathBuf,
    ) -> Self {
        Self {
            executor,
            ctx,
            cooldown,
            max_trades_per_day,
            kill_switch,
            last_attempt: None,
            sent: VecDeque::new(),
        }
    }

    fn gate(&self, now: Instant, ripcord: bool) -> Gate {
        if self.kill_switch.exists() {
            return Gate::KillSwitch;
        }
        let cooldown = if ripcord { self.cooldown.min(RIPCORD_COOLDOWN) } else { self.cooldown };
        if self.last_attempt.is_some_and(|t| now.duration_since(t) < cooldown) {
            return Gate::Cooldown;
        }
        let recent = self.sent.iter().filter(|t| now.duration_since(**t) < DAY).count();
        if recent >= self.max_trades_per_day {
            return Gate::DailyCap;
        }
        Gate::Go
    }

    /// Carry out the controller's action if the gates allow. Returns a line
    /// to print and whether it deserves an alert.
    fn act(&mut self, action: Action) -> Option<Outcome> {
        let ripcord = matches!(action, Action::Delever { ripcord: true, .. });
        if matches!(action, Action::Hold | Action::Insolvent) {
            return None;
        }
        let now = Instant::now();
        match self.gate(now, ripcord) {
            Gate::Go => {}
            Gate::Cooldown => return None,
            Gate::KillSwitch => {
                let msg = format!("trading paused: {} exists", self.kill_switch.display());
                return Some(Outcome { msg, alert: false, blocked: true });
            }
            Gate::DailyCap => {
                let msg =
                    format!("daily cap of {} trades reached; not trading", self.max_trades_per_day);
                return Some(Outcome { msg, alert: false, blocked: true });
            }
        }
        self.last_attempt = Some(now);
        let result = match action {
            Action::Delever { sell, ripcord } => self.executor.delever(&self.ctx, sell, ripcord),
            Action::Relever { borrow } => self.executor.relever(&self.ctx, borrow),
            Action::Hold | Action::Insolvent => unreachable!("filtered above"),
        };
        let (msg, alert) = match result {
            Ok(Executed::Sent { signature, summary }) => {
                self.sent.push_back(Instant::now());
                while self.sent.front().is_some_and(|t| now.duration_since(*t) >= DAY) {
                    self.sent.pop_front();
                }
                (format!("TRADED {summary} · {signature}"), true)
            }
            Ok(Executed::Simulated { summary, .. }) => {
                (format!("would trade (simulated, not sent): {summary}"), false)
            }
            Err(err) => (format!("trade FAILED: {err:#}"), true),
        };
        Some(Outcome { msg, alert, blocked: false })
    }
}

/// What an automatic rebalance attempt did.
#[derive(Debug, PartialEq)]
struct Outcome {
    msg: String,
    alert: bool,
    /// Held back by the kill switch or the daily cap.
    blocked: bool,
}

/// What one check concluded.
struct Verdict {
    status: &'static str,
    line: String,
    action: Action,
}

pub fn run(
    watched: &mut [Watched],
    notifier: &Notifier,
    settings: Settings,
    mut feed: Option<Feed>,
) -> Result<()> {
    let multi = watched.len() > 1;
    if settings.once {
        let failed =
            watched.iter_mut().map(|w| check(w, multi, notifier, false)).filter(|ok| !ok).count();
        if failed > 0 {
            bail!("{failed} of {} position(s) failed", watched.len());
        }
        return Ok(());
    }

    let mut heartbeat = Instant::now();
    loop {
        let now = Instant::now();
        let beat = now >= heartbeat;
        for w in watched.iter_mut() {
            let due = w.dirty && w.last_check.is_none_or(|t| now - t >= settings.min_check);
            if beat || due {
                // Between heartbeats, only print what changed.
                check(w, multi, notifier, !beat);
                w.dirty = false;
                w.last_check = Some(now);
            }
        }
        if beat {
            heartbeat = now + settings.interval;
        }

        let Some(f) = &feed else {
            thread::sleep(heartbeat.saturating_duration_since(Instant::now()));
            continue;
        };
        // Sleep until the heartbeat, or until a changed position may be
        // re-checked, whichever is sooner; wake early on a new change.
        let wake = watched
            .iter()
            .filter(|w| w.dirty)
            .filter_map(|w| w.last_check.map(|t| t + settings.min_check))
            .fold(heartbeat, Instant::min);
        match f.changes.recv_timeout(wake.saturating_duration_since(Instant::now())) {
            Ok(first) => {
                for key in std::iter::once(first).chain(f.changes.try_iter()) {
                    for &i in f.positions.get(&key).into_iter().flatten() {
                        watched[i].dirty = true;
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                eprintln!("account stream stopped; checking on the timer only");
                feed = None;
            }
        }
    }
}

/// Snapshot one position, report it, alert on changes and maybe trade.
/// `quiet` prints the status line only when it changed. Returns false on error.
fn check(w: &mut Watched, multi: bool, notifier: &Notifier, quiet: bool) -> bool {
    let prefix = if multi { format!("[{}] ", w.label) } else { String::new() };
    let mode = w.mode();
    let verdict = w.venue.snapshot().and_then(|snap| {
        let params = w.controller.params(snap.liquidation_leverage());
        if !w.header_shown {
            print_header(&prefix, w.venue.name(), &snap, &params, mode);
            w.header_shown = true;
        }
        evaluate(&snap, &params)
    });
    let now = Utc::now().format("%H:%M:%S");
    let (status, line, action) = match verdict {
        Ok(v) => (v.status, v.line, Some(v.action)),
        Err(err) => ("error", format!("error: {err:#}"), None),
    };
    if quiet {
        w.rechecks += 1;
    }
    if !quiet || status != w.last_status {
        let rechecks = match (quiet, w.rechecks) {
            (false, n) if n > 0 => format!("  · {n} re-checks on updates"),
            _ => String::new(),
        };
        println!("{now}  {prefix}{line}{rechecks}");
    }
    if !quiet {
        w.rechecks = 0;
    }
    if should_alert(w.last_status, status) {
        notifier.send(&format!("[borrowguard {}] {prefix}{line}", w.venue.name()));
    }
    w.last_status = status;

    let traded = match (&mut w.auto, action) {
        (Some(auto), Some(action)) => auto.act(action),
        _ => None,
    };
    if let Some(Outcome { msg, alert, blocked }) = traded {
        // A held-back trade repeats on every check; between heartbeats, skip it.
        if !(quiet && blocked) {
            println!("{now}  {prefix}  ↳ {msg}");
        }
        if alert {
            notifier.send(&format!("[borrowguard {}] {prefix}{msg}", w.venue.name()));
        }
    }
    status != "error"
}

/// Alert when the verdict changes, but stay quiet when a watch starts healthy.
fn should_alert(last: &str, now: &str) -> bool {
    now != last && !(last.is_empty() && now == "hold")
}

fn print_header(prefix: &str, venue: &str, snap: &Snapshot, params: &Params, mode: &str) {
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
        "{prefix}controller  target {}× band ±{} ripcord {}× ({mode})",
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
    let decided = params.decide(&pos, price);

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
    Ok(Verdict { status, line, action: decided })
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

    struct Fake(std::cell::Cell<u32>);

    impl Execute for Fake {
        fn delever(&self, _: &ExecContext, _: f64, _: bool) -> Result<Executed> {
            self.0.set(self.0.get() + 1);
            Ok(Executed::Sent { signature: "sig".into(), summary: "sold".into() })
        }
        fn relever(&self, _: &ExecContext, _: f64) -> Result<Executed> {
            self.0.set(self.0.get() + 1);
            Ok(Executed::Sent { signature: "sig".into(), summary: "bought".into() })
        }
        fn close(&self, _: &ExecContext) -> Result<Executed> {
            unreachable!("the watch loop never closes positions")
        }
        fn static_accounts(
            &self,
            _: &borrowguard_kamino::Pubkey,
        ) -> Result<Vec<borrowguard_kamino::Pubkey>> {
            Ok(vec![])
        }
    }

    fn auto(kill_switch: PathBuf) -> AutoExec {
        let ctx = ExecContext {
            rpc: ExecContext::rpc("http://127.0.0.1:1"),
            signer: solana_sdk::signature::Keypair::new(),
            jupiter: borrowguard_exec::jupiter::Jupiter::new("http://127.0.0.1:1", None, None),
            slippage_bps: 50,
            ripcord_slippage_bps: 150,
            priority_fee: 0,
            lookup_tables: vec![],
            send: true,
        };
        AutoExec::new(
            Box::new(Fake(Default::default())),
            ctx,
            Duration::from_secs(120),
            2,
            kill_switch,
        )
    }

    #[test]
    fn trades_only_when_the_controller_wants_to() {
        let mut a = auto(PathBuf::from("/nonexistent/STOP"));
        assert_eq!(a.act(Action::Hold), None);
        assert_eq!(a.act(Action::Insolvent), None);
        let Outcome { msg, alert, .. } =
            a.act(Action::Delever { sell: 10.0, ripcord: false }).unwrap();
        assert!(msg.starts_with("TRADED") && alert, "{msg}");
    }

    #[test]
    fn cooldown_then_daily_cap() {
        let mut a = auto(PathBuf::from("/nonexistent/STOP"));
        let t0 = Instant::now();
        assert!(a.act(Action::Relever { borrow: 5.0 }).is_some());
        // Within the cooldown nothing happens, silently.
        assert_eq!(a.act(Action::Relever { borrow: 5.0 }), None);
        assert_eq!(a.gate(t0 + Duration::from_secs(60), false), Gate::Cooldown);
        // Ripcord waits only RIPCORD_COOLDOWN.
        assert_eq!(a.gate(t0 + Duration::from_secs(30), true), Gate::Go);
        // Two sends in the last day hit the cap of 2.
        a.sent.push_back(t0);
        assert_eq!(a.gate(t0 + Duration::from_secs(600), false), Gate::DailyCap);
        assert_eq!(a.gate(t0 + DAY + Duration::from_secs(1), false), Gate::Go);
    }

    #[test]
    fn kill_switch_pauses_everything() {
        let stop = std::env::temp_dir().join(format!("bg-stop-{}", std::process::id()));
        std::fs::write(&stop, "").unwrap();
        let mut a = auto(stop.clone());
        let Outcome { msg, alert, blocked } =
            a.act(Action::Delever { sell: 1.0, ripcord: true }).unwrap();
        assert!(msg.contains("paused") && !alert && blocked, "{msg}");
        std::fs::remove_file(stop).unwrap();
        assert!(a
            .act(Action::Delever { sell: 1.0, ripcord: true })
            .unwrap()
            .msg
            .starts_with("TRADED"));
    }
}
