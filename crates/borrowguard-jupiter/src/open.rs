//! Opening a leveraged Jupiter Lend position from the wallet, in one atomic
//! transaction funded by a Jupiter Lend flash loan:
//!
//! ```text
//! wrap SOL → flash-borrow debt → swap debt→collateral
//! → operate(deposit stake + swap output, borrow flash + fee) → flash payback
//! ```
//!
//! A setup transaction first creates the position (and mints its NFT), and
//! a lookup table is made for the position's accounts so the swap fits.

use anyhow::{ensure, Context, Result};
use borrowguard_exec::{
    token::{create_ata_idempotent, unwrap_sol, wrap_sol, NATIVE_MINT},
    tx::create_lookup_table,
    ExecContext, Execute, Executed, Opened,
};
use solana_program::pubkey::Pubkey;
use solana_sdk::signer::Signer;

use crate::exec::{base_units, scale_down, size_relever, MAX_SWAP_ACCOUNTS};
use crate::ix::{self, Token};
use crate::layout::{mint_decimals, parse_vault_config, parse_vault_state};
use crate::{find_positions, JupiterVenue};

/// Oracle rates are debt-per-collateral with 15 decimals.
const RATE_SCALE: f64 = 1e15;

/// LTV that `leverage` needs, checked against the vault's collateral factor
/// (per mille).
pub fn check_leverage(leverage: f64, collateral_factor: u16) -> Result<f64> {
    ensure!(leverage > 1.0, "leverage must be above 1×");
    let ltv = 1.0 - 1.0 / leverage;
    let max_ltv = collateral_factor as f64 / 1_000.0;
    ensure!(
        ltv < max_ltv,
        "{leverage}× needs an LTV of {:.1}%, above the vault's {:.1}% (max {:.2}×)",
        ltv * 100.0,
        max_ltv * 100.0,
        1.0 / (1.0 - max_ltv)
    );
    Ok(ltv)
}

/// Open a position in `vault_id` with `deposit` collateral tokens from the
/// wallet, levered to `leverage`.
pub fn open(
    ctx: &ExecContext,
    rpc_url: &str,
    vault_id: u16,
    deposit: f64,
    leverage: f64,
) -> Result<Opened> {
    ensure!(deposit > 0.0, "deposit must be positive");
    let owner = ctx.signer.pubkey();
    let keys = [ix::vault_config_pda(vault_id), ix::vault_state_pda(vault_id)];
    let accounts = ctx.rpc.get_multiple_accounts(&keys)?;
    let get =
        |i: usize| accounts[i].as_ref().with_context(|| format!("vault {vault_id} not found"));
    let config = parse_vault_config(&get(0)?.data)?;
    let state = parse_vault_state(&get(1)?.data)?;
    check_leverage(leverage, config.collateral_factor)?;

    let mints = ctx.rpc.get_multiple_accounts(&[config.supply_token, config.borrow_token])?;
    let token = |i: usize, mint: Pubkey| -> Result<Token> {
        let a = mints[i].as_ref().with_context(|| format!("mint {mint} not found"))?;
        Ok(Token { mint, program: a.owner, decimals: mint_decimals(&a.data)? })
    };
    let (supply, borrow) = (token(0, config.supply_token)?, token(1, config.borrow_token)?);

    // Reuse an empty position of the wallet's in this vault, else make one.
    let empty = find_positions(rpc_url, &owner)?.into_iter().find(|p| {
        p.position.vault_id == vault_id && p.position.supply_only && p.position.supply_raw == 0
    });
    let mut setup = Vec::new();
    let position = match empty {
        Some(p) => p.key,
        None => {
            setup.push(ix::init_position(&owner, vault_id, state.next_position_id));
            ix::position_pda(vault_id, state.next_position_id)
        }
    };
    let atas = [supply.ata(&owner), borrow.ata(&owner)];
    for (t, account) in [supply, borrow].iter().zip(ctx.rpc.get_multiple_accounts(&atas)?) {
        if account.is_none() {
            setup.push(create_ata_idempotent(&owner, &t.mint, &t.program));
        }
    }

    let mut opened = Opened { position, lookup_table: None, steps: Vec::new(), stopped: None };
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

    let venue = JupiterVenue::new(rpc_url, position, Some(owner));
    let table = match ctx.lookup_tables.first() {
        Some(_) => None,
        None if ctx.send => {
            let accounts = venue.static_accounts(&owner)?;
            Some(create_lookup_table(&ctx.rpc, &ctx.signer, &accounts, ctx.priority_fee)?)
        }
        None => {
            opened.stopped =
                Some("the open transaction needs a lookup table; --send creates one".into());
            return Ok(opened);
        }
    };
    opened.lookup_table = table.or(ctx.lookup_tables.first().copied());

    let l = venue.load(&owner)?;
    let rate = l.r.rate as f64 / RATE_SCALE;
    let equity = base_units(deposit, supply.decimals);
    let flash = base_units(deposit * rate * (leverage - 1.0), borrow.decimals);
    let quote = ctx.jupiter.quote(
        &borrow.mint,
        &supply.mint,
        flash,
        ctx.slippage_bps,
        MAX_SWAP_ACCOUNTS,
    )?;
    let amounts = size_relever(flash, quote.min_out, l.flash_fee_bps)?;
    let swap = ctx.jupiter.swap_instructions(&quote, &owner)?;
    let (inits, operate) = venue.plan_operate(
        &owner,
        &l,
        (equity + amounts.deposit) as i128,
        amounts.borrow as i128,
    )?;

    let mut ixs = Vec::new();
    if supply.mint == NATIVE_MINT {
        ixs.extend(wrap_sol(&owner, equity));
    }
    ixs.extend(inits);
    ixs.push(ix::flashloan_borrow(&owner, &borrow, amounts.flash));
    ixs.extend(swap.setup);
    ixs.push(swap.swap);
    ixs.extend(swap.cleanup);
    ixs.push(operate);
    ixs.push(ix::flashloan_payback(&owner, &borrow, amounts.flash));
    if supply.mint == NATIVE_MINT || borrow.mint == NATIVE_MINT {
        // Swap output above the guaranteed minimum stays in the wallet as SOL.
        ixs.push(unwrap_sol(&owner));
    }

    let tables: Vec<Pubkey> =
        swap.lookup_tables.iter().copied().chain(l.vault_table).chain(table).collect();
    let (c, d) = (l.r.config.supply_token, l.r.config.borrow_token);
    let (c, d) = (
        borrowguard_core::token::symbol(&c.to_string()),
        borrowguard_core::token::symbol(&d.to_string()),
    );
    let done = ctx.run(&ixs, &tables, |units| {
        format!(
            "deposit {:.6} {c} + borrow {:.4} {d} → ≥{:.6} {c} (quote {:.6}, impact {:.3}%), {units} CU",
            scale_down(equity, supply.decimals),
            scale_down(amounts.borrow, borrow.decimals),
            scale_down(quote.min_out, supply.decimals),
            scale_down(quote.out_amount, supply.decimals),
            quote.price_impact_pct * 100.0,
        )
    })?;
    opened.steps.push(("open", done));
    Ok(opened)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leverage_must_fit_the_collateral_factor() {
        assert!((check_leverage(2.0, 800).unwrap() - 0.5).abs() < 1e-12);
        assert!(check_leverage(4.9, 800).is_ok());
        let err = check_leverage(5.0, 800).unwrap_err().to_string();
        assert!(err.contains("max 5.00×"), "{err}");
        assert!(check_leverage(0.5, 800).is_err());
    }
}
