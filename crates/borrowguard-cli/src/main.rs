mod alerts;
mod backtest;
mod config;
mod monitor;
mod prices;
mod stream;

use std::{path::PathBuf, time::Duration};

use anyhow::{ensure, Context, Result};
use borrowguard_core::Venue;
use borrowguard_core::{leverage_from_ltv, Action, Params};
use borrowguard_exec::{jupiter::Jupiter, load_keypair, ExecContext, Execute, Executed, Opened};
use borrowguard_jupiter::JupiterVenue;
use borrowguard_kamino::{KaminoVenue, Pubkey};
use borrowguard_marginfi::MarginfiVenue;
use chrono::{DateTime, Utc};
use clap::{Args, Parser, Subcommand};
use solana_client::rpc_client::RpcClient;

use crate::alerts::Notifier;
use crate::backtest::{Report, SimConfig};
use crate::monitor::{AutoExec, Controller, Feed, Watched};
use crate::stream::Source;

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
    /// List a wallet's Kamino, Project 0 (marginfi) and Jupiter Lend positions.
    Positions(PositionsArgs),
    /// Watch a live Kamino, Project 0 (marginfi) or Jupiter Lend position and
    /// report what the
    /// controller would do.
    /// Read-only: never sends a transaction.
    Monitor(MonitorArgs),
    /// Watch every position listed in a config file (see borrowguard.example.toml).
    /// Read-only: never sends a transaction.
    Watch(WatchArgs),
    /// Rebalance a Kamino or Jupiter Lend position now, in whichever direction the controller
    /// decides (delever or relever), funded by a flash loan. Simulates only
    /// unless --send is given.
    #[command(alias = "delever")]
    Rebalance(RebalanceArgs),
    /// Create an address lookup table with a position's accounts (one-time,
    /// ~0.004 SOL rent). Rebalances need it to fit a swap route.
    LookupTable(LookupTableArgs),
    /// Open a leveraged Kamino or Jupiter Lend position from the wallet in one
    /// flash-loan transaction (plus one-time setup and a lookup table).
    /// Simulates only unless --send is given.
    Open(OpenArgs),
    /// Close a Kamino or Jupiter Lend position in one flash-loan transaction:
    /// repay all debt with part of the collateral, withdraw the rest to the
    /// wallet. Simulates only unless --send is given.
    Close(CloseArgs),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum OpenVenue {
    Kamino,
    Jupiter,
}

#[derive(Args)]
struct OpenArgs {
    #[arg(long, value_enum)]
    venue: OpenVenue,
    /// Kamino lending market.
    #[arg(long, default_value_t = borrowguard_kamino::MAIN_MARKET)]
    market: Pubkey,
    /// Kamino collateral token: a symbol (SOL, JITOSOL, CBBTC, …) or a mint.
    #[arg(long, required_if_eq("venue", "kamino"))]
    collateral: Option<String>,
    /// Kamino debt token: a symbol (USDC, SOL, …) or a mint.
    #[arg(long, required_if_eq("venue", "kamino"))]
    debt: Option<String>,
    /// Jupiter Lend vault id; the vault fixes the pair.
    #[arg(long, required_if_eq("venue", "jupiter"))]
    vault: Option<u16>,
    /// Collateral tokens to put in from the wallet.
    #[arg(long)]
    deposit: f64,
    /// Leverage to open at, e.g. 2 for 2×.
    #[arg(long)]
    leverage: f64,
    /// Owner's Solana keypair file (JSON), kept outside the repo.
    #[arg(long, env = "BORROWGUARD_KEYPAIR")]
    keypair: PathBuf,
    #[command(flatten)]
    rpc: RpcArgs,
    #[command(flatten)]
    exec: ExecArgs,
}

#[derive(Args)]
struct CloseArgs {
    /// Kamino obligation or Jupiter Lend position to close; the keypair must own it.
    #[arg(long, alias = "obligation")]
    position: Pubkey,
    /// Owner's Solana keypair file (JSON), kept outside the repo.
    #[arg(long, env = "BORROWGUARD_KEYPAIR")]
    keypair: PathBuf,
    #[command(flatten)]
    rpc: RpcArgs,
    #[command(flatten)]
    exec: ExecArgs,
}

#[derive(Args)]
struct LookupTableArgs {
    #[arg(long, alias = "obligation")]
    position: Pubkey,
    #[arg(long, env = "BORROWGUARD_KEYPAIR")]
    keypair: PathBuf,
    #[command(flatten)]
    rpc: RpcArgs,
    #[arg(long, default_value_t = 50_000)]
    priority_fee: u64,
}

/// Controller settings shared by `backtest` and `monitor`.
#[derive(Args)]
struct ControllerArgs {
    #[arg(long, default_value_t = Controller::DEFAULT.target)]
    target: f64,
    #[arg(long, default_value_t = Controller::DEFAULT.ripcord)]
    ripcord: f64,
    /// Slippage + fees per routine trade, in basis points.
    #[arg(long, default_value_t = Controller::DEFAULT.cost_bps)]
    cost_bps: f64,
    /// Slippage + fees per ripcord trade, in basis points.
    #[arg(long, default_value_t = Controller::DEFAULT.ripcord_cost_bps)]
    ripcord_cost_bps: f64,
    /// Max quote value per routine trade.
    #[arg(long)]
    max_trade: Option<f64>,
    /// Skip routine trades smaller than this (quote).
    #[arg(long, default_value_t = Controller::DEFAULT.min_trade)]
    min_trade: f64,
}

impl ControllerArgs {
    fn controller(&self, band: f64) -> Controller {
        Controller {
            target: self.target,
            band,
            ripcord: self.ripcord,
            cost_bps: self.cost_bps,
            ripcord_cost_bps: self.ripcord_cost_bps,
            max_trade: self.max_trade,
            min_trade: self.min_trade,
        }
    }

    fn params(&self, band: f64, liquidation: f64) -> Params {
        self.controller(band).params(liquidation)
    }
}

/// Telegram / Discord alert credentials, usually from `.env`.
#[derive(Args)]
struct AlertArgs {
    #[arg(long, env = "TELEGRAM_BOT_TOKEN", hide_env_values = true)]
    telegram_bot_token: Option<String>,
    #[arg(long, env = "TELEGRAM_CHAT_ID")]
    telegram_chat_id: Option<String>,
    #[arg(long, env = "DISCORD_WEBHOOK_URL", hide_env_values = true)]
    discord_webhook_url: Option<String>,
}

impl AlertArgs {
    fn notifier(self) -> Notifier {
        Notifier::new(self.telegram_bot_token, self.telegram_chat_id, self.discord_webhook_url)
    }
}

/// How `monitor` / `watch` hear about price moves between timer checks.
#[derive(Args)]
struct StreamArgs {
    /// poll: the timer only (any RPC). websocket: re-check when a price feed
    /// or the position changes (most RPCs, free tiers too). grpc: the same
    /// over a Yellowstone gRPC stream, fastest (needs `--features grpc`).
    #[arg(long, env = "BORROWGUARD_STREAM", value_enum, default_value = "poll")]
    stream: stream::Kind,
    /// Websocket endpoint; defaults to the RPC URL with ws(s)://.
    #[arg(long, env = "WS_URL")]
    ws_url: Option<String>,
    /// Yellowstone gRPC endpoint, e.g. a Helius LaserStream or Triton URL.
    #[arg(long, env = "GRPC_URL")]
    grpc_url: Option<String>,
    #[arg(long, env = "GRPC_TOKEN", hide_env_values = true)]
    grpc_token: Option<String>,
    /// Least milliseconds between two stream-triggered checks of one position.
    #[arg(long, default_value_t = 1_000)]
    min_check_ms: u64,
}

impl StreamArgs {
    fn source(&self, rpc_url: &str) -> Result<Source> {
        Ok(match self.stream {
            stream::Kind::Poll => Source::Poll,
            stream::Kind::Websocket => Source::Websocket {
                url: self.ws_url.clone().unwrap_or_else(|| stream::websocket_url(rpc_url)),
            },
            stream::Kind::Grpc => Source::Grpc {
                url: self
                    .grpc_url
                    .clone()
                    .context("--stream grpc needs --grpc-url (or GRPC_URL)")?,
                token: self.grpc_token.clone(),
            },
        })
    }
}

/// Subscribe to the accounts every watched position depends on.
fn start_feed(source: &Source, watched: &[Watched]) -> Result<Option<Feed>> {
    if matches!(source, Source::Poll) {
        return Ok(None);
    }
    let mut positions: std::collections::HashMap<Pubkey, Vec<usize>> = Default::default();
    for (i, w) in watched.iter().enumerate() {
        match w.venue.watch_accounts() {
            Ok(keys) => {
                for key in keys {
                    positions.entry(Pubkey::new_from_array(key)).or_default().push(i);
                }
            }
            Err(err) => {
                eprintln!("[{}] can't list accounts to stream ({err:#}); timer only", w.label)
            }
        }
    }
    let accounts = positions.keys().copied().collect();
    Ok(stream::start(source, accounts)?.map(|changes| Feed { changes, positions }))
}

/// "every 10s" or "every 10s + websocket updates", for the startup line.
fn cadence(interval: u64, source: &Source, feed: &Option<Feed>) -> String {
    match feed {
        Some(f) => format!(
            "every {interval}s + {} updates on {} accounts",
            source.name(),
            f.positions.len()
        ),
        None => format!("every {interval}s"),
    }
}

#[derive(Args)]
struct RpcArgs {
    /// Solana RPC endpoint. `positions` needs one that allows getProgramAccounts.
    #[arg(long, env = "RPC_URL", default_value = "https://api.mainnet-beta.solana.com")]
    rpc_url: String,
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

#[derive(Args)]
struct PositionsArgs {
    /// Wallet that owns the positions.
    #[arg(long)]
    owner: Pubkey,
    #[command(flatten)]
    rpc: RpcArgs,
}

#[derive(Args)]
struct MonitorArgs {
    /// Kamino obligation, marginfi account or Jupiter Lend position to watch
    /// (find it with `borrowguard positions`). The venue is detected from the
    /// account.
    #[arg(long, alias = "obligation")]
    position: Pubkey,
    #[command(flatten)]
    rpc: RpcArgs,
    #[command(flatten)]
    controller: ControllerArgs,
    /// No-trade band half-width around the target.
    #[arg(long, default_value_t = Controller::DEFAULT.band)]
    band: f64,
    /// Seconds between checks.
    #[arg(long, default_value_t = 10)]
    interval: u64,
    /// Check once and exit (non-zero on error).
    #[arg(long)]
    once: bool,
    /// Fee payer for read-only simulations; defaults to the position owner.
    #[arg(long)]
    payer: Option<Pubkey>,
    #[command(flatten)]
    alerts: AlertArgs,
    #[command(flatten)]
    stream: StreamArgs,
}

#[derive(Args)]
struct WatchArgs {
    /// Positions to watch, with per-position settings.
    #[arg(long, default_value = "borrowguard.toml")]
    config: PathBuf,
    #[command(flatten)]
    rpc: RpcArgs,
    /// Seconds between rounds; overrides `interval_secs` in the config.
    #[arg(long)]
    interval: Option<u64>,
    /// Check every position once and exit (non-zero if any failed).
    #[arg(long)]
    once: bool,
    /// Send rebalances for positions with `execute = true`. Without it they
    /// are only simulated.
    #[arg(long)]
    live: bool,
    #[command(flatten)]
    alerts: AlertArgs,
    #[command(flatten)]
    stream: StreamArgs,
}

#[derive(Args)]
struct RebalanceArgs {
    /// Kamino obligation or Jupiter Lend position to rebalance; the keypair
    /// must own it.
    #[arg(long, alias = "obligation")]
    position: Pubkey,
    /// Owner's Solana keypair file (JSON), kept outside the repo.
    #[arg(long, env = "BORROWGUARD_KEYPAIR")]
    keypair: PathBuf,
    #[command(flatten)]
    rpc: RpcArgs,
    /// Force a delever: debt-token value of collateral to sell.
    #[arg(long, conflicts_with = "borrow")]
    sell: Option<f64>,
    /// Force a relever: debt tokens to borrow and turn into collateral.
    /// Without --sell or --borrow, the controller decides (and does nothing
    /// if the position is inside its band).
    #[arg(long)]
    borrow: Option<f64>,
    #[command(flatten)]
    controller: ControllerArgs,
    #[arg(long, default_value_t = Controller::DEFAULT.band)]
    band: f64,
    #[command(flatten)]
    exec: ExecArgs,
}

/// How trades are executed.
#[derive(Args)]
struct ExecArgs {
    /// Actually send the transaction. Without it, it is only simulated.
    #[arg(long)]
    send: bool,
    /// Max swap slippage for routine trades, basis points.
    #[arg(long, default_value_t = 50)]
    slippage_bps: u16,
    /// Max swap slippage for ripcord trades, basis points.
    #[arg(long, default_value_t = 150)]
    ripcord_slippage_bps: u16,
    /// Priority fee, micro-lamports per compute unit.
    #[arg(long, default_value_t = 50_000)]
    priority_fee: u64,
    /// Extra address lookup table(s) holding the position's accounts.
    #[arg(long = "lookup-table")]
    lookup_tables: Vec<Pubkey>,
    #[arg(long, env = "JUPITER_URL", default_value = borrowguard_exec::jupiter::DEFAULT_URL)]
    jupiter_url: String,
    #[arg(long, env = "JUPITER_API_KEY", hide_env_values = true)]
    jupiter_api_key: Option<String>,
    /// Only swap through these DEXes, e.g. "Whirlpool,Raydium CLMM,Meteora DLMM".
    /// Default: any, for the best price.
    #[arg(long)]
    jupiter_dexes: Option<String>,
}

impl ExecArgs {
    fn context(self, rpc_url: &str, signer: solana_sdk::signature::Keypair) -> ExecContext {
        ExecContext {
            rpc: ExecContext::rpc(rpc_url),
            signer,
            jupiter: Jupiter::new(&self.jupiter_url, self.jupiter_api_key, self.jupiter_dexes),
            slippage_bps: self.slippage_bps,
            ripcord_slippage_bps: self.ripcord_slippage_bps,
            priority_fee: self.priority_fee,
            lookup_tables: self.lookup_tables,
            send: self.send,
        }
    }
}

fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    match Cli::parse().cmd {
        Cmd::FetchPrices(args) => fetch(args),
        Cmd::Backtest(args) => run_backtest(args),
        Cmd::Positions(args) => positions(args),
        Cmd::Monitor(args) => run_monitor(args),
        Cmd::Watch(args) => run_watch(args),
        Cmd::Rebalance(args) => run_rebalance(args),
        Cmd::LookupTable(args) => run_lookup_table(args),
        Cmd::Open(args) => run_open(args),
        Cmd::Close(args) => run_close(args),
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

fn positions(args: PositionsArgs) -> Result<()> {
    let rpc_url = &args.rpc.rpc_url;
    match borrowguard_kamino::find_obligations(rpc_url, &args.owner) {
        Ok(obligations) => print_kamino(&obligations),
        Err(err) => println!("kamino: lookup failed: {err:#}\n"),
    }
    let marginfi = borrowguard_marginfi::find_accounts(rpc_url, &args.owner)
        .and_then(|accounts| print_marginfi(rpc_url, &accounts));
    if let Err(err) = marginfi {
        println!("marginfi: lookup failed: {err:#}\n");
    }
    match borrowguard_jupiter::find_positions(rpc_url, &args.owner) {
        Ok(positions) => print_jupiter(&positions),
        Err(err) => println!("jupiter: lookup failed: {err:#}"),
    }
    Ok(())
}

fn print_kamino(obligations: &[(Pubkey, borrowguard_kamino::layout::Obligation)]) {
    if obligations.is_empty() {
        println!("kamino: no obligations\n");
        return;
    }
    println!("kamino (values as of each obligation's last refresh; `monitor` shows live ones)");
    println!(
        "  {:<45} {:<9} {:>5} {:>5} {:>14} {:>7} {:>7}  guardable",
        "position", "kind", "coll", "debt", "collateral $", "LTV", "liq LTV"
    );
    for (key, o) in obligations {
        let deposited = o.deposited_value_sf.to_f64();
        let ratio = |v: f64| {
            if deposited > 0.0 {
                format!("{:.1}%", v / deposited * 100.0)
            } else {
                "-".into()
            }
        };
        let (deposits, borrows) = (o.active_deposits().count(), o.active_borrows().count());
        println!(
            "  {:<45} {:<9} {:>5} {:>5} {:>14.2} {:>7} {:>7}  {}",
            key.to_string(),
            o.kind(),
            deposits,
            borrows,
            deposited,
            ratio(o.borrow_factor_adjusted_debt_value_sf.to_f64()),
            ratio(o.unhealthy_borrow_value_sf.to_f64()),
            if deposits == 1 && borrows == 1 { "yes" } else { "no" },
        );
    }
    println!();
}

fn print_marginfi(
    rpc_url: &str,
    accounts: &[(Pubkey, borrowguard_marginfi::layout::MarginfiAccount)],
) -> Result<()> {
    if accounts.is_empty() {
        println!("marginfi: no accounts\n");
        return Ok(());
    }
    let mut keys: Vec<Pubkey> =
        accounts.iter().flat_map(|(_, a)| a.balances.iter().map(|b| b.bank)).collect();
    keys.sort();
    keys.dedup();
    let banks: std::collections::HashMap<_, _> =
        borrowguard_marginfi::fetch_banks(rpc_url, &keys)?.into_iter().collect();
    let symbol = |bank: &Pubkey| {
        banks
            .get(bank)
            .map_or("?".to_string(), |b| borrowguard_core::token::symbol(&b.mint.to_string()))
    };
    println!("marginfi / Project 0 (liq LTV from bank weights; `monitor` shows live values)");
    println!(
        "  {:<45} {:>5} {:>5} {:<18} {:>8}  guardable",
        "position", "coll", "debt", "pair", "liq LTV"
    );
    for (key, a) in accounts {
        let deposits: Vec<_> = a.deposits().collect();
        let borrows: Vec<_> = a.borrows().collect();
        let (pair, liq) = match (deposits.as_slice(), borrows.as_slice()) {
            ([d], [b]) => {
                let liq = match (banks.get(&d.bank), banks.get(&b.bank)) {
                    (Some(c), Some(l)) => {
                        format!("{:.1}%", c.asset_weight_maint / l.liability_weight_maint * 100.0)
                    }
                    _ => "-".into(),
                };
                (format!("{}/{}", symbol(&d.bank), symbol(&b.bank)), liq)
            }
            _ => ("-".into(), "-".into()),
        };
        println!(
            "  {:<45} {:>5} {:>5} {:<18} {:>8}  {}",
            key.to_string(),
            deposits.len(),
            borrows.len(),
            pair,
            liq,
            if deposits.len() == 1 && borrows.len() == 1 { "yes" } else { "no" },
        );
    }
    println!();
    Ok(())
}

fn print_jupiter(positions: &[borrowguard_jupiter::FoundPosition]) {
    if positions.is_empty() {
        println!("jupiter: no positions");
        return;
    }
    println!("jupiter (`monitor` shows live amounts)");
    println!(
        "  {:<45} {:>6} {:>8} {:<14} {:>8}  guardable",
        "position", "vault", "nft", "pair", "liq LTV"
    );
    for p in positions {
        println!(
            "  {:<45} {:>6} {:>8} {:<14} {:>7.1}%  {}",
            p.key.to_string(),
            p.position.vault_id,
            p.position.nft_id,
            p.pair,
            p.liquidation_ltv * 100.0,
            if p.position.supply_only { "no (no debt)" } else { "yes" },
        );
    }
    println!("\n  watch one with: borrowguard monitor --position <POSITION> --payer <YOUR_WALLET>");
}

fn open_venue(rpc_url: &str, position: Pubkey, payer: Option<Pubkey>) -> Result<Box<dyn Venue>> {
    let rpc = RpcClient::new(rpc_url.to_string());
    let account = rpc
        .get_account(&position)
        .with_context(|| format!("fetch {position} (closed, or not a mainnet address?)"))?;
    if account.owner == borrowguard_kamino::PROGRAM_ID {
        Ok(Box::new(KaminoVenue::new(rpc_url, position, payer)))
    } else if account.owner == borrowguard_jupiter::VAULTS_PROGRAM_ID {
        Ok(Box::new(JupiterVenue::new(rpc_url, position, payer)))
    } else if account.owner == borrowguard_marginfi::PROGRAM_ID {
        Ok(Box::new(MarginfiVenue::new(rpc_url, position, payer)))
    } else {
        anyhow::bail!(
            "{position} is owned by {}, not Kamino Lend, Jupiter Lend or marginfi",
            account.owner
        )
    }
}

/// A venue BorrowGuard can rebalance on, not just watch.
trait Rebalancer: Venue + Execute {}
impl<T: Venue + Execute> Rebalancer for T {}

/// Open a position for rebalancing by `owner`, checking that `owner` controls it.
fn open_rebalancer(rpc_url: &str, position: Pubkey, owner: Pubkey) -> Result<Box<dyn Rebalancer>> {
    let account = RpcClient::new(rpc_url.to_string())
        .get_account(&position)
        .with_context(|| format!("fetch {position} (closed, or not a mainnet address?)"))?;
    let (venue, position_owner): (Box<dyn Rebalancer>, Pubkey) = if account.owner
        == borrowguard_kamino::PROGRAM_ID
    {
        let kamino = KaminoVenue::new(rpc_url, position, Some(owner));
        let position_owner = kamino.owner()?;
        (Box::new(kamino), position_owner)
    } else if account.owner == borrowguard_jupiter::VAULTS_PROGRAM_ID {
        let jupiter = JupiterVenue::new(rpc_url, position, Some(owner));
        let position_owner = jupiter.owner()?;
        (Box::new(jupiter), position_owner)
    } else {
        anyhow::bail!("{position}: rebalancing works on Kamino and Jupiter Lend positions so far")
    };
    ensure!(
        position_owner == owner,
        "keypair {owner} doesn't own {position} (owner {position_owner})"
    );
    Ok(venue)
}

fn run_monitor(args: MonitorArgs) -> Result<()> {
    let venue = open_venue(&args.rpc.rpc_url, args.position, args.payer)?;
    let notifier = args.alerts.notifier();
    let (name, label) = (venue.name().to_string(), args.position.to_string());
    let mut watched = [Watched::new(label, venue, args.controller.controller(args.band))];
    let source = args.stream.source(&args.rpc.rpc_url)?;
    let feed = if args.once { None } else { start_feed(&source, &watched)? };
    println!(
        "watching {name} position {} {}, alerts: {}",
        args.position,
        cadence(args.interval, &source, &feed),
        channels(&notifier)
    );
    let settings = monitor::Settings {
        interval: Duration::from_secs(args.interval),
        once: args.once,
        min_check: Duration::from_millis(args.stream.min_check_ms),
    };
    monitor::run(&mut watched, &notifier, settings, feed)
}

fn run_watch(args: WatchArgs) -> Result<()> {
    let cfg = config::load(&args.config)?;
    let rpc_url = &args.rpc.rpc_url;
    let signer = match (&cfg.execution, cfg.positions.iter().any(|p| p.execute)) {
        (Some(exec), true) => Some(load_keypair(&exec.keypair)?),
        _ => None,
    };
    let mut watched = Vec::with_capacity(cfg.positions.len());
    for p in cfg.positions {
        let venue = open_venue(rpc_url, p.address, p.payer)
            .with_context(|| format!("position \"{}\"", p.label))?;
        println!("  {:<24} {:<8} {}", p.label, venue.name(), p.address);
        let mut entry = Watched::new(p.label.clone(), venue, p.controller);
        if p.execute {
            let (exec, signer) = (cfg.execution.as_ref().unwrap(), signer.as_ref().unwrap());
            entry = entry.with_auto(auto_exec(rpc_url, &p, exec, signer, args.live)?);
        }
        watched.push(entry);
    }
    let interval = args.interval.unwrap_or(cfg.interval_secs);
    let notifier = args.alerts.notifier();
    let trading = watched.iter().filter(|w| w.auto.is_some()).count();
    let source = args.stream.source(rpc_url)?;
    let feed = if args.once { None } else { start_feed(&source, &watched)? };
    println!(
        "watching {} positions {}, alerts: {}",
        watched.len(),
        cadence(interval, &source, &feed),
        channels(&notifier)
    );
    if trading > 0 {
        match args.live {
            true => {
                println!("LIVE: auto-rebalance sends real transactions for {trading} position(s)")
            }
            false => {
                println!("auto-rebalance simulated for {trading} position(s); add --live to send")
            }
        }
    }
    println!();
    let settings = monitor::Settings {
        interval: Duration::from_secs(interval),
        once: args.once,
        min_check: Duration::from_millis(args.stream.min_check_ms),
    };
    monitor::run(&mut watched, &notifier, settings, feed)
}

/// Executor for one `execute = true` position, checked before the loop starts
/// so a misconfiguration fails now rather than mid-crash.
fn auto_exec(
    rpc_url: &str,
    p: &config::PositionConfig,
    exec: &config::Execution,
    signer: &solana_sdk::signature::Keypair,
    live: bool,
) -> Result<AutoExec> {
    let owner = solana_sdk::signer::Signer::pubkey(signer);
    let venue = open_rebalancer(rpc_url, p.address, owner).with_context(|| p.label.clone())?;
    let ctx = ExecContext {
        rpc: ExecContext::rpc(rpc_url),
        signer: solana_sdk::signature::Keypair::from_bytes(&signer.to_bytes())?,
        jupiter: Jupiter::new(
            exec.jupiter_url.as_deref().unwrap_or(borrowguard_exec::jupiter::DEFAULT_URL),
            std::env::var("JUPITER_API_KEY").ok(),
            exec.jupiter_dexes.clone(),
        ),
        slippage_bps: exec.slippage_bps,
        ripcord_slippage_bps: exec.ripcord_slippage_bps,
        priority_fee: exec.priority_fee,
        lookup_tables: p.lookup_table.into_iter().collect(),
        send: live,
    };
    Ok(AutoExec::new(
        venue,
        ctx,
        Duration::from_secs(exec.cooldown_secs),
        exec.max_trades_per_day,
        exec.kill_switch.clone(),
    ))
}

fn run_rebalance(args: RebalanceArgs) -> Result<()> {
    let signer = load_keypair(&args.keypair)?;
    let owner = solana_sdk::signer::Signer::pubkey(&signer);
    let rpc_url = &args.rpc.rpc_url;
    let venue = open_rebalancer(rpc_url, args.position, owner)?;

    let snap = venue.snapshot()?;
    let params = args.controller.params(args.band, snap.liquidation_leverage());
    params.validate().with_context(|| {
        format!("controller settings vs venue liquidation at {:.2}×", snap.liquidation_leverage())
    })?;
    let action = match (args.sell, args.borrow) {
        (Some(sell), _) => Action::Delever { sell, ripcord: false },
        (_, Some(borrow)) => Action::Relever { borrow },
        _ => params.decide(&snap.position(), snap.price()),
    };
    let leverage = snap.position().leverage(snap.price()).unwrap_or(f64::INFINITY);
    let (coll, debt) = (&snap.collateral_symbol, &snap.debt_symbol);
    let plan = match action {
        Action::Delever { sell, ripcord } => {
            format!(
                "{}delever {sell:.4} {debt} worth of {coll}",
                if ripcord { "RIPCORD " } else { "" }
            )
        }
        Action::Relever { borrow } => format!("relever: borrow {borrow:.4} {debt}, buy {coll}"),
        other => {
            println!("{coll}/{debt} at {leverage:.3}×: controller says {other:?}; nothing to do");
            return Ok(());
        }
    };
    println!(
        "{coll}/{debt} at {leverage:.3}×: {plan}{}",
        if args.exec.send { "" } else { " (simulation only; add --send to execute)" }
    );
    let ctx = args.exec.context(rpc_url, signer);
    let result = match action {
        Action::Delever { sell, ripcord } => venue.delever(&ctx, sell, ripcord)?,
        Action::Relever { borrow } => venue.relever(&ctx, borrow)?,
        _ => unreachable!("handled above"),
    };
    match result {
        Executed::Simulated { summary, .. } => println!("simulated ok: {summary}"),
        Executed::Sent { signature, summary } => {
            println!("sent: {summary}\nsignature: {signature}");
            let after = venue.snapshot()?;
            let lev = after.position().leverage(after.price()).unwrap_or(f64::INFINITY);
            println!("leverage now {lev:.3}× (was {leverage:.3}×)");
        }
    }
    Ok(())
}

fn run_lookup_table(args: LookupTableArgs) -> Result<()> {
    let signer = load_keypair(&args.keypair)?;
    let owner = solana_sdk::signer::Signer::pubkey(&signer);
    let venue = open_rebalancer(&args.rpc.rpc_url, args.position, owner)?;
    let addresses = venue.static_accounts(&owner)?;
    println!("creating a lookup table with {} accounts for {}", addresses.len(), args.position);
    let rpc = ExecContext::rpc(&args.rpc.rpc_url);
    let table =
        borrowguard_exec::tx::create_lookup_table(&rpc, &signer, &addresses, args.priority_fee)?;
    println!("lookup table: {table}\nuse it with: --lookup-table {table}");
    Ok(())
}

/// A token given as a symbol (SOL, USDC, …) or a mint address.
fn token_mint(token: &str) -> Result<Pubkey> {
    if let Ok(mint) = token.parse() {
        return Ok(mint);
    }
    borrowguard_core::token::mint(token)
        .with_context(|| format!("unknown token {token}; pass its mint address"))?
        .parse()
        .map_err(Into::into)
}

fn run_open(args: OpenArgs) -> Result<()> {
    let signer = load_keypair(&args.keypair)?;
    let owner = solana_sdk::signer::Signer::pubkey(&signer);
    let rpc_url = &args.rpc.rpc_url;
    let send = args.exec.send;
    println!(
        "opening a {:?} position: deposit {}, leverage {}×{}",
        args.venue,
        args.deposit,
        args.leverage,
        if send { "" } else { " (simulation only; add --send to execute)" }
    );
    let ctx = args.exec.context(rpc_url, signer);
    let opened: Opened = match args.venue {
        OpenVenue::Kamino => borrowguard_kamino::open::open(
            &ctx,
            &borrowguard_kamino::open::OpenRequest {
                market: args.market,
                collateral_mint: token_mint(args.collateral.as_deref().unwrap_or_default())?,
                debt_mint: token_mint(args.debt.as_deref().unwrap_or_default())?,
                deposit: args.deposit,
                leverage: args.leverage,
            },
        )?,
        OpenVenue::Jupiter => borrowguard_jupiter::open::open(
            &ctx,
            rpc_url,
            args.vault.context("--vault is required")?,
            args.deposit,
            args.leverage,
        )?,
    };
    for (step, done) in &opened.steps {
        print_executed(step, done);
    }
    if let Some(table) = opened.lookup_table {
        println!("lookup table: {table}");
    }
    if let Some(why) = &opened.stopped {
        println!("stopped: {why}");
        return Ok(());
    }
    println!("position: {}", opened.position);
    if send {
        let venue = open_rebalancer(rpc_url, opened.position, owner)?;
        let snap = venue.snapshot()?;
        let lev = snap.position().leverage(snap.price()).unwrap_or(f64::INFINITY);
        println!(
            "{:.6} {} against {:.4} {}: {lev:.3}× (liquidation at {:.2}×)",
            snap.collateral,
            snap.collateral_symbol,
            snap.debt,
            snap.debt_symbol,
            snap.liquidation_leverage()
        );
        println!(
            "\nguard it: add to borrowguard.toml\n\n[[positions]]\nlabel = \"{}/{}\"\naddress = \"{}\"{}",
            snap.collateral_symbol,
            snap.debt_symbol,
            opened.position,
            opened.lookup_table.map_or(String::new(), |t| format!("\nlookup_table = \"{t}\""))
        );
    }
    Ok(())
}

fn run_close(args: CloseArgs) -> Result<()> {
    let signer = load_keypair(&args.keypair)?;
    let owner = solana_sdk::signer::Signer::pubkey(&signer);
    let rpc_url = &args.rpc.rpc_url;
    let venue = open_rebalancer(rpc_url, args.position, owner)?;
    let snap = venue.snapshot()?;
    let lev = snap.position().leverage(snap.price()).unwrap_or(f64::INFINITY);
    println!(
        "{:.6} {} against {:.4} {} at {lev:.3}×: closing{}",
        snap.collateral,
        snap.collateral_symbol,
        snap.debt,
        snap.debt_symbol,
        if args.exec.send { "" } else { " (simulation only; add --send to execute)" }
    );
    let ctx = args.exec.context(rpc_url, signer);
    print_executed("close", &venue.close(&ctx)?);
    Ok(())
}

fn print_executed(step: &str, done: &Executed) {
    match done {
        Executed::Simulated { summary, .. } => println!("{step}: simulated ok: {summary}"),
        Executed::Sent { signature, summary } => {
            println!("{step}: sent: {summary}\n  signature: {signature}")
        }
    }
}

fn channels(notifier: &Notifier) -> String {
    let channels = notifier.channels();
    if channels.is_empty() {
        "off".to_string()
    } else {
        channels.join(", ")
    }
}

fn date(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .map_or_else(|| ms.to_string(), |t| t.format("%Y-%m-%d %H:%M").to_string())
}
