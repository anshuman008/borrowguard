# BorrowGuard

An open-source Rust bot that keeps **your own** Solana lending positions
(Kamino, Jupiter Lend, Project 0 / marginfi) at a target leverage, so they
never drift into liquidation.

You pick a target leverage, a no-trade band around it and an emergency
"ripcord" level. When leverage leaves the band, BorrowGuard alerts you and (on
Kamino and Jupiter Lend) rebalances the position with a flash loan, in one
transaction, from your own wallet.

> **Status: early.** The controller core, backtester and live monitor (Kamino,
> Jupiter Lend, Project 0 / marginfi) work. **Rebalancing in both directions**
> (delever and relever) works on Kamino and Jupiter Lend, by hand or
> automatically from `watch --live`, and is tested on a mainnet fork. It has
> not been run with real money yet.

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

Each delever is one atomic transaction (Kamino shown):

```
refresh reserves + obligation → flash-borrow USDC → repay debt
→ withdraw collateral → swap collateral → USDC → flash-repay
```

## Layout

| Crate | What |
|---|---|
| `crates/borrowguard-core` | Position math, the controller and the `Venue` trait. No Solana deps; shared by backtester and bot. |
| `crates/borrowguard-kamino` | Kamino Lend venue: klend account layouts, instruction builders, live snapshots, delever execution. |
| `crates/borrowguard-jupiter` | Jupiter Lend venue: Fluid vault layouts, exact tick math, live snapshots, `operate` / flash-loan builders, delever and relever execution. |
| `crates/borrowguard-marginfi` | Project 0 (marginfi v2) venue: account and bank layouts, live snapshots from the risk engine's health cache. |
| `crates/borrowguard-exec` | Execution plumbing shared by venues: Jupiter swaps, lookup tables, compose / simulate / send. |
| `crates/borrowguard-cli` | The `borrowguard` binary: `fetch-prices`, `backtest`, `positions`, `monitor`, `watch`, `lookup-table`, `rebalance`. |

## Venues

| Venue | Status |
|---|---|
| Kamino Lend | ✅ monitor · ✅ delever + relever, manual or automatic (fork-tested) |
| Jupiter Lend | ✅ monitor · ✅ delever + relever, manual or automatic (fork-tested) |
| Project 0 / marginfi | ✅ monitor |
| Save | planned |

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

# Find your Kamino, Project 0 / marginfi and Jupiter Lend positions
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

And for a SOL/USDC account on Project 0 / marginfi (2026-10-19), default 3× settings:

```
marginfi  SOL/USDC  liquidation at 7.04× (LTV 85.8%), borrow limit 3.67× (LTV 72.7%)
controller  target 3× band ±0.15 ripcord 3.6× (monitor only: nothing is traded)
19:08:26  SOL/USDC 101.7669  lev 2.136×  LTV 53.2%  ripcord@ 74.9371 (-26.4%)  liq@ 63.0872 (-38.0%)  → RELEVER borrow 1.33 USDC, buy SOL
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
- **Project 0 / marginfi:** simulates `lending_account_pulse_health` with each
  bank's oracle accounts and decodes the account's fresh health cache. The risk
  engine does the oracle reads, weights, e-mode boosts and confidence
  haircuts; the liquidation LTV comes from the values it compares.

`--payer` is the fee payer for those simulations: any wallet with a little SOL,
normally your own. Kamino and marginfi default it to the position's owner,
which fails if that wallet holds no SOL. Jupiter finds
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

## Rebalance a position

Each rebalance is one atomic transaction, funded by a flash loan from the same
protocol. On Kamino:

```
delever:  flash-borrow USDC → refresh → repay debt → refresh → withdraw SOL
          → Jupiter swap SOL → USDC → flash-repay
relever:  flash-borrow USDC → Jupiter swap USDC → SOL → refresh → deposit SOL
          → refresh → borrow USDC → flash-repay
```

On Jupiter Lend, one `operate` call changes collateral and debt together:

```
delever:  flash-borrow USDC → operate(withdraw SOL, pay back USDC)
          → Jupiter swap SOL → USDC → flash payback
relever:  flash-borrow USDC → Jupiter swap USDC → SOL
          → operate(deposit SOL, borrow USDC) → flash payback
```

`operate` must be handed the tick the position will land in, so BorrowGuard
predicts it with a port of the vault's own integer math (it reproduces the
stored tick of every live position it was checked against), and creates any
tick or branch account the move needs first.

Amounts follow the swap's **guaranteed** output (Jupiter's slippage floor): a
delever repays only what that output covers net of the flash fee, a relever
deposits only what it guarantees and borrows back exactly the flash loan plus
its fee. Any extra stays in your wallet, so a delever lands slightly above
target, more so at high leverage. Every transaction is simulated first.

```bash
# One-time per position: a lookup table with its accounts (~0.004 SOL rent).
# Without it, the swap route doesn't fit in one transaction.
./target/release/borrowguard lookup-table --position <POSITION> --keypair ~/.config/solana/borrowguard.json

# Simulate what the controller would do (nothing is sent)
./target/release/borrowguard rebalance --position <POSITION> --keypair ~/.config/solana/borrowguard.json \
    --lookup-table <TABLE> --target 3

# Do it (--sell / --borrow force a direction and size)
./target/release/borrowguard rebalance ... --send
```

The keypair must own the position (Kamino: the obligation's owner; Jupiter
Lend: the wallet holding the position NFT). Keep it in a file outside the repo
(it can also come from `BORROWGUARD_KEYPAIR` in `.env`).

### Automatic rebalancing from `watch`

Add an `[execution]` table and opt positions in (see
`borrowguard.example.toml`):

```toml
[execution]
keypair = "~/.config/solana/borrowguard.json"
cooldown_secs = 120
max_trades_per_day = 24

[[position]]
address = "<POSITION>"
target = 3.0
execute = true
lookup_table = "<TABLE>"
```

```bash
./target/release/borrowguard watch          # rebalances are simulated and printed
./target/release/borrowguard watch --live   # rebalances are sent
touch STOP                                  # pause all trading immediately; rm STOP to resume
```

Guards: two switches (`execute = true` **and** `--live`), a kill-switch file,
a per-position cooldown (20s for ripcords so an emergency can retry), a daily
trade cap, and an alert on every trade or failure. At startup it checks the
keypair owns each position and that a lookup table is set. Real fork run:

```
controller  target 1.4× band ±0.1 ripcord 2.5× (AUTO-REBALANCE LIVE: trades are sent)
15:24:50  SOL/USDC 102.2289  lev 1.787×  LTV 44.0%  ...  → DELEVER sell 93.23 USDC of SOL, repay debt
15:24:50    ↳ TRADED sell 0.912000 SOL → ≥92.7323 USDC (quote 93.1983, impact 0.015%), repay 92.7314 USDC, 423321 CU · 12AFja…
15:24:53  SOL/USDC 102.2289  lev 1.401×  LTV 28.6%  ...  → HOLD
```

### Test on a mainnet fork first

[Surfpool](https://github.com/txtx/surfpool) runs a local copy of mainnet with
fake money. The test script takes a real Kamino obligation or Jupiter Lend
position, hands it to a throwaway keypair inside the fork, relevers it up and
delevers it back down:

```bash
surfpool start -u "$RPC_URL" --no-tui --no-deploy        # another terminal
RPC_URL=... python3 scripts/fork_delever_test.py <POSITION> --up 2.2 --target 1.4
```

Run it soon after starting the fork: Surfpool makes slots slower than mainnet,
and the script refuses a fork that has drifted far behind.

Kamino SOL/USDC:

```
sent: borrow 13.9274 USDC → ≥0.135688 SOL (quote 0.136370, impact 0.020%), deposit 0.135688 SOL, 431654 CU
PASS up: leverage 1.767× → 2.201× (target 2.2×)
sent: sell 0.253874 SOL → ≥25.7919 USDC (quote 25.9215, impact 0.029%), repay 25.7916 USDC, 426437 CU
PASS down: leverage 2.201× → 1.403× (target 1.4×)
```

Jupiter Lend SOL/USDC (also passes on cbBTC/USDC and the JitoSOL/SOL loop):

```
sent: borrow 312.8846 USDC → ≥3.086737 SOL (quote 3.102248, impact 0.037%), deposit 3.086737 SOL, 335029 CU
PASS up: leverage 2.002× → 2.502× (target 2.5×)
sent: sell 5.662911 SOL → ≥568.0523 USDC (quote 570.9068, impact 0.008%), repay 568.0523 USDC, 323996 CU
PASS down: leverage 2.502× → 1.603× (target 1.6×)
```

On the fork the script routes swaps through classic pools only
(`--jupiter-dexes`): proprietary market makers price from off-chain updates a
frozen fork doesn't receive.

## Roadmap

1. ✅ Controller core and backtester
2. ✅ Read-only monitor for Kamino: live leverage, trigger prices, alerts
3. ✅ Jupiter Lend venue (monitor)
4. ✅ Project 0 / marginfi venue (monitor)
5. ✅ Watch many positions from one config file
6. ✅ Executor for Kamino: flash-loan delever and relever, manual
   (`rebalance`) or automatic (`watch --live`), tested on a
   [Surfpool](https://github.com/txtx/surfpool) mainnet fork
7. ✅ Executor for Jupiter Lend: the same, fork-tested on SOL/USDC,
   cbBTC/USDC and JitoSOL/SOL vaults
8. ⬜ Mainnet with a small position; dry-run by default
9. ⬜ Stretch: liquidator mode (same building blocks, other people's positions)

## Safety

- Use a dedicated wallet that holds only the position you want guarded.
- Keep the signing keypair in a file outside the repo; `.env` and `*.keypair.json`
  are gitignored.
- `rebalance` only simulates unless you pass `--send`; `watch` only simulates
  unless a position has `execute = true` **and** you pass `--live`. Every
  transaction is simulated before it is sent. `touch STOP` pauses all trading.
- Test on a Surfpool fork before trusting it with real money.
- This is experimental software that can lose money. No warranty.

## License

MIT
