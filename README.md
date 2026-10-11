# BorrowGuard

An open-source Rust bot that keeps **your own** Kamino lending position at a
target leverage, so it never drifts into liquidation.

Think of it as a constant-leverage fund for one person. The controller design
follows constant-leverage funds: a target,
a no-trade band, an emergency "ripcord" and flash-loan rebalancing. It runs
against a position you own instead of a pooled vault.

> **Status: early.** The controller core, backtester and a read-only live
> monitor (Kamino) work. The executor is not built yet, so BorrowGuard
> never sends a transaction.

## How it works

Leverage = collateral value ÷ (collateral value − debt). When the price falls,
your debt stays the same while your collateral shrinks, so leverage climbs
toward the lender's liquidation point. BorrowGuard repays debt as the price
falls and borrows more as it rises, which keeps leverage at the target.

```
  1× ── target−band ══ target ══ target+band ── ripcord ── liquidation
        relever ◄─────── hold ───────► delever    emergency  lender acts
```

| Leverage (3× example) | Action |
|---|---|
| 2.85× – 3.15× | Hold |
| above 3.15× | Delever: sell collateral, repay debt, back to 3× |
| below 2.85× | Relever: borrow, buy collateral, back to 3× |
| ≥ 3.6× | Ripcord: delever in one trade with wider slippage, ignoring size caps |
| 4.0× | Kamino liquidates; BorrowGuard exists to never get here |

Trade sizes account for execution cost, so the position lands exactly on target
after paying slippage and fees (see `borrowguard-core/src/controller.rs`).

The planned delever is one atomic transaction:

```
refresh reserves + obligation → flash-borrow USDC → repay debt
→ withdraw collateral → swap collateral → USDC → flash-repay
```

## Layout

| Crate | What |
|---|---|
| `crates/borrowguard-core` | Position math, the controller and the `Venue` trait. No Solana deps; shared by backtester and bot. |
| `crates/borrowguard-kamino` | Kamino Lend venue: klend account layouts, refresh instructions, live snapshots. |
| `crates/borrowguard-cli` | The `borrowguard` binary: `fetch-prices`, `backtest`, `positions`, `monitor`. |

## Venues

| Venue | Status |
|---|---|
| Kamino Lend | ✅ monitor |
| Jupiter Lend | next |
| Project 0 / marginfi, Save | planned |

Each venue implements `borrowguard_core::Venue`, returning a `Snapshot`:
collateral and debt amounts, prices, and the venue's own liquidation and
borrow-limit LTVs. The controller doesn't know which venue it's running on.
BorrowGuard handles positions with exactly one collateral and one debt.

## Quick start

```bash
cargo build --release

# Price history (Binance public market data, no key needed)
./target/release/borrowguard fetch-prices --symbol SOLUSDT --interval 5m --days 365 --out data/SOLUSDT-5m.csv

# Compare band widths against an unmanaged position
./target/release/borrowguard backtest --prices data/SOLUSDT-5m.csv --band 0.05,0.1,0.15,0.25

# Log every trade of one run
./target/release/borrowguard backtest --prices data/SOLUSDT-5m.csv --band 0.15 --trades data/trades.csv
```

Run `borrowguard backtest --help` for every knob: target, ripcord, liquidation
LTV, cost, borrow APR, trade caps and liquidation bonus.

### Watch a live Kamino position

```bash
cp .env.example .env   # optional: RPC_URL and alert credentials

# Find your obligation
./target/release/borrowguard positions --owner <YOUR_WALLET>

# Check once, or keep watching every 10s
./target/release/borrowguard monitor --obligation <OBLIGATION> --target 3 --once
./target/release/borrowguard monitor --obligation <OBLIGATION> --target 3
```

Real output for a mainnet JitoSOL/SOL multiply position (2026-10-12):

```
$ borrowguard monitor --obligation 2w9MS6MBFr68VpF6dkGwb2ucdKTpNFeXwmS79HSDTxAG --target 12 --band 0.4 --ripcord 15 --once
kamino  JITOSOL/SOL  liquidation at 20.00× (LTV 95.0%), borrow limit 12.50× (LTV 92.0%)
controller  target 12× band ±0.4 ripcord 15× (dry run: nothing is traded)
18:50:29  JITOSOL/SOL 1.3057  lev 12.974×  LTV 92.3%  ripcord@ 1.2912 (-1.1%)  liq@ 1.2685 (-2.9%)  → DELEVER sell 12117.09 SOL of JITOSOL, repay debt
```

How the monitor gets its numbers: it simulates `refresh_reserve` ×2 +
`refresh_obligation` and decodes the resulting accounts. klend itself does the
oracle reads, interest accrual and elevation-group / borrow-factor math, so the
monitor sees exactly what klend would liquidate on. The simulation costs
nothing and needs no signature. The fee payer defaults to the obligation owner
and only needs to exist.

Alerts go to Telegram and/or Discord whenever the verdict changes (for example
hold → delever, or an RPC error).

### Example: SOL, Oct 2025 → Oct 2026, 5-minute bars, 3× target

```
price       182.67 → 112.02 (-38.7%), naive 3× = -116.0%

strategy               equity    return  max lev  delever  relever  ripcord  liquidated
unmanaged              568.77    -43.1%    4.00×        0        0        0  2025-11-03 20:50
band ±0.05              23.47    -97.7%    3.65×     1787     1653        0  no
band ±0.15              31.36    -96.9%    3.65×      346      293        0  no
band ±0.25              41.28    -95.9%    3.73×      128       96        0  no
band ±0.4               17.58    -98.2%    4.14×       56       34        1  2026-08-22 05:10
```

What this shows:

- **BorrowGuard prevents liquidation.** That's all it does. With bands up to
  ±0.25 the position survived a year in which an unmanaged 3× was liquidated
  within three weeks.
- **It doesn't prevent losses.** Constant 3× leverage in a choppy, falling
  market decays hard (−97% while SOL fell 39%). Every leveraged ETF has the
  same volatility decay. Backtest before you pick a target.
- **Narrow bands trade more and pay more in costs. Wide bands drift into the
  ripcord zone.**
- **Reaction speed matters.** On hourly bars, a 12% one-hour wick on 2023-12-11
  ($72.92 → $64.18) liquidates every 3× setting, because the cushion at 3× is
  only ~11%. The live bot must watch continuously, not poll.

### Backtest model

The model is deliberately conservative. Read `crates/borrowguard-cli/src/backtest.rs`
for details.

- The bot acts only at each bar's close. Liquidation is checked first, at the
  bar's low. Shorter bars approximate a faster bot.
- Liquidation is modelled as total: all debt is repaid, the lender keeps a bonus
  of `liq_bonus × debt`, and the simulation stops. Real Kamino liquidations are
  partial.
- Borrow interest and collateral yield accrue at constant APRs.
- Trades fill at the bar close with a fixed cost in bps. The live bot will use
  real swap quotes.

## Roadmap

1. ✅ Controller core and backtester
2. ✅ Read-only monitor for Kamino: live leverage, trigger prices, alerts
3. ⬜ Jupiter Lend venue
4. ⬜ Executor: flash-loan delever/relever transactions, tested on a
   [Surfpool](https://github.com/txtx/surfpool) mainnet fork
5. ⬜ Mainnet with a small position; dry-run by default
6. ⬜ Stretch: liquidator mode (same building blocks, other people's positions)

## Safety

- Use a dedicated wallet that holds only the position you want guarded.
- Keys belong in `.env` or a keypair file outside the repo. Both are gitignored.
- When the executor lands, it will be dry-run by default. Simulate every
  transaction before sending it.
- This is experimental software that can lose money. No warranty.

## License

MIT
