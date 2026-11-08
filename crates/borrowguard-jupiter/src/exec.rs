//! Jupiter Lend rebalancing: one atomic transaction around the vault's
//! `operate`, funded by a Jupiter Lend flash loan.
//!
//! ```text
//! delever: flash-borrow debt → operate(withdraw collateral, pay back debt)
//!          → swap collateral→debt → flash payback
//! relever: flash-borrow debt → swap debt→collateral
//!          → operate(deposit collateral, borrow debt) → flash payback
//! close:   flash-borrow debt → operate(withdraw all, pay back all)
//!          → swap part of the collateral→debt → flash payback
//! ```
//!
//! `operate` must be handed the tick the position lands in, which depends on
//! the exact amounts, so it is predicted with the vault program's own integer
//! math (`get_new_position_info` and `add_debt_to_tick`).

use std::collections::HashSet;

use anyhow::{bail, ensure, Context, Result};
use borrowguard_core::Snapshot;
use borrowguard_exec::{
    token::{create_ata_idempotent, unwrap_sol, NATIVE_MINT},
    ExecContext, Execute, Executed,
};
use solana_program::{instruction::Instruction, pubkey::Pubkey, system_program, sysvar};
use solana_sdk::signer::Signer;

use crate::ix::{self, OperateAccounts, Token};
use crate::layout::{
    mint_decimals, parse_flashloan_fee, parse_tick, parse_tick_has_debt, parse_vault_metadata,
    parse_vault_state, tick_has_debt_index, token_account_owner, VaultState,
};
use crate::tick::{self, MIN_TICK};
use crate::{snapshot_from, JupiterVenue, Reading};

/// Route size cap so the swap fits beside `operate` and the flash loan.
pub(crate) const MAX_SWAP_ACCOUNTS: u8 = 20;
/// `operate` amount meaning all of it: withdraw every bit of collateral, or
/// pay back every bit of debt.
pub const ALL: i128 = i128::MIN;
/// Close flash-borrows this much more than the debt last seen, so interest
/// accrued until the transaction lands is covered; the rest stays in the wallet.
const CLOSE_DEBT_MARGIN: f64 = 1e-4;
/// The flashloan program rejects smaller loans.
const MIN_FLASHLOAN: u64 = 1_000;
const EXCHANGE_PRICES_PRECISION: u128 = 1_000_000_000_000;
const FOUR_DECIMALS: u128 = 10_000;
/// Highest `TickHasDebtArray` index.
const MAX_ARRAY_INDEX: u8 = 15;

pub(crate) fn flash_fee(amount: u64, fee_bps: u16) -> u64 {
    (amount as u128 * fee_bps as u128).div_ceil(FOUR_DECIMALS) as u64
}

/// Amounts for one delever, in token base units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeleverAmounts {
    /// Collateral withdrawn and swapped.
    pub sell: u64,
    /// Debt paid back. `operate` rounds a payback up by one unit, so it
    /// takes `repay + 1` from the wallet: exactly the flash loan.
    pub repay: u64,
    pub flash: u64,
    pub flash_fee: u64,
}

/// Size a delever from the swap's guaranteed output: the flash loan plus its
/// fee must be covered by `min_swap_out`.
pub fn size_delever(
    sell: u64,
    min_swap_out: u64,
    debt_outstanding: u64,
    fee_bps: u16,
) -> Result<DeleverAmounts> {
    let mut flash =
        (min_swap_out as u128 * FOUR_DECIMALS / (FOUR_DECIMALS + fee_bps as u128)) as u64;
    while flash > 0 && flash + flash_fee(flash, fee_bps) > min_swap_out {
        flash -= 1;
    }
    ensure!(flash >= MIN_FLASHLOAN, "swap output too small to repay anything");
    let repay = flash - 1;
    ensure!(
        repay < debt_outstanding,
        "delever would repay all debt ({repay} of {debt_outstanding}); close the position instead"
    );
    Ok(DeleverAmounts { sell, repay, flash, flash_fee: flash_fee(flash, fee_bps) })
}

/// Amounts for one relever, in token base units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleverAmounts {
    /// Debt token flash-borrowed and swapped into collateral.
    pub flash: u64,
    pub flash_fee: u64,
    /// Collateral deposited. `operate` rounds a deposit up by one unit, so
    /// it takes `deposit + 1`: the swap's guaranteed output.
    pub deposit: u64,
    /// Debt borrowed to repay the flash loan and its fee.
    pub borrow: u64,
}

pub fn size_relever(flash: u64, min_swap_out: u64, fee_bps: u16) -> Result<ReleverAmounts> {
    ensure!(flash >= MIN_FLASHLOAN, "borrow too small for a flash loan");
    ensure!(min_swap_out > 1, "swap output too small to deposit anything");
    let fee = flash_fee(flash, fee_bps);
    Ok(ReleverAmounts { flash, flash_fee: fee, deposit: min_swap_out - 1, borrow: flash + fee })
}

/// Vault exchange prices (1e12) and borrow fee (bps), as `operate` applies them.
#[derive(Debug, Clone, Copy)]
pub struct Prices {
    pub supply_ex: u128,
    pub borrow_ex: u128,
    pub borrow_fee: u16,
}

/// A position's raw collateral and net debt after `operate(new_col, new_debt)`,
/// amounts already scaled to 9 decimals ([`ALL`] passes through unscaled).
/// Port of `Position::get_new_position_info`.
pub fn apply_operate(
    col_raw: u128,
    debt_raw: u128,
    new_col: i128,
    new_debt: i128,
    p: Prices,
) -> Result<(u128, u128)> {
    let mut col = col_raw;
    if new_col == ALL {
        col = 0;
    } else if new_col > 0 {
        col += new_col as u128 * EXCHANGE_PRICES_PRECISION / p.supply_ex;
    } else if new_col < 0 {
        let withdraw = (new_col.unsigned_abs() * EXCHANGE_PRICES_PRECISION).div_ceil(p.supply_ex);
        col = col.checked_sub(withdraw).context("withdrawal exceeds collateral")?;
    }
    let mut debt = debt_raw;
    if new_debt == ALL {
        debt = 0;
    } else if new_debt > 0 {
        let borrow = (new_debt as u128 * EXCHANGE_PRICES_PRECISION).div_ceil(p.borrow_ex);
        debt += borrow + (borrow * p.borrow_fee as u128).div_ceil(FOUR_DECIMALS);
    } else if new_debt < 0 {
        let payback = new_debt.unsigned_abs() * EXCHANGE_PRICES_PRECISION / p.borrow_ex - 1;
        debt = debt.checked_sub(payback).context("payback exceeds debt")?;
    }
    Ok((col, debt))
}

/// The vault keeps amounts with 9 decimals.
fn scale(amount: i128, decimals: u8) -> Result<i128> {
    ensure!(decimals <= 9, "tokens with more than 9 decimals aren't supported");
    if amount == ALL {
        return Ok(ALL);
    }
    Ok(amount * 10i128.pow(9 - decimals as u32))
}

pub(crate) fn base_units(tokens: f64, decimals: u8) -> u64 {
    (tokens * 10f64.powi(decimals as i32)).floor() as u64
}

pub(crate) fn scale_down(raw: u64, decimals: u8) -> f64 {
    raw as f64 / 10f64.powi(decimals as i32)
}

/// Position state an `operate` needs, read fresh.
pub(crate) struct Loaded {
    pub(crate) r: Reading,
    position_token_account: Pubkey,
    pub(crate) supply: Token,
    pub(crate) borrow: Token,
    state: VaultState,
    /// The vault's own address lookup table.
    pub(crate) vault_table: Option<Pubkey>,
    pub(crate) flash_fee_bps: u16,
}

impl Loaded {
    fn snapshot(&self) -> Result<Snapshot> {
        let r = &self.r;
        snapshot_from(&r.pos, &r.config, r.supply_ex, r.borrow_ex, r.rate, r.slot)
    }

    /// Where the position sits: its tick, the tick-id count its liquidation
    /// history is keyed by, raw collateral and raw net debt. A position
    /// without debt sits at the bottom tick.
    fn current(&self) -> Result<(i32, u32, u128, u128)> {
        let pos = &self.r.pos;
        if pos.supply_only {
            return Ok((MIN_TICK, 0, pos.supply_raw as u128, 0));
        }
        let net_debt = tick::debt_raw(pos.supply_raw, pos.tick)?
            .checked_sub(pos.dust_debt_raw as u128)
            .context("dust debt exceeds position debt")?;
        Ok((pos.tick, self.r.tick.total_ids, pos.supply_raw as u128, net_debt))
    }
}

impl JupiterVenue {
    pub(crate) fn load(&self, owner: &Pubkey) -> Result<Loaded> {
        let r = self.read()?;
        let vault = r.pos.vault_id;
        let keys = [
            r.config.supply_token,
            r.config.borrow_token,
            r.pos.position_mint,
            ix::vault_state_pda(vault),
            ix::vault_metadata_pda(vault),
            ix::flashloan_admin(),
        ];
        let accounts = self.rpc.get_multiple_accounts(&keys)?;
        let get = |i: usize| accounts[i].as_ref().with_context(|| format!("{} not found", keys[i]));
        let token = |i: usize| -> Result<Token> {
            let a = get(i)?;
            Ok(Token { mint: keys[i], program: a.owner, decimals: mint_decimals(&a.data)? })
        };
        let (supply, borrow) = (token(0)?, token(1)?);

        let nft_account =
            borrowguard_exec::token::associated_token_address(owner, &keys[2], &get(2)?.owner);
        let nft = self.rpc.get_account_data(&nft_account).ok();
        ensure!(
            nft.as_deref().is_some_and(|d| d.len() >= 72
                && token_account_owner(d).ok() == Some(*owner)
                && d[64..72] == 1u64.to_le_bytes()),
            "signer {owner} doesn't hold this position's NFT"
        );

        Ok(Loaded {
            position_token_account: nft_account,
            supply,
            borrow,
            state: parse_vault_state(&get(3)?.data)?,
            vault_table: get(4).ok().and_then(|a| parse_vault_metadata(&a.data).ok()),
            flash_fee_bps: parse_flashloan_fee(&get(5)?.data)?,
            r,
        })
    }

    /// The `operate` instruction for these amounts (token base units), and the
    /// instructions that create any tick / branch accounts it needs first.
    pub(crate) fn plan_operate(
        &self,
        owner: &Pubkey,
        l: &Loaded,
        new_col: i128,
        new_debt: i128,
    ) -> Result<(Vec<Instruction>, Instruction)> {
        let vault = l.r.pos.vault_id;
        let (current_tick, current_ids, col_raw, net_debt) = l.current()?;
        let prices = Prices {
            supply_ex: l.r.supply_ex,
            borrow_ex: l.r.borrow_ex,
            borrow_fee: l.r.config.borrow_fee,
        };
        let (col, debt) = apply_operate(
            col_raw,
            net_debt,
            scale(new_col, l.supply.decimals)?,
            scale(new_debt, l.borrow.decimals)?,
            prices,
        )?;
        let final_tick = if col == 0 || debt == 0 {
            MIN_TICK
        } else {
            tick::position_tick(col, tick::scaled_debt(debt))?
        };

        // Which tick / branch accounts exist decides what to create first.
        let final_tick_key = ix::tick_pda(vault, final_tick);
        let new_branch_id = if l.state.branch_liquidated {
            l.state.total_branch_id + 1
        } else {
            l.state.current_branch_id
        };
        let probe = [
            final_tick_key,
            ix::branch_pda(vault, new_branch_id),
            ix::branch_pda(vault, l.state.current_branch_id),
            ix::tick_pda(vault, current_tick),
        ];
        let found = self.rpc.get_multiple_accounts(&probe)?;
        let mut inits = Vec::new();
        if found[3].is_none() {
            // A new position starts at the bottom tick, which may not exist yet.
            inits.push(ix::init_tick(owner, vault, current_tick));
        }
        let final_ids = match (final_tick == current_tick, &found[0]) {
            (true, _) => current_ids,
            (false, Some(a)) => parse_tick(&a.data)?.total_ids,
            (false, None) => {
                inits.push(ix::init_tick(owner, vault, final_tick));
                0
            }
        };
        let current_tick_id = ix::tick_id_liquidation_pda(vault, current_tick, current_ids);
        let final_tick_id = ix::tick_id_liquidation_pda(vault, final_tick, final_ids);
        let ids =
            [(current_tick_id, current_tick, current_ids), (final_tick_id, final_tick, final_ids)];
        let ids_found = self.rpc.get_multiple_accounts(&[current_tick_id, final_tick_id])?;
        for (i, (key, tick, total_ids)) in ids.iter().enumerate() {
            if ids_found[i].is_none() && (i == 0 || *key != current_tick_id) {
                inits.push(ix::init_tick_id_liquidation(owner, vault, *tick, *total_ids));
            }
        }
        if found[1].is_none() {
            inits.push(ix::init_branch(owner, vault, new_branch_id));
        }
        let branches = if l.state.current_branch_id > 0 && found[2].is_some() {
            vec![probe[2]]
        } else {
            vec![]
        };

        let operate = ix::operate(
            &OperateAccounts {
                signer: *owner,
                vault_id: vault,
                oracle: l.r.config.oracle,
                oracle_program: l.r.config.oracle_program,
                supply: l.supply,
                borrow: l.borrow,
                position: self.position,
                position_token_account: l.position_token_account,
                current_tick,
                current_tick_id,
                final_tick,
                final_tick_id,
                new_branch: probe[1],
                oracle_sources: l.r.oracle.sources.clone(),
                branches,
                tick_has_debt: self.tick_has_debt_arrays(
                    vault,
                    current_tick,
                    final_tick,
                    &l.state,
                )?,
            },
            new_col,
            new_debt,
        );
        Ok((inits, operate))
    }

    /// Bitmap accounts `operate` may touch. Taking the position out of the
    /// vault's top tick can make the program search down for the next tick
    /// with debt, so then it needs every array down to that tick.
    fn tick_has_debt_arrays(
        &self,
        vault: u16,
        current: i32,
        final_tick: i32,
        state: &VaultState,
    ) -> Result<Vec<Pubkey>> {
        let mut indices = Vec::new();
        if current == state.topmost_tick {
            let top = tick_has_debt_index(current);
            let keys: Vec<Pubkey> = (0..=top).map(|i| ix::tick_has_debt_pda(vault, i)).collect();
            let arrays = self.rpc.get_multiple_accounts(&keys)?;
            let next = arrays
                .iter()
                .rev()
                .flatten()
                .find_map(|a| parse_tick_has_debt(&a.data).ok()?.highest_below(current))
                .unwrap_or(MIN_TICK);
            indices.extend(tick_has_debt_index(next)..=(top + 1).min(MAX_ARRAY_INDEX));
        } else {
            indices.push(tick_has_debt_index(current));
        }
        let final_index = tick_has_debt_index(final_tick);
        if !indices.contains(&final_index) {
            indices.push(final_index);
        }
        Ok(indices.into_iter().map(|i| ix::tick_has_debt_pda(vault, i)).collect())
    }

    /// Create the owner's token accounts only when missing.
    pub(crate) fn missing_atas(&self, owner: &Pubkey, l: &Loaded) -> Result<Vec<Instruction>> {
        let tokens = [l.supply, l.borrow];
        let atas: Vec<Pubkey> = tokens.iter().map(|t| t.ata(owner)).collect();
        let existing = self.rpc.get_multiple_accounts(&atas)?;
        Ok(tokens
            .iter()
            .zip(existing)
            .filter(|(_, account)| account.is_none())
            .map(|(t, _)| create_ata_idempotent(owner, &t.mint, &t.program))
            .collect())
    }
}

impl Execute for JupiterVenue {
    fn delever(&self, ctx: &ExecContext, sell: f64, ripcord: bool) -> Result<Executed> {
        let owner = ctx.signer.pubkey();
        let l = self.load(&owner)?;
        let snap = l.snapshot()?;
        let sell_tokens = sell / snap.price();
        ensure!(
            sell_tokens > 0.0 && sell_tokens < snap.collateral,
            "can't sell {sell_tokens} of {} collateral",
            snap.collateral
        );
        let sell_raw = base_units(sell_tokens, l.supply.decimals);
        let slippage = if ripcord { ctx.ripcord_slippage_bps } else { ctx.slippage_bps };
        let quote = ctx.jupiter.quote(
            &l.supply.mint,
            &l.borrow.mint,
            sell_raw,
            slippage,
            MAX_SWAP_ACCOUNTS,
        )?;
        let debt_raw = base_units(snap.debt, l.borrow.decimals);
        let amounts = size_delever(sell_raw, quote.min_out, debt_raw, l.flash_fee_bps)?;
        let swap = ctx.jupiter.swap_instructions(&quote, &owner)?;
        let (inits, operate) =
            self.plan_operate(&owner, &l, -(sell_raw as i128), -(amounts.repay as i128))?;

        let mut ixs = self.missing_atas(&owner, &l)?;
        ixs.extend(inits);
        ixs.push(ix::flashloan_borrow(&owner, &l.borrow, amounts.flash));
        ixs.push(operate);
        ixs.extend(swap.setup);
        ixs.push(swap.swap);
        ixs.extend(swap.cleanup);
        ixs.push(ix::flashloan_payback(&owner, &l.borrow, amounts.flash));

        let tables: Vec<Pubkey> = swap.lookup_tables.iter().copied().chain(l.vault_table).collect();
        let (coll_sym, debt_sym) = (&snap.collateral_symbol, &snap.debt_symbol);
        ctx.run(&ixs, &tables, |units| {
            format!(
                "{}sell {:.6} {coll_sym} → ≥{:.4} {debt_sym} (quote {:.4}, impact {:.3}%), repay {:.4} {debt_sym}, {units} CU",
                if ripcord { "RIPCORD " } else { "" },
                scale_down(amounts.sell, l.supply.decimals),
                scale_down(quote.min_out, l.borrow.decimals),
                scale_down(quote.out_amount, l.borrow.decimals),
                quote.price_impact_pct * 100.0,
                scale_down(amounts.repay, l.borrow.decimals),
            )
        })
    }

    fn relever(&self, ctx: &ExecContext, borrow: f64) -> Result<Executed> {
        let owner = ctx.signer.pubkey();
        let l = self.load(&owner)?;
        let snap = l.snapshot()?;
        let flash_raw = base_units(borrow, l.borrow.decimals);
        let quote = ctx.jupiter.quote(
            &l.borrow.mint,
            &l.supply.mint,
            flash_raw,
            ctx.slippage_bps,
            MAX_SWAP_ACCOUNTS,
        )?;
        let amounts = size_relever(flash_raw, quote.min_out, l.flash_fee_bps)?;
        let swap = ctx.jupiter.swap_instructions(&quote, &owner)?;
        let (inits, operate) =
            self.plan_operate(&owner, &l, amounts.deposit as i128, amounts.borrow as i128)?;

        let mut ixs = self.missing_atas(&owner, &l)?;
        ixs.extend(inits);
        ixs.push(ix::flashloan_borrow(&owner, &l.borrow, amounts.flash));
        ixs.extend(swap.setup);
        ixs.push(swap.swap);
        ixs.extend(swap.cleanup);
        ixs.push(operate);
        ixs.push(ix::flashloan_payback(&owner, &l.borrow, amounts.flash));

        let tables: Vec<Pubkey> = swap.lookup_tables.iter().copied().chain(l.vault_table).collect();
        let (coll_sym, debt_sym) = (&snap.collateral_symbol, &snap.debt_symbol);
        ctx.run(&ixs, &tables, |units| {
            format!(
                "borrow {:.4} {debt_sym} → ≥{:.6} {coll_sym} (quote {:.6}, impact {:.3}%), deposit {:.6} {coll_sym}, {units} CU",
                scale_down(amounts.borrow, l.borrow.decimals),
                scale_down(quote.min_out, l.supply.decimals),
                scale_down(quote.out_amount, l.supply.decimals),
                quote.price_impact_pct * 100.0,
                scale_down(amounts.deposit, l.supply.decimals),
            )
        })
    }

    fn close(&self, ctx: &ExecContext) -> Result<Executed> {
        let owner = ctx.signer.pubkey();
        let l = self.load(&owner)?;
        let snap = l.snapshot()?;
        let flash = (snap.debt * (1.0 + CLOSE_DEBT_MARGIN) * 10f64.powi(l.borrow.decimals as i32))
            .ceil() as u64
            + 2;
        let need = flash + flash_fee(flash, l.flash_fee_bps);
        // Withdrawing everything can come out a unit short of the snapshot.
        let coll_raw = base_units(snap.collateral, l.supply.decimals).saturating_sub(2);
        let estimate = (scale_down(need, l.borrow.decimals) / snap.price()
            * (1.0 + ctx.slippage_bps as f64 / 10_000.0 + 0.002)
            * 10f64.powi(l.supply.decimals as i32))
        .ceil() as u64;
        let quote = ctx.jupiter.quote_covering(
            &l.supply.mint,
            &l.borrow.mint,
            need,
            estimate,
            coll_raw,
            ctx.slippage_bps,
            MAX_SWAP_ACCOUNTS,
        )?;
        let swap = ctx.jupiter.swap_instructions(&quote, &owner)?;
        let (inits, operate) = self.plan_operate(&owner, &l, ALL, ALL)?;

        let mut ixs = self.missing_atas(&owner, &l)?;
        ixs.extend(inits);
        ixs.push(ix::flashloan_borrow(&owner, &l.borrow, flash));
        ixs.push(operate);
        ixs.extend(swap.setup);
        ixs.push(swap.swap);
        ixs.extend(swap.cleanup);
        ixs.push(ix::flashloan_payback(&owner, &l.borrow, flash));
        if l.supply.mint == NATIVE_MINT || l.borrow.mint == NATIVE_MINT {
            ixs.push(unwrap_sol(&owner));
        }

        let tables: Vec<Pubkey> = swap.lookup_tables.iter().copied().chain(l.vault_table).collect();
        let (coll_sym, debt_sym) = (&snap.collateral_symbol, &snap.debt_symbol);
        let sold = scale_down(quote.in_amount, l.supply.decimals);
        ctx.run(&ixs, &tables, |units| {
            format!(
                "repay {:.4} {debt_sym}, withdraw {:.6} {coll_sym}, sell {sold:.6} {coll_sym} → ≥{:.4} {debt_sym} (impact {:.3}%), keep ~{:.6} {coll_sym}, {units} CU",
                snap.debt,
                snap.collateral,
                scale_down(quote.min_out, l.borrow.decimals),
                quote.price_impact_pct * 100.0,
                snap.collateral - sold,
            )
        })
    }

    fn static_accounts(&self, owner: &Pubkey) -> Result<Vec<Pubkey>> {
        let l = self.load(owner)?;
        let vault = l.r.pos.vault_id;
        let config = ix::vault_config_pda(vault);
        let admin = ix::flashloan_admin();
        let mut out = vec![
            ix::VAULTS_PROGRAM_ID,
            ix::LIQUIDITY_PROGRAM_ID,
            ix::FLASHLOAN_PROGRAM_ID,
            l.r.config.oracle_program,
            config,
            ix::vault_state_pda(vault),
            l.r.config.oracle,
            self.position,
            l.position_token_account,
            ix::liquidity(),
            admin,
            ix::user_borrow_position_pda(&l.borrow.mint, &admin),
            ix::branch_pda(vault, l.state.current_branch_id),
            borrowguard_exec::token::ASSOCIATED_TOKEN_PROGRAM_ID,
            system_program::ID,
            sysvar::instructions::ID,
        ];
        out.extend(l.r.oracle.sources.iter().copied());
        for t in [l.supply, l.borrow] {
            out.extend([
                t.mint,
                t.program,
                t.ata(owner),
                t.liquidity_vault(),
                ix::token_reserve_pda(&t.mint),
                ix::rate_model_pda(&t.mint),
            ]);
        }
        out.extend([
            ix::user_supply_position_pda(&l.supply.mint, &config),
            ix::user_borrow_position_pda(&l.borrow.mint, &config),
        ]);
        let mut seen = HashSet::new();
        out.retain(|k| seen.insert(*k));
        if out.len() > 256 {
            bail!("too many accounts for one lookup table");
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRICES: Prices = Prices {
        supply_ex: 1_100_000_000_000, // 1.1
        borrow_ex: 1_250_000_000_000, // 1.25
        borrow_fee: 0,
    };

    #[test]
    fn delever_flash_loan_is_covered_by_the_worst_case_swap() {
        let a = size_delever(1_000_000_000, 99_500_000, 500_000_000, 0).unwrap();
        assert_eq!((a.flash, a.repay, a.flash_fee), (99_500_000, 99_499_999, 0));
        let a = size_delever(1_000_000_000, 99_500_000, 500_000_000, 5).unwrap();
        assert!(a.flash + a.flash_fee <= 99_500_000);
        assert!(a.flash + a.flash_fee + 1 > 99_500_000 - 2, "{a:?}");
        assert_eq!(a.repay, a.flash - 1);
    }

    #[test]
    fn delever_refuses_to_repay_everything_or_dust() {
        assert!(size_delever(1, 10_000_000, 9_000_000, 0).is_err());
        assert!(size_delever(1, 999, 9_000_000, 0).is_err());
    }

    #[test]
    fn relever_deposits_the_guaranteed_output_and_borrows_back_the_fee() {
        let a = size_relever(100_000_000, 950_000_000, 10).unwrap();
        assert_eq!((a.deposit, a.flash_fee, a.borrow), (949_999_999, 100_000, 100_100_000));
        assert!(size_relever(999, 1_000, 0).is_err());
    }

    #[test]
    fn operate_math_rounds_like_the_vault() {
        // Withdraw rounds the collateral removed up, payback rounds the debt
        // removed down and then takes one unit off.
        let (col, debt) =
            apply_operate(1_000_000_000_000, 500_000_000_000, -1_000, -1_000, PRICES).unwrap();
        assert_eq!(col, 1_000_000_000_000 - 910); // ceil(1000 / 1.1)
        assert_eq!(debt, 500_000_000_000 - (800 - 1)); // floor(1000 / 1.25) - 1
                                                       // Deposit rounds down, borrow rounds up and adds the fee.
        let fee = Prices { borrow_fee: 100, ..PRICES };
        let (col, debt) = apply_operate(0, 0, 1_000, 1_000, fee).unwrap();
        assert_eq!((col, debt), (909, 800 + 8));
        assert!(apply_operate(10, 10, -1_000_000, 0, PRICES).is_err());
    }

    #[test]
    fn scales_amounts_to_nine_decimals() {
        assert_eq!(scale(-5, 6).unwrap(), -5_000);
        assert_eq!(scale(7, 9).unwrap(), 7);
        assert!(scale(1, 18).is_err());
    }
}
