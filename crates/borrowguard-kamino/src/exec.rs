//! Kamino delever: one atomic transaction that pays down debt with the
//! position's own collateral, funded by a klend flash loan.
//!
//! ```text
//! create ATAs → flash_borrow(debt) → refresh → repay_v2 → refresh
//! → withdraw_v2(collateral) → Jupiter swap collateral→debt → flash_repay
//! ```
//!
//! The repay amount is what the swap's *guaranteed* output covers (Jupiter's
//! slippage floor, net of the flash fee), so the flash loan can always be
//! repaid and any extra swap output stays in the wallet.

use anyhow::{ensure, Context, Result};
use borrowguard_core::{Snapshot, Venue};
use borrowguard_exec::{tx::COMPUTE_BUDGET_IXS, ExecContext, Execute, Executed};
use solana_program::{instruction::Instruction, pubkey::Pubkey, system_program, sysvar};
use solana_sdk::signer::Signer;

use crate::ix;
use crate::layout::{parse_obligation, Obligation};
use crate::reserve::{parse_reserve, Reserve};
use crate::{single_pair, KaminoVenue, PROGRAM_ID};

/// Route size cap so the swap fits beside ~10 klend instructions.
const MAX_SWAP_ACCOUNTS: u8 = 20;

/// Amounts for one delever, all in token base units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeleverAmounts {
    /// Collateral liquidity withdrawn and swapped.
    pub sell: u64,
    /// Reserve collateral (cTokens) withdrawn to get at least `sell`.
    pub ctokens: u64,
    /// Debt repaid, equal to the flash loan.
    pub repay: u64,
    /// Flash loan fee on top of `repay`.
    pub flash_fee: u64,
}

/// Size a delever from a quote's guaranteed output.
pub fn size_delever(
    sell: u64,
    min_swap_out: u64,
    debt_outstanding: u64,
    collateral: &Reserve,
    debt: &Reserve,
) -> Result<DeleverAmounts> {
    let repay = (min_swap_out as f64 / (1.0 + debt.flash_loan_fee)).floor() as u64;
    ensure!(repay > 0, "swap output too small to repay anything");
    ensure!(
        repay < debt_outstanding,
        "delever would repay all debt ({repay} of {debt_outstanding}); close the position instead"
    );
    let flash_fee = (repay as f64 * debt.flash_loan_fee).ceil() as u64;
    ensure!(repay + flash_fee <= min_swap_out, "flash fee leaves the loan uncovered");
    let ctokens = collateral
        .collateral_for_liquidity(sell)
        .context("collateral reserve has no liquidity to redeem")?;
    Ok(DeleverAmounts { sell, ctokens, repay, flash_fee })
}

/// Amounts for one relever, all in token base units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleverAmounts {
    /// Debt token flash-borrowed and swapped into collateral.
    pub flash: u64,
    /// Collateral deposited: the swap's guaranteed output.
    pub deposit: u64,
    /// Debt borrowed against the new collateral to repay the flash loan.
    pub borrow: u64,
    pub flash_fee: u64,
}

/// Size a relever: borrow back exactly the flash loan plus its fee, and
/// deposit only what the swap guarantees (any extra stays in the wallet).
pub fn size_relever(flash: u64, min_swap_out: u64, debt: &Reserve) -> Result<ReleverAmounts> {
    ensure!(flash > 0, "nothing to borrow");
    ensure!(min_swap_out > 0, "swap output too small to deposit anything");
    let flash_fee = (flash as f64 * debt.flash_loan_fee).ceil() as u64;
    Ok(ReleverAmounts { flash, deposit: min_swap_out, borrow: flash + flash_fee, flash_fee })
}

/// Position state a rebalance needs, read fresh.
struct Loaded {
    obligation: Obligation,
    coll_key: Pubkey,
    debt_key: Pubkey,
    coll: Reserve,
    debt: Reserve,
    /// Outstanding debt, base units.
    debt_raw: u64,
    snap: Snapshot,
    coll_ata: Pubkey,
    debt_ata: Pubkey,
}

impl KaminoVenue {
    fn load(&self, owner: &Pubkey) -> Result<Loaded> {
        let obligation = parse_obligation(&self.rpc.get_account_data(&self.obligation)?)?;
        ensure!(
            obligation.owner == *owner,
            "signer {owner} doesn't own this obligation (owner {})",
            obligation.owner
        );
        let (deposit, borrow) = single_pair(&obligation)?;
        let (coll_key, debt_key) = (deposit.deposit_reserve, borrow.borrow_reserve);
        let debt_raw = (borrow.borrowed_amount_sf.get() >> 60) as u64;
        let mut reserves = self.rpc.get_multiple_accounts(&[coll_key, debt_key])?.into_iter();
        let mut next = || -> Result<Reserve> {
            Ok(parse_reserve(&reserves.next().flatten().context("reserve not found")?.data)?)
        };
        let (coll, debt) = (next()?, next()?);
        // Price and amounts from klend's own refreshed view.
        let snap = self.snapshot()?;
        let coll_ata = ix::associated_token_address(owner, &coll.mint, &coll.token_program);
        let debt_ata = ix::associated_token_address(owner, &debt.mint, &debt.token_program);
        Ok(Loaded {
            obligation,
            coll_key,
            debt_key,
            coll,
            debt,
            debt_raw,
            snap,
            coll_ata,
            debt_ata,
        })
    }

    /// klend wants reserves and the obligation refreshed right before each
    /// action that reads them, and again after an action marks them stale.
    fn refresh(&self, l: &Loaded) -> Vec<Instruction> {
        vec![
            ix::refresh_reserve(&PROGRAM_ID, &l.coll_key, &l.coll),
            ix::refresh_reserve(&PROGRAM_ID, &l.debt_key, &l.debt),
            ix::refresh_obligation(
                &PROGRAM_ID,
                &l.obligation.lending_market,
                &self.obligation,
                &[l.coll_key],
                &[l.debt_key],
                Some(&l.obligation.referrer).filter(|r| **r != Default::default()),
            ),
        ]
    }

    /// Create token accounts only when missing: every byte counts in a
    /// transaction that also carries a swap route.
    fn missing_atas(&self, owner: &Pubkey, l: &Loaded) -> Result<Vec<Instruction>> {
        let existing = self.rpc.get_multiple_accounts(&[l.coll_ata, l.debt_ata])?;
        Ok([(&l.coll, &existing[0]), (&l.debt, &existing[1])]
            .into_iter()
            .filter(|(_, account)| account.is_none())
            .map(|(reserve, _)| {
                ix::create_ata_idempotent(owner, &reserve.mint, &reserve.token_program)
            })
            .collect())
    }
}

fn scale(raw: u64, decimals: u32) -> f64 {
    raw as f64 / 10f64.powi(decimals as i32)
}

impl Execute for KaminoVenue {
    fn delever(&self, ctx: &ExecContext, sell: f64, ripcord: bool) -> Result<Executed> {
        let owner = ctx.signer.pubkey();
        let l = self.load(&owner)?;
        let sell_tokens = sell / l.snap.price();
        ensure!(
            sell_tokens > 0.0 && sell_tokens < l.snap.collateral,
            "can't sell {sell_tokens} of {} collateral",
            l.snap.collateral
        );
        let sell_raw = (sell_tokens * 10f64.powi(l.coll.decimals as i32)).floor() as u64;

        let slippage = if ripcord { ctx.ripcord_slippage_bps } else { ctx.slippage_bps };
        let quote =
            ctx.jupiter.quote(&l.coll.mint, &l.debt.mint, sell_raw, slippage, MAX_SWAP_ACCOUNTS)?;
        let amounts = size_delever(sell_raw, quote.min_out, l.debt_raw, &l.coll, &l.debt)?;
        let swap = ctx.jupiter.swap_instructions(&quote, &owner)?;

        let mut ixs = self.missing_atas(&owner, &l)?;
        let borrow_index = (COMPUTE_BUDGET_IXS + ixs.len()) as u8;
        ixs.push(ix::flash_borrow(
            &PROGRAM_ID,
            &owner,
            &l.debt_key,
            &l.debt,
            &l.debt_ata,
            amounts.repay,
        ));
        ixs.extend(self.refresh(&l));
        ixs.push(ix::repay_v2(
            &PROGRAM_ID,
            &owner,
            &self.obligation,
            &l.debt_key,
            &l.debt,
            &l.debt_ata,
            amounts.repay,
        ));
        ixs.extend(self.refresh(&l));
        ixs.push(ix::withdraw_v2(
            &PROGRAM_ID,
            &owner,
            &self.obligation,
            &l.coll_key,
            &l.coll,
            &l.coll_ata,
            amounts.ctokens,
        ));
        ixs.extend(swap.setup);
        ixs.push(swap.swap);
        ixs.extend(swap.cleanup);
        ixs.push(ix::flash_repay(
            &PROGRAM_ID,
            &owner,
            &l.debt_key,
            &l.debt,
            &l.debt_ata,
            amounts.repay,
            borrow_index,
        ));

        let (coll_sym, debt_sym) = (&l.snap.collateral_symbol, &l.snap.debt_symbol);
        ctx.run(&ixs, &swap.lookup_tables, |units| {
            format!(
                "{}sell {:.6} {coll_sym} → ≥{:.4} {debt_sym} (quote {:.4}, impact {:.3}%), repay {:.4} {debt_sym}, {units} CU",
                if ripcord { "RIPCORD " } else { "" },
                scale(amounts.sell, l.coll.decimals),
                scale(quote.min_out, l.debt.decimals),
                scale(quote.out_amount, l.debt.decimals),
                quote.price_impact_pct * 100.0,
                scale(amounts.repay, l.debt.decimals),
            )
        })
    }

    fn relever(&self, ctx: &ExecContext, borrow: f64) -> Result<Executed> {
        let owner = ctx.signer.pubkey();
        let l = self.load(&owner)?;
        let flash_raw = (borrow * 10f64.powi(l.debt.decimals as i32)).floor() as u64;

        let quote = ctx.jupiter.quote(
            &l.debt.mint,
            &l.coll.mint,
            flash_raw,
            ctx.slippage_bps,
            MAX_SWAP_ACCOUNTS,
        )?;
        let amounts = size_relever(flash_raw, quote.min_out, &l.debt)?;
        let swap = ctx.jupiter.swap_instructions(&quote, &owner)?;
        // Elevation-group obligations must show their deposits to borrow.
        let emode_deposits =
            if l.obligation.elevation_group > 0 { vec![l.coll_key] } else { vec![] };

        let mut ixs = self.missing_atas(&owner, &l)?;
        let borrow_index = (COMPUTE_BUDGET_IXS + ixs.len()) as u8;
        ixs.push(ix::flash_borrow(
            &PROGRAM_ID,
            &owner,
            &l.debt_key,
            &l.debt,
            &l.debt_ata,
            amounts.flash,
        ));
        ixs.extend(swap.setup);
        ixs.push(swap.swap);
        ixs.extend(swap.cleanup);
        ixs.extend(self.refresh(&l));
        ixs.push(ix::deposit_v2(
            &PROGRAM_ID,
            &owner,
            &self.obligation,
            &l.coll_key,
            &l.coll,
            &l.coll_ata,
            amounts.deposit,
        ));
        ixs.extend(self.refresh(&l));
        ixs.push(ix::borrow_v2(
            &PROGRAM_ID,
            &owner,
            &self.obligation,
            &l.debt_key,
            &l.debt,
            &l.debt_ata,
            amounts.borrow,
            &emode_deposits,
        ));
        ixs.push(ix::flash_repay(
            &PROGRAM_ID,
            &owner,
            &l.debt_key,
            &l.debt,
            &l.debt_ata,
            amounts.flash,
            borrow_index,
        ));

        let (coll_sym, debt_sym) = (&l.snap.collateral_symbol, &l.snap.debt_symbol);
        ctx.run(&ixs, &swap.lookup_tables, |units| {
            format!(
                "borrow {:.4} {debt_sym} → ≥{:.6} {coll_sym} (quote {:.6}, impact {:.3}%), deposit {:.6} {coll_sym}, {units} CU",
                scale(amounts.borrow, l.debt.decimals),
                scale(quote.min_out, l.coll.decimals),
                scale(quote.out_amount, l.coll.decimals),
                quote.price_impact_pct * 100.0,
                scale(amounts.deposit, l.coll.decimals),
            )
        })
    }

    fn static_accounts(&self, owner: &Pubkey) -> Result<Vec<Pubkey>> {
        let obligation = parse_obligation(&self.rpc.get_account_data(&self.obligation)?)?;
        let (deposit, borrow) = single_pair(&obligation)?;
        let keys = [deposit.deposit_reserve, borrow.borrow_reserve];
        let mut reserves = Vec::with_capacity(2);
        for account in self.rpc.get_multiple_accounts(&keys)? {
            reserves.push(parse_reserve(&account.context("reserve not found")?.data)?);
        }
        let market = obligation.lending_market;
        let mut out = vec![
            PROGRAM_ID,
            market,
            ix::lending_market_authority(&PROGRAM_ID, &market),
            self.obligation,
            ix::FARMS_PROGRAM_ID,
            ix::ASSOCIATED_TOKEN_PROGRAM_ID,
            system_program::ID,
            sysvar::instructions::ID,
        ];
        for (key, r) in keys.iter().zip(&reserves) {
            out.extend([
                *key,
                r.mint,
                r.supply_vault,
                r.fee_vault,
                r.collateral_mint,
                r.collateral_supply_vault,
                r.token_program,
                ix::associated_token_address(owner, &r.mint, &r.token_program),
            ]);
            out.extend(
                [r.pyth_price, r.switchboard_price, r.switchboard_twap, r.scope_prices]
                    .into_iter()
                    .flatten(),
            );
        }
        for (farm, _) in [(reserves[0].farm_collateral, ()), (reserves[1].farm_debt, ())] {
            if let Some(farm) = farm {
                out.extend([farm, ix::obligation_farm_user_state(&farm, &self.obligation)]);
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reserve::tests::reserve_bytes;

    fn reserves(flash_fee: f64) -> (Reserve, Reserve) {
        let mut coll = parse_reserve(&reserve_bytes("SOL", 100.0, 9)).unwrap();
        coll.collateral_mint_supply = 1_000_000;
        coll.total_liquidity = 1_100_000; // 1.1 liquidity per cToken
        let mut debt = parse_reserve(&reserve_bytes("USDC", 1.0, 6)).unwrap();
        debt.flash_loan_fee = flash_fee;
        (coll, debt)
    }

    #[test]
    fn repays_what_the_worst_case_swap_covers() {
        let (coll, debt) = reserves(0.0);
        let a = size_delever(110_000, 9_950_000, 50_000_000, &coll, &debt).unwrap();
        assert_eq!((a.repay, a.flash_fee), (9_950_000, 0));
        assert!(
            a.ctokens as u128 * coll.total_liquidity / coll.collateral_mint_supply as u128
                >= 110_000
        );
    }

    #[test]
    fn leaves_room_for_the_flash_fee() {
        let (coll, debt) = reserves(0.001);
        let a = size_delever(110_000, 10_000_000, 50_000_000, &coll, &debt).unwrap();
        assert!(a.repay + a.flash_fee <= 10_000_000);
        assert!(a.repay > 9_980_000);
    }

    #[test]
    fn relever_borrows_back_flash_loan_plus_fee() {
        let (_, debt) = reserves(0.001);
        let a = size_relever(100_000_000, 950_000_000, &debt).unwrap();
        assert_eq!((a.flash, a.deposit, a.flash_fee), (100_000_000, 950_000_000, 100_000));
        assert_eq!(a.borrow, 100_100_000);
        assert!(size_relever(0, 1, &debt).is_err());
        assert!(size_relever(1, 0, &debt).is_err());
    }

    #[test]
    fn refuses_to_repay_everything() {
        let (coll, debt) = reserves(0.0);
        assert!(size_delever(110_000, 10_000_000, 10_000_000, &coll, &debt).is_err());
        assert!(size_delever(110_000, 0, 10_000_000, &coll, &debt).is_err());
    }
}
