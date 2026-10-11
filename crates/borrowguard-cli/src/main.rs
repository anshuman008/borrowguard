mod backtest;
mod prices;

use std::path::PathBuf;

use anyhow::{ensure, Context, Result};
use borrowguard_core::{leverage_from_ltv, Params};
use chrono::{DateTime, Utc};
use clap::{Args, Parser, Subcommand};

use crate::backtest::{Report, SimConfig};

#[derive(Parser)]
#[command(name = "borrowguard", version, about = "Keep a lending position at a target leverage")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Download historical candles from a Binance-compatible API into a CSV.
    FetchPrices(FetchArgs),
    /// Replay the controller over historical candles and compare settings.
    Backtest(BacktestArgs),
}

/// Controller settings shared by `backtest` and `monitor`.
#[derive(Args)]
struct ControllerArgs {
    #[arg(long, default_value_t = 3.0)]
    target: f64,
    #[arg(long, default_value_t = 3.6)]
    ripcord: f64,
    /// Slippage + fees per routine trade, in basis points.
    #[arg(long, default_value_t = 30.0)]
    cost_bps: f64,
    /// Slippage + fees per ripcord trade, in basis points.
    #[arg(long, default_value_t = 100.0)]
    ripcord_cost_bps: f64,
    /// Max quote value per routine trade.
    #[arg(long)]
    max_trade: Option<f64>,
    /// Skip routine trades smaller than this (quote).
    #[arg(long, default_value_t = 1.0)]
    min_trade: f64,
}

impl ControllerArgs {
    fn params(&self, band: f64, liquidation: f64) -> Params {
        Params {
            target: self.target,
            band,
            ripcord: self.ripcord,
            liquidation,
            cost: self.cost_bps / 10_000.0,
            ripcord_cost: self.ripcord_cost_bps / 10_000.0,
            max_trade: self.max_trade,
            min_trade: self.min_trade,
        }
    }
}

#[derive(Args)]
struct FetchArgs {
    #[arg(long, default_value = "SOLUSDT")]
    symbol: String,
    /// Bar size, e.g. 1m, 5m, 1h. Shorter bars model a faster bot.
    #[arg(long, default_value = "1h")]
    interval: String,
    /// How many days back from now.
    #[arg(long, default_value_t = 365)]
    days: i64,
    #[arg(long)]
    out: PathBuf,
    /// Public market-data mirror; swap for https://api.binance.com if preferred.
    #[arg(long, default_value = "https://data-api.binance.vision")]
    base_url: String,
}

#[derive(Args)]
struct BacktestArgs {
    /// Candle CSV written by `fetch-prices`.
    #[arg(long)]
    prices: PathBuf,
    #[command(flatten)]
    controller: ControllerArgs,
    /// No-trade band half-width. Pass several (comma-separated) to compare.
    #[arg(long, value_delimiter = ',', default_value = "0.15")]
    band: Vec<f64>,
    /// Lender's liquidation LTV; 0.75 means liquidation at 4×.
    #[arg(long, default_value_t = 0.75)]
    liq_ltv: f64,
    /// Starting equity in quote (USD).
    #[arg(long, default_value_t = 1_000.0)]
    equity: f64,
    /// Borrow APR on debt, in percent.
    #[arg(long, default_value_t = 8.0)]
    borrow_apr: f64,
    /// Yield on collateral (e.g. LST staking), in percent.
    #[arg(long, default_value_t = 0.0)]
    supply_apr: f64,
    /// Liquidation bonus taken from collateral, in basis points of debt.
    #[arg(long, default_value_t = 500.0)]
    liq_bonus_bps: f64,
    /// Write every trade to this CSV (needs a single --band).
    #[arg(long)]
    trades: Option<PathBuf>,
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::FetchPrices(args) => fetch(args),
        Cmd::Backtest(args) => run_backtest(args),
    }
}

fn fetch(args: FetchArgs) -> Result<()> {
    let end = Utc::now().timestamp_millis();
    let start = end - args.days * 86_400_000;
    let candles = prices::fetch_binance(&args.base_url, &args.symbol, &args.interval, start, end)?;
    ensure!(!candles.is_empty(), "no candles returned for {}", args.symbol);
    prices::write_csv(&args.out, &candles)?;
    println!(
        "wrote {} {} candles ({} → {}) to {}",
        candles.len(),
        args.interval,
        date(candles[0].time),
        date(candles[candles.len() - 1].time),
        args.out.display()
    );
    Ok(())
}

fn run_backtest(args: BacktestArgs) -> Result<()> {
    ensure!(args.trades.is_none() || args.band.len() == 1, "--trades needs a single --band value");
    let candles = prices::load_csv(&args.prices)?;
    let (first, last) = (
        candles.first().context("price file is empty")?,
        candles.last().context("price file is empty")?,
    );
    let liquidation = leverage_from_ltv(args.liq_ltv);
    let underlying = last.close / first.close - 1.0;
    let target = args.controller.target;

    println!("period      {} → {} ({} bars)", date(first.time), date(last.time), candles.len());
    println!(
        "price       {:.2} → {:.2} ({:+.1}%), naive {}× = {:+.1}%",
        first.close,
        last.close,
        underlying * 100.0,
        target,
        underlying * target * 100.0
    );
    println!(
        "controller  target {}×, ripcord {}×, liquidation {:.2}×, cost {} bps",
        target, args.controller.ripcord, liquidation, args.controller.cost_bps
    );
    println!();
    println!(
        "{:<16} {:>12} {:>9} {:>8} {:>8} {:>8} {:>8} {:>10} {:>10}  liquidated",
        "strategy",
        "equity",
        "return",
        "max lev",
        "delever",
        "relever",
        "ripcord",
        "costs",
        "interest"
    );

    let base = |band: f64, managed: bool| -> Result<SimConfig> {
        let params = args.controller.params(band, liquidation);
        params.validate()?;
        Ok(SimConfig {
            params,
            managed,
            equity: args.equity,
            borrow_apr: args.borrow_apr / 100.0,
            supply_apr: args.supply_apr / 100.0,
            liq_bonus: args.liq_bonus_bps / 10_000.0,
        })
    };

    let unmanaged = backtest::run(&base(args.band[0], false)?, &candles)?;
    print_row("unmanaged", &unmanaged, args.equity);
    for &band in &args.band {
        let report = backtest::run(&base(band, true)?, &candles)?;
        print_row(&format!("band ±{band}"), &report, args.equity);
        if let Some(path) = &args.trades {
            let mut writer = csv::Writer::from_path(path)
                .with_context(|| format!("create {}", path.display()))?;
            for trade in &report.trades {
                writer.serialize(trade)?;
            }
            writer.flush()?;
        }
    }
    Ok(())
}

fn print_row(name: &str, r: &Report, start_equity: f64) {
    println!(
        "{:<16} {:>12.2} {:>8.1}% {:>7.2}× {:>8} {:>8} {:>8} {:>10.2} {:>10.2}  {}",
        name,
        r.final_equity,
        (r.final_equity / start_equity - 1.0) * 100.0,
        r.max_leverage,
        r.count("delever"),
        r.count("relever"),
        r.count("ripcord"),
        r.costs,
        r.interest,
        r.liquidated_at.map_or_else(|| "no".to_string(), date)
    );
}

fn date(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .map_or_else(|| ms.to_string(), |t| t.format("%Y-%m-%d %H:%M").to_string())
}
