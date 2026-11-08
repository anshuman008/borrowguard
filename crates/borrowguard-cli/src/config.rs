//! `borrowguard.toml`: the positions `watch` keeps an eye on.
//!
//! ```toml
//! interval_secs = 10
//!
//! [defaults]            # applies to every position unless overridden
//! payer = "<YOUR_WALLET>"
//! target = 3.0
//!
//! [[position]]
//! address = "<OBLIGATION_OR_POSITION>"
//! label = "sol loop"
//! band = 0.2
//! ```
//!
//! Unknown keys are rejected so a typo can't silently fall back to a default.
//!
//! Automatic rebalancing is opt-in per position (`execute = true`) and needs
//! an `[execution]` table; `watch` still only simulates unless run with `--live`.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use anyhow::{bail, ensure, Context, Result};
use borrowguard_kamino::Pubkey;
use serde::Deserialize;

use crate::monitor::Controller;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    interval_secs: Option<u64>,
    execution: Option<Execution>,
    #[serde(default)]
    defaults: Entry,
    #[serde(default, rename = "position")]
    positions: Vec<Entry>,
}

/// Shared shape of `[defaults]` and each `[[position]]`.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    address: Option<String>,
    label: Option<String>,
    payer: Option<String>,
    target: Option<f64>,
    band: Option<f64>,
    ripcord: Option<f64>,
    cost_bps: Option<f64>,
    ripcord_cost_bps: Option<f64>,
    max_trade: Option<f64>,
    min_trade: Option<f64>,
    execute: Option<bool>,
    lookup_table: Option<String>,
}

/// How automatic rebalances are signed and sent.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Execution {
    /// Keypair file of the positions' owner, kept outside the repo.
    pub keypair: PathBuf,
    #[serde(default = "Execution::default_slippage")]
    pub slippage_bps: u16,
    #[serde(default = "Execution::default_ripcord_slippage")]
    pub ripcord_slippage_bps: u16,
    /// Micro-lamports per compute unit.
    #[serde(default = "Execution::default_priority_fee")]
    pub priority_fee: u64,
    /// Minimum seconds between trades on one position.
    #[serde(default = "Execution::default_cooldown")]
    pub cooldown_secs: u64,
    /// Most trades one position may send in 24 hours.
    #[serde(default = "Execution::default_max_trades")]
    pub max_trades_per_day: usize,
    /// While this file exists, no trades are sent or simulated.
    #[serde(default = "Execution::default_kill_switch")]
    pub kill_switch: PathBuf,
    pub jupiter_url: Option<String>,
    pub jupiter_dexes: Option<String>,
}

impl Execution {
    fn default_slippage() -> u16 {
        50
    }
    fn default_ripcord_slippage() -> u16 {
        150
    }
    fn default_priority_fee() -> u64 {
        50_000
    }
    fn default_cooldown() -> u64 {
        120
    }
    fn default_max_trades() -> usize {
        24
    }
    fn default_kill_switch() -> PathBuf {
        PathBuf::from("STOP")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct WatchConfig {
    pub interval_secs: u64,
    pub execution: Option<Execution>,
    pub positions: Vec<PositionConfig>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PositionConfig {
    pub address: Pubkey,
    pub label: String,
    pub payer: Option<Pubkey>,
    pub controller: Controller,
    /// Rebalance automatically (still simulate-only without `watch --live`).
    pub execute: bool,
    /// Lookup table with the position's accounts; required to execute.
    pub lookup_table: Option<Pubkey>,
}

pub fn load(path: &Path) -> Result<WatchConfig> {
    let text = std::fs::read_to_string(path).with_context(|| {
        format!("read {} (copy borrowguard.example.toml to start)", path.display())
    })?;
    parse(&text).with_context(|| format!("in {}", path.display()))
}

pub fn parse(text: &str) -> Result<WatchConfig> {
    let file: File = toml::from_str(text)?;
    ensure!(file.defaults.address.is_none(), "[defaults] can't have an address");
    ensure!(!file.positions.is_empty(), "no [[position]] entries");

    let d = &file.defaults;
    let mut seen = HashSet::new();
    let positions = file
        .positions
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let Some(address) = &p.address else {
                bail!("position #{} has no address", i + 1);
            };
            let address: Pubkey =
                address.parse().with_context(|| format!("position #{}: bad address", i + 1))?;
            ensure!(seen.insert(address), "position {address} is listed twice");
            let payer = p
                .payer
                .as_ref()
                .or(d.payer.as_ref())
                .map(|k| k.parse().context("bad payer"))
                .transpose()?;
            let pick = |own: Option<f64>, shared: Option<f64>, default: f64| {
                own.or(shared).unwrap_or(default)
            };
            let base = Controller::DEFAULT;
            let controller = Controller {
                target: pick(p.target, d.target, base.target),
                band: pick(p.band, d.band, base.band),
                ripcord: pick(p.ripcord, d.ripcord, base.ripcord),
                cost_bps: pick(p.cost_bps, d.cost_bps, base.cost_bps),
                ripcord_cost_bps: pick(
                    p.ripcord_cost_bps,
                    d.ripcord_cost_bps,
                    base.ripcord_cost_bps,
                ),
                max_trade: p.max_trade.or(d.max_trade).or(base.max_trade),
                min_trade: pick(p.min_trade, d.min_trade, base.min_trade),
            };
            let label = p.label.clone().unwrap_or_else(|| short(&address));
            let execute = p.execute.or(d.execute).unwrap_or(false);
            let lookup_table = p
                .lookup_table
                .as_ref()
                .map(|k| k.parse().with_context(|| format!("{label}: bad lookup_table")))
                .transpose()?;
            if execute {
                ensure!(
                    file.execution.is_some(),
                    "{label}: execute = true needs an [execution] table with a keypair"
                );
                ensure!(
                    lookup_table.is_some(),
                    "{label}: execute = true needs a lookup_table (create one with `borrowguard lookup-table`)"
                );
            }
            Ok(PositionConfig { address, label, payer, controller, execute, lookup_table })
        })
        .collect::<Result<_>>()?;
    Ok(WatchConfig {
        interval_secs: file.interval_secs.unwrap_or(10),
        execution: file.execution,
        positions,
    })
}

/// `2w9MS6MB…TxAG`-style label for positions without one.
fn short(key: &Pubkey) -> String {
    let s = key.to_string();
    format!("{}…{}", &s[..4], &s[s.len() - 4..])
}

/// A `[[position]]` entry that guards `address` at `target` leverage.
pub fn entry(label: &str, address: &Pubkey, target: f64, lookup_table: Option<Pubkey>) -> String {
    let table = lookup_table.map_or(String::new(), |t| format!("lookup_table = \"{t}\"\n"));
    format!(
        "[[position]]\nlabel = \"{label}\"\naddress = \"{address}\"\ntarget = {target:?}\n{table}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printed_entries_parse() {
        let address: Pubkey = KAMINO.parse().unwrap();
        let table: Pubkey = JUPITER.parse().unwrap();
        let cfg = parse(&entry("SOL/USDC", &address, 1.5, Some(table))).unwrap();
        let [p] = &cfg.positions[..] else { panic!("one position") };
        assert_eq!((p.address, p.label.as_str()), (address, "SOL/USDC"));
        assert_eq!((p.controller.target, p.lookup_table), (1.5, Some(table)));
        assert_eq!(
            parse(&entry("x", &address, 2.0, None)).unwrap().positions[0].controller.target,
            2.0
        );
    }

    const KAMINO: &str = "2w9MS6MBFr68VpF6dkGwb2ucdKTpNFeXwmS79HSDTxAG";
    const JUPITER: &str = "9wG4DVpiJgbBu4chpRGZkKSoQQe9NcochTA3dAgs3eic";
    const WALLET: &str = "5jYbV6aAPgMVWy75UbJextaAbzmJjN6svKqbQKe9RbVF";

    #[test]
    fn merges_defaults_and_overrides() {
        let cfg = parse(&format!(
            r#"
            interval_secs = 5
            [defaults]
            payer = "{WALLET}"
            target = 3.5
            [[position]]
            address = "{KAMINO}"
            label = "jitosol loop"
            target = 12.0
            band = 0.4
            ripcord = 15.0
            [[position]]
            address = "{JUPITER}"
            "#
        ))
        .unwrap();
        assert_eq!(cfg.interval_secs, 5);
        let [a, b] = &cfg.positions[..] else { panic!("two positions") };
        assert_eq!(a.label, "jitosol loop");
        assert_eq!(
            (a.controller.target, a.controller.band, a.controller.ripcord),
            (12.0, 0.4, 15.0)
        );
        assert_eq!(b.label, "9wG4…3eic");
        assert_eq!(b.controller.target, 3.5);
        assert_eq!(b.controller.band, Controller::DEFAULT.band);
        assert_eq!(b.payer, Some(WALLET.parse().unwrap()));
    }

    #[test]
    fn rejects_typos_and_bad_entries() {
        let bad = [
            format!("[[position]]\naddress = \"{KAMINO}\"\ntraget = 3.0"),
            "interval_secs = 5".to_string(),
            "[[position]]\nlabel = \"no address\"".to_string(),
            "[[position]]\naddress = \"not-a-key\"".to_string(),
            format!("[[position]]\naddress = \"{KAMINO}\"\n[[position]]\naddress = \"{KAMINO}\""),
            format!("[defaults]\naddress = \"{KAMINO}\"\n[[position]]\naddress = \"{JUPITER}\""),
        ];
        for text in bad {
            assert!(parse(&text).is_err(), "should reject:\n{text}");
        }
    }

    #[test]
    fn execution_is_opt_in_and_validated() {
        let base = format!("[[position]]\naddress = \"{KAMINO}\"\n");
        assert!(!parse(&base).unwrap().positions[0].execute);

        let needs_table = format!("[execution]\nkeypair = \"k.json\"\n{base}execute = true\n");
        assert!(parse(&needs_table).unwrap_err().to_string().contains("lookup_table"));

        let needs_keypair = format!("{base}execute = true\nlookup_table = \"{JUPITER}\"\n");
        assert!(parse(&needs_keypair).unwrap_err().to_string().contains("[execution]"));

        let ok = format!(
            "[execution]\nkeypair = \"k.json\"\ncooldown_secs = 30\n{base}execute = true\nlookup_table = \"{JUPITER}\"\n"
        );
        let cfg = parse(&ok).unwrap();
        let exec = cfg.execution.unwrap();
        assert_eq!((exec.cooldown_secs, exec.slippage_bps, exec.max_trades_per_day), (30, 50, 24));
        assert_eq!(exec.kill_switch, PathBuf::from("STOP"));
        assert!(cfg.positions[0].execute);
    }

    #[test]
    fn example_file_parses() {
        let cfg = parse(include_str!("../../../borrowguard.example.toml")).unwrap();
        assert!(!cfg.positions.is_empty());
    }
}
