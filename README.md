# BorrowGuard

An open-source Rust bot that keeps **your own** Kamino lending position at a
target leverage, so it never drifts into liquidation.

Think of it as a constant-leverage fund for one person. The controller design
follows constant-leverage funds: a target,
a no-trade band, an emergency "ripcord" and flash-loan rebalancing. It runs
against a position you own instead of a pooled vault.

> **Status: early.** The controller core, backtester and live monitor (Kamino,
> Jupiter Lend) work. Kamino **delever** works and is tested on a mainnet fork;
> it runs only when you call it with `--send`. Automatic rebalancing from
> `watch`, and relevering, are not built yet.

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
| `crates/borrowguard-kamino` | Kamino Lend venue: klend account layouts, instruction builders, live snapshots, delever execution. |
| `crates/borrowguard-jupiter` | Jupiter Lend venue: Fluid vault layouts, exact tick math, live snapshots. |
| `crates/borrowguard-exec` | Execution plumbing shared by venues: Jupiter swaps, lookup tables, compose / simulate / send. |
| `crates/borrowguard-cli` | The `borrowguard` binary: `fetch-prices`, `backtest`, `positions`, `monitor`, `watch`, `lookup-table`, `delever`. |

## Venues

| Venue | Status |
|---|---|
| Kamino Lend | ✅ monitor · ✅ delever (fork-tested) |
| Jupiter Lend | ✅ monitor |
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

### Watch a live position

```bash
cp .env.example .env   # optional: RPC_URL and alert credentials

# Find your Kamino obligations and Jupiter Lend positions
./target/release/borrowguard positions --owner <YOUR_WALLET>

# Check once, or keep watching every 10s. The venue is detected from the account.
./target/release/borrowguard monitor --position <POSITION> --payer <YOUR_WALLET> --target 3 --once
./target/release/borrowguard monitor --position <POSITION> --payer <YOUR_WALLET> --target 3
```

Real output for a mainnet JitoSOL/SOL multiply position on Kamino (2026-10-12):

```
$ borrowguard monitor --position 2w9MS6MBFr68VpF6dkGwb2ucdKTpNFeXwmS79HSDTxAG --target 12 --band 0.4 --ripcord 15 --once
kamino  JITOSOL/SOL  liquidation at 20.00× (LTV 95.0%), borrow limit 12.50× (LTV 92.0%)
controller  target 12× band ±0.4 ripcord 15× (dry run: nothing is traded)
18:50:29  JITOSOL/SOL 1.3057  lev 12.974×  LTV 92.3%  ripcord@ 1.2912 (-1.1%)  liq@ 1.2685 (-2.9%)  → DELEVER sell 12117.09 SOL of JITOSOL, repay debt
```

And for a SOL/USDC position on Jupiter Lend (2026-10-13), default 3× settings:

```
jupiter  SOL/USDC  liquidation at 6.67× (LTV 85.0%), borrow limit 5.00× (LTV 80.0%)
controller  target 3× band ±0.15 ripcord 3.6× (dry run: nothing is traded)
19:39:44  SOL/USDC 108.1160  lev 3.888×  LTV 74.3%  ripcord@ 111.1921 (+2.8%)  liq@ 94.4769 (-12.6%)  → RIPCORD sell 5393.20 USDC of SOL, repay debt
```

How the monitor gets its numbers: it asks each protocol to do its own math by
simulating its instructions, which costs nothing and needs no signature.

- **Kamino:** simulates `refresh_reserve` ×2 + `refresh_obligation` and decodes
  the resulting accounts. klend does the oracle reads, interest accrual and
  elevation-group / borrow-factor math.
- **Jupiter Lend:** debt is derived from the position's tick with a bit-exact
  port of Fluid's tick math, converted to tokens with the vault's own
  `get_exchange_prices` view, and priced with the oracle's
  `get_exchange_rate_liquidate` view (the price the vault liquidates on). A
  position whose tick was already liquidated is reported as unsupported rather
  than shown with wrong numbers.

`--payer` is the fee payer for those simulations: any wallet with a little SOL,
normally your own. Kamino defaults it to the obligation owner. Jupiter finds
the position's NFT holder, a lookup the public RPC throttles, so pass
`--payer` there or use a dedicated RPC.

Alerts go to Telegram and/or Discord whenever the verdict changes (for example
hold → delever, or an RPC error).

### Watch all your positions at once

List them in `borrowguard.toml` (gitignored), with shared defaults and
per-position overrides:

```bash
cp borrowguard.example.toml borrowguard.toml   # then edit addresses and settings
./target/release/borrowguard watch             # add --once for a single round
```

```toml
interval_secs = 10

[defaults]
payer = "<YOUR_WALLET>"
target = 3.0

[[position]]
address = "<KAMINO_OBLIGATION>"
label = "kamino jitosol loop"
target = 12.0
band = 0.4
ripcord = 15.0

[[position]]
address = "<JUPITER_POSITION>"
label = "jupiter sol/usdc"
```

Unknown keys are rejected, so a typo like `traget` fails loudly instead of
silently falling back to a default. Each line of output and each alert is
prefixed with the position's label.

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

## Rebalance a Kamino position (delever)

One atomic transaction, funded by a Kamino flash loan:

```
flash-borrow USDC → refresh → repay debt → refresh → withdraw SOL
→ Jupiter swap SOL → USDC → flash-repay
```

The repay amount is what the swap's **guaranteed** output covers (Jupiter's
slippage floor, minus the flash fee), so the flash loan can always be repaid;
any extra output stays in your wallet. Every transaction is simulated first.

```bash
# One-time per position: a lookup table with its accounts (~0.004 SOL rent).
# Without it, the swap route doesn't fit in one transaction.
./target/release/borrowguard lookup-table --position <OBLIGATION> --keypair ~/.config/solana/borrowguard.json

# Simulate what the controller would do (nothing is sent)
./target/release/borrowguard delever --position <OBLIGATION> --keypair ~/.config/solana/borrowguard.json \
    --lookup-table <TABLE> --target 3

# Do it
./target/release/borrowguard delever ... --send
```

The keypair must be the obligation's owner. Keep it in a file outside the repo
(it can also come from `BORROWGUARD_KEYPAIR` in `.env`).

### Test on a mainnet fork first

[Surfpool](https://github.com/txtx/surfpool) runs a local copy of mainnet with
fake money. The test script copies a real obligation into it, hands it to a
throwaway keypair, and delevers it:

```bash
surfpool start -u "$RPC_URL" --no-tui --no-deploy        # another terminal
RPC_URL=... python3 scripts/fork_delever_test.py <KAMINO_OBLIGATION> --target 1.4
```

```
sent: sell 0.874558 SOL → ≥90.2472 USDC (quote 90.7007, impact 0.010%), repay 90.2463 USDC, 416032 CU
leverage now 1.401× (was 1.767×)
PASS: leverage 1.767× → 1.401× (target 1.4×)
```

On the fork the script routes swaps through classic pools only
(`--jupiter-dexes`): proprietary market makers price from off-chain updates a
frozen fork doesn't receive.

## Roadmap

1. ✅ Controller core and backtester
2. ✅ Read-only monitor for Kamino: live leverage, trigger prices, alerts
3. ✅ Jupiter Lend venue (monitor)
4. ✅ Watch many positions from one config file
5. 🟡 Executor: ✅ Kamino flash-loan delever, tested on a
   [Surfpool](https://github.com/txtx/surfpool) mainnet fork · ⬜ relever ·
   ⬜ automatic rebalancing from `watch`
6. ⬜ Mainnet with a small position; dry-run by default
7. ⬜ Stretch: liquidator mode (same building blocks, other people's positions)

## Safety

- Use a dedicated wallet that holds only the position you want guarded.
- Keep the signing keypair in a file outside the repo; `.env` and `*.keypair.json`
  are gitignored.
- `delever` only simulates unless you pass `--send`, and every transaction is
  simulated before it is sent.
- Test on a Surfpool fork before trusting it with real money.
- This is experimental software that can lose money. No warranty.

## License

MIT
