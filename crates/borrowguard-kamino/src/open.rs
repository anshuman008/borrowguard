//! Opening a leveraged Kamino position from the wallet, in one atomic
//! transaction funded by a klend flash loan:
//!
//! ```text
//! wrap SOL → flash_borrow(debt) → swap debt→collateral → refresh
//! → deposit_v2(stake + swap output) → refresh → borrow_v2(flash + fee) → flash_repay
//! ```
//!
//! The first time, a setup transaction creates the wallet's klend accounts
//! (user metadata, obligation, farm states), and a lookup table is made for
//! the position's accounts so the swap route fits.

use anyhow::{ensure, Context, Result};
use borrowguard_exec::{
    token::{unwrap_sol, wrap_sol, NATIVE_MINT},
    tx::{create_lookup_table, COMPUTE_BUDGET_IXS},
    ExecContext, Executed, Opened,
};
use solana_program::{instruction::Instruction, pubkey::Pubkey};
use solana_sdk::signer::Signer;

use crate::exec::{position_accounts, size_relever, MAX_SWAP_ACCOUNTS};
use crate::ix::{self, FarmKind};
use crate::layout::parse_obligation;
use crate::reserve::Reserve;
use crate::{find_reserve, PROGRAM_ID};

/// How many of a wallet's vanilla obligation ids to consider.
const OBLIGATION_IDS: u8 = 16;

pub struct OpenRequest {
    pub market: Pubkey,
    pub collateral_mint: Pubkey,
    pub debt_mint: Pubkey,
    /// Collateral tokens put in from the wallet.
    pub deposit: f64,
    pub leverage: f64,
}

/// LTV that `leverage` needs, checked against what the reserves allow.
pub fn check_leverage(leverage: f64, coll: &Reserve, debt: &Reserve) -> Result<f64> {
    ensure!(leverage > 1.0, "leverage must be above 1×");
    let ltv = 1.0 - 1.0 / leverage;
    // klend treats a borrow factor below 100% as 100%.
    let borrow_factor = debt.borrow_factor_pct.max(100) as f64 / 100.0;
    let max_ltv = coll.loan_to_value_pct as f64 / 100.0 / borrow_factor;
    ensure!(
        ltv < max_ltv,
        "{leverage}× needs an LTV of {:.1}%, above the {:.1}% {} allows against {} (max {:.2}×; \
         elevation groups aren't supported yet)",
        ltv * 100.0,
        max_ltv * 100.0,
        coll.name,
        debt.name,
        1.0 / (1.0 - max_ltv)
    );
    Ok(ltv)
}

pub fn open(ctx: &ExecContext, req: &OpenRequest) -> Result<Opened> {
    let owner = ctx.signer.pubkey();
    let (coll_key, coll) = find_reserve(&ctx.rpc, &req.market, &req.collateral_mint)?;
    let (debt_key, debt) = find_reserve(&ctx.rpc, &req.market, &req.debt_mint)?;
    ensure!(coll_key != debt_key, "collateral and debt must differ");
    ensure!(req.deposit > 0.0, "deposit must be positive");
    check_leverage(req.leverage, &coll, &debt)?;
    ensure!(
        coll.market_price > 0.0 && debt.market_price > 0.0,
        "reserve has no price ({} {}, {} {})",
        coll.name,
        coll.market_price,
        debt.name,
        debt.market_price
    );

    let (obligation, new_obligation) = pick_obligation(ctx, &owner, &req.market)?;
    let mut opened =
        Opened { position: obligation, lookup_table: None, steps: Vec::new(), stopped: None };

    let setup = setup_ixs(
        ctx,
        &owner,
        &obligation,
        new_obligation,
        [(coll_key, &coll), (debt_key, &debt)],
    )?;
    if !setup.is_empty() {
        let n = setup.len();
        let done = ctx.run(&setup, &[], |units| format!("{n} setup instructions, {units} CU"))?;
        let simulated = matches!(done, Executed::Simulated { .. });
        opened.steps.push(("setup", done));
        if simulated {
            opened.stopped =
                Some("the open transaction needs the setup on-chain first; add --send".into());
            return Ok(opened);
        }
    }

    let table = match ctx.lookup_tables.first() {
        Some(_) => None,
        None if ctx.send => {
            let accounts =
                position_accounts(&owner, &obligation, &[(coll_key, &coll), (debt_key, &debt)]);
            Some(create_lookup_table(&ctx.rpc, &ctx.signer, &accounts, ctx.priority_fee)?)
        }
        None => {
            opened.stopped =
                Some("the open transaction needs a lookup table; --send creates one".into());
            return Ok(opened);
        }
    };
    opened.lookup_table = table.or(ctx.lookup_tables.first().copied());

    // Size from the reserves' last-refresh prices; the swap quote and klend's
    // own LTV check guard the actual amounts.
    let price = coll.market_price / debt.market_price;
    let equity = (req.deposit * 10f64.powi(coll.decimals as i32)).floor() as u64;
    let flash = (req.deposit * price * (req.leverage - 1.0) * 10f64.powi(debt.decimals as i32))
        .floor() as u64;
    let quote =
        ctx.jupiter.quote(&debt.mint, &coll.mint, flash, ctx.slippage_bps, MAX_SWAP_ACCOUNTS)?;
    let amounts = size_relever(flash, quote.min_out, &debt)?;
    let swap = ctx.jupiter.swap_instructions(&quote, &owner)?;
    let coll_ata = ix::associated_token_address(&owner, &coll.mint, &coll.token_program);
    let debt_ata = ix::associated_token_address(&owner, &debt.mint, &debt.token_program);

    let referrer = parse_obligation(&ctx.rpc.get_account_data(&obligation).unwrap_or_default())
        .ok()
        .map(|o| o.referrer)
        .filter(|r| *r != Pubkey::default());
    let refresh = |deposits: &[Pubkey]| {
        vec![
            ix::refresh_reserve(&PROGRAM_ID, &coll_key, &coll),
            ix::refresh_reserve(&PROGRAM_ID, &debt_key, &debt),
            ix::refresh_obligation(
                &PROGRAM_ID,
                &req.market,
                &obligation,
                deposits,
                &[],
                referrer.as_ref(),
            ),
        ]
    };

    let mut ixs: Vec<Instruction> = Vec::new();
    if coll.mint == NATIVE_MINT {
        ixs.extend(wrap_sol(&owner, equity));
    }
    let borrow_index = (COMPUTE_BUDGET_IXS + ixs.len()) as u8;
    ixs.push(ix::flash_borrow(&PROGRAM_ID, &owner, &debt_key, &debt, &debt_ata, amounts.flash));
    ixs.extend(swap.setup);
    ixs.push(swap.swap);
    ixs.extend(swap.cleanup);
    ixs.extend(refresh(&[]));
    ixs.push(ix::deposit_v2(
        &PROGRAM_ID,
        &owner,
        &obligation,
        &coll_key,
        &coll,
        &coll_ata,
        equity + amounts.deposit,
    ));
    ixs.extend(refresh(&[coll_key]));
    ixs.push(ix::borrow_v2(
        &PROGRAM_ID,
        &owner,
        &obligation,
        &debt_key,
        &debt,
        &debt_ata,
        amounts.borrow,
        &[],
    ));
    ixs.push(ix::flash_repay(
        &PROGRAM_ID,
        &owner,
        &debt_key,
        &debt,
        &debt_ata,
        amounts.flash,
        borrow_index,
    ));
    if coll.mint == NATIVE_MINT || debt.mint == NATIVE_MINT {
        // Swap output above the guaranteed minimum stays in the wallet as SOL.
        ixs.push(unwrap_sol(&owner));
    }

    let scale = |raw: u64, decimals: u32| raw as f64 / 10f64.powi(decimals as i32);
    let tables: Vec<Pubkey> = swap.lookup_tables.iter().copied().chain(table).collect();
    let done = ctx.run(&ixs, &tables, |units| {
        format!(
            "deposit {:.6} {c} + borrow {:.4} {d} → ≥{:.6} {c} (quote {:.6}, impact {:.3}%), {units} CU",
            scale(equity, coll.decimals),
            scale(amounts.borrow, debt.decimals),
            scale(quote.min_out, coll.decimals),
            scale(quote.out_amount, coll.decimals),
            quote.price_impact_pct * 100.0,
            c = coll.name,
            d = debt.name,
        )
    })?;
    opened.steps.push(("open", done));
    Ok(opened)
}

/// The wallet's first empty vanilla obligation in `market`, or the first
/// free id to create one at (`true`).
fn pick_obligation(ctx: &ExecContext, owner: &Pubkey, market: &Pubkey) -> Result<(Pubkey, bool)> {
    let keys: Vec<Pubkey> = (0..OBLIGATION_IDS)
        .map(|id| ix::vanilla_obligation(&PROGRAM_ID, owner, market, id))
        .collect();
    let accounts = ctx.rpc.get_multiple_accounts(&keys)?;
    let mut free = None;
    for (key, account) in keys.iter().zip(&accounts) {
        match account {
            Some(a) => {
                let o = parse_obligation(&a.data)?;
                if o.active_deposits().count() == 0 && o.active_borrows().count() == 0 {
                    return Ok((*key, false));
                }
            }
            None => free = free.or(Some(*key)),
        }
    }
    free.map(|key| (key, true)).with_context(|| {
        format!("all {OBLIGATION_IDS} vanilla obligation ids in {market} are in use")
    })
}

/// Instructions creating whatever the open transaction expects to exist:
/// klend user metadata, the obligation, its farm states and token accounts.
fn setup_ixs(
    ctx: &ExecContext,
    owner: &Pubkey,
    obligation: &Pubkey,
    new_obligation: bool,
    reserves: [(Pubkey, &Reserve); 2],
) -> Result<Vec<Instruction>> {
    let [(coll_key, coll), (debt_key, debt)] = reserves;
    let metadata = ix::user_metadata(&PROGRAM_ID, owner);
    // (reserve, its farm, which side) for each reserve with a farm.
    let farms: Vec<(Pubkey, &Reserve, Pubkey, FarmKind)> = [
        (coll_key, coll, coll.farm_collateral, FarmKind::Collateral),
        (debt_key, debt, debt.farm_debt, FarmKind::Debt),
    ]
    .into_iter()
    .filter_map(|(key, r, farm, kind)| Some((key, r, farm?, kind)))
    .collect();
    let states: Vec<Pubkey> =
        farms.iter().map(|f| ix::obligation_farm_user_state(&f.2, obligation)).collect();
    let atas = [coll, debt].map(|r| ix::associated_token_address(owner, &r.mint, &r.token_program));

    let probe: Vec<Pubkey> =
        std::iter::once(metadata).chain(states.iter().copied()).chain(atas).collect();
    let found = ctx.rpc.get_multiple_accounts(&probe)?;
    let missing = |key: &Pubkey| found[probe.iter().position(|k| k == key).unwrap()].is_none();

    let mut ixs = Vec::new();
    if missing(&metadata) {
        ixs.push(ix::init_user_metadata(&PROGRAM_ID, owner));
    }
    if new_obligation {
        let market = coll.lending_market;
        let id = (0..OBLIGATION_IDS)
            .find(|id| ix::vanilla_obligation(&PROGRAM_ID, owner, &market, *id) == *obligation)
            .context("obligation id")?;
        ixs.push(ix::init_vanilla_obligation(&PROGRAM_ID, owner, &market, id));
    }
    for ((reserve_key, reserve, farm, kind), state) in farms.iter().zip(&states) {
        if missing(state) {
            ixs.push(ix::init_obligation_farm(
                &PROGRAM_ID,
                owner,
                obligation,
                reserve_key,
                reserve,
                farm,
                *kind,
            ));
        }
    }
    for (r, ata) in [coll, debt].iter().zip(&atas) {
        if missing(ata) {
            ixs.push(ix::create_ata_idempotent(owner, &r.mint, &r.token_program));
        }
    }
    Ok(ixs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reserve::{parse_reserve, tests::reserve_bytes};

    #[test]
    fn leverage_must_fit_the_reserve_ltv() {
        let mut sol = parse_reserve(&reserve_bytes("SOL", 100.0, 9)).unwrap();
        let mut usdc = parse_reserve(&reserve_bytes("USDC", 1.0, 6)).unwrap();
        sol.loan_to_value_pct = 75;
        usdc.borrow_factor_pct = 100;
        assert!((check_leverage(2.0, &sol, &usdc).unwrap() - 0.5).abs() < 1e-12);
        assert!(check_leverage(3.9, &sol, &usdc).is_ok());
        let err = check_leverage(4.0, &sol, &usdc).unwrap_err().to_string();
        assert!(err.contains("max 4.00×"), "{err}");
        assert!(check_leverage(1.0, &sol, &usdc).is_err());
        usdc.borrow_factor_pct = 150; // 75% / 1.5 = 50% → max 2×
        assert!(check_leverage(2.0, &sol, &usdc).is_err());
    }
}
