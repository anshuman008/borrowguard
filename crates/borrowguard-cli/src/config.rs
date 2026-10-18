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

use std::{collections::HashSet, path::Path};

use anyhow::{bail, ensure, Context, Result};
use borrowguard_kamino::Pubkey;
use serde::Deserialize;

use crate::monitor::Controller;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    interval_secs: Option<u64>,
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
}

#[derive(Debug, Clone, PartialEq)]
pub struct WatchConfig {
    pub interval_secs: u64,
    pub positions: Vec<PositionConfig>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PositionConfig {
    pub address: Pubkey,
    pub label: String,
    pub payer: Option<Pubkey>,
    pub controller: Controller,
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
            Ok(PositionConfig { address, label, payer, controller })
        })
        .collect::<Result<_>>()?;
    Ok(WatchConfig { interval_secs: file.interval_secs.unwrap_or(10), positions })
}

/// `2w9MS6MB…TxAG`-style label for positions without one.
fn short(key: &Pubkey) -> String {
    let s = key.to_string();
    format!("{}…{}", &s[..4], &s[s.len() - 4..])
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn example_file_parses() {
        let cfg = parse(include_str!("../../../borrowguard.example.toml")).unwrap();
        assert!(!cfg.positions.is_empty());
    }
}
