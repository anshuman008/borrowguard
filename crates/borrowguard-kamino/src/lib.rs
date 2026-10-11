//! Kamino Lend (klend) venue for BorrowGuard.
//!
//! Snapshots are read by simulating `refresh_reserve` ×2 + `refresh_obligation`
//! and decoding the post-simulation accounts. That makes klend itself do the
//! oracle reads, interest accrual and elevation-group / borrow-factor math, so
//! BorrowGuard sees exactly the numbers klend would liquidate on.

pub mod ix;
pub mod layout;
pub mod reserve;

use anyhow::{bail, ensure, Context, Result};
use borrowguard_core::{Snapshot, Venue};
use solana_account_decoder::UiAccountEncoding;
use solana_client::{
    rpc_client::RpcClient,
    rpc_config::{
        RpcAccountInfoConfig, RpcProgramAccountsConfig, RpcSimulateTransactionAccountsConfig,
        RpcSimulateTransactionConfig,
    },
    rpc_filter::{Memcmp, RpcFilterType},
};
use solana_program::pubkey;
pub use solana_program::pubkey::Pubkey;
use solana_sdk::{commitment_config::CommitmentConfig, message::Message, transaction::Transaction};

use crate::layout::{
    parse_obligation, Obligation, ObligationCollateral, ObligationLiquidity,
    OBLIGATION_ACCOUNT_LEN, OBLIGATION_OWNER_OFFSET,
};
use crate::reserve::{parse_reserve, Reserve};

/// Kamino Lend mainnet program.
pub const PROGRAM_ID: Pubkey = pubkey!("KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD");

pub struct KaminoVenue {
    rpc: RpcClient,
    obligation: Pubkey,
    /// Fee payer for simulations; defaults to the obligation owner.
    payer: Option<Pubkey>,
}

impl KaminoVenue {
    pub fn new(rpc_url: &str, obligation: Pubkey, payer: Option<Pubkey>) -> Self {
        let rpc =
            RpcClient::new_with_commitment(rpc_url.to_string(), CommitmentConfig::confirmed());
        Self { rpc, obligation, payer }
    }
}

impl Venue for KaminoVenue {
    fn name(&self) -> &str {
        "kamino"
    }

    fn snapshot(&self) -> Result<Snapshot> {
        let data = self.rpc.get_account_data(&self.obligation).with_context(|| {
            format!("fetch obligation {} (closed, or not a mainnet address?)", self.obligation)
        })?;
        let obligation = parse_obligation(&data)?;
        let (deposit, borrow) = single_pair(&obligation)?;
        let keys = [self.obligation, deposit.deposit_reserve, borrow.borrow_reserve];

        let mut reserves = Vec::with_capacity(2);
        for (key, account) in keys[1..].iter().zip(self.rpc.get_multiple_accounts(&keys[1..])?) {
            let account = account.with_context(|| format!("reserve {key} not found"))?;
            reserves.push(parse_reserve(&account.data)?);
        }
        let referrer = Some(&obligation.referrer).filter(|r| **r != Pubkey::default());
        let ixs = [
            ix::refresh_reserve(&PROGRAM_ID, &keys[1], &reserves[0]),
            ix::refresh_reserve(&PROGRAM_ID, &keys[2], &reserves[1]),
            ix::refresh_obligation(
                &PROGRAM_ID,
                &obligation.lending_market,
                &self.obligation,
                &keys[1..2],
                &keys[2..3],
                referrer,
            ),
        ];
        let payer = self.payer.unwrap_or(obligation.owner);
        let tx = Transaction::new_unsigned(Message::new(&ixs, Some(&payer)));
        let sim = self
            .rpc
            .simulate_transaction_with_config(
                &tx,
                RpcSimulateTransactionConfig {
                    sig_verify: false,
                    replace_recent_blockhash: true,
                    commitment: Some(CommitmentConfig::confirmed()),
                    accounts: Some(RpcSimulateTransactionAccountsConfig {
                        encoding: Some(UiAccountEncoding::Base64),
                        addresses: keys.iter().map(Pubkey::to_string).collect(),
                    }),
                    ..Default::default()
                },
            )
            .context("simulate refresh")?;
        if let Some(err) = sim.value.err {
            let logs = sim.value.logs.unwrap_or_default().join("\n  ");
            bail!("refresh simulation failed: {err}\n  {logs}");
        }

        let accounts = sim.value.accounts.context("RPC returned no simulated accounts")?;
        let mut fresh = keys.iter().zip(accounts).map(|(key, account)| {
            account
                .and_then(|a| a.data.decode())
                .with_context(|| format!("simulated account {key} missing"))
        });
        let mut next = || fresh.next().context("RPC returned too few simulated accounts")?;
        let obligation = parse_obligation(&next()?)?;
        let collateral = parse_reserve(&next()?)?;
        let debt = parse_reserve(&next()?)?;
        snapshot_from(&obligation, &collateral, &debt, sim.context.slot)
    }
}

/// Build a snapshot from a freshly refreshed obligation and its two reserves.
pub fn snapshot_from(
    obligation: &Obligation,
    collateral: &Reserve,
    debt: &Reserve,
    slot: u64,
) -> Result<Snapshot> {
    let (deposit, borrow) = single_pair(obligation)?;
    ensure!(
        collateral.market_price > 0.0 && debt.market_price > 0.0,
        "reserve has no price ({} {}, {} {})",
        collateral.name,
        collateral.market_price,
        debt.name,
        debt.market_price
    );
    let deposited = obligation.deposited_value_sf.to_f64();
    let borrowed = obligation.borrowed_assets_market_value_sf.to_f64();
    ensure!(deposited > 0.0 && borrowed > 0.0, "obligation has no collateral value or no debt");
    // klend liquidates when debt × borrow factor exceeds collateral × liquidation
    // threshold; fold the borrow factor (1 inside elevation groups) into the LTVs.
    let borrow_factor = obligation.borrow_factor_adjusted_debt_value_sf.to_f64() / borrowed;

    Ok(Snapshot {
        collateral_symbol: collateral.name.clone(),
        debt_symbol: debt.name.clone(),
        collateral: deposit.market_value_sf.to_f64() / collateral.market_price,
        debt: borrow.borrowed_amount_sf.to_f64() / 10f64.powi(debt.decimals as i32),
        collateral_price: collateral.market_price,
        debt_price: debt.market_price,
        liquidation_ltv: obligation.unhealthy_borrow_value_sf.to_f64() / deposited / borrow_factor,
        max_ltv: obligation.allowed_borrow_value_sf.to_f64() / deposited / borrow_factor,
        prices_ok: obligation.last_update.prices_ok(),
        slot,
    })
}

/// BorrowGuard handles exactly one collateral and one debt per obligation.
fn single_pair(obligation: &Obligation) -> Result<(&ObligationCollateral, &ObligationLiquidity)> {
    let deposits: Vec<_> = obligation.active_deposits().collect();
    let borrows: Vec<_> = obligation.active_borrows().collect();
    match (deposits.as_slice(), borrows.as_slice()) {
        ([deposit], [borrow]) => Ok((*deposit, *borrow)),
        _ => bail!(
            "BorrowGuard needs exactly one collateral and one debt; obligation has {} deposits and {} borrows",
            deposits.len(),
            borrows.len()
        ),
    }
}

/// Every klend obligation owned by `owner`, with its cached (last-refresh) state.
pub fn find_obligations(rpc_url: &str, owner: &Pubkey) -> Result<Vec<(Pubkey, Obligation)>> {
    let rpc = RpcClient::new_with_commitment(rpc_url.to_string(), CommitmentConfig::confirmed());
    let accounts = rpc
        .get_program_accounts_with_config(
            &PROGRAM_ID,
            RpcProgramAccountsConfig {
                filters: Some(vec![
                    RpcFilterType::DataSize(OBLIGATION_ACCOUNT_LEN as u64),
                    RpcFilterType::Memcmp(Memcmp::new_base58_encoded(
                        OBLIGATION_OWNER_OFFSET,
                        owner.as_ref(),
                    )),
                ]),
                account_config: RpcAccountInfoConfig {
                    encoding: Some(UiAccountEncoding::Base64),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .context("getProgramAccounts on klend (needs an RPC that allows it, e.g. Helius)")?;
    accounts.into_iter().map(|(key, account)| Ok((key, parse_obligation(&account.data)?))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::U128Le;
    use crate::reserve::tests::reserve_bytes;
    use bytemuck::Zeroable;

    fn sf(v: f64) -> U128Le {
        U128Le::new((v * (1u128 << 60) as f64) as u128)
    }

    /// $3,000 of SOL against 2,000 USDC: liquidation threshold 80%, max LTV 75%.
    fn obligation(borrow_factor: f64) -> Obligation {
        let mut o = Obligation::zeroed();
        o.deposits[0].deposit_reserve = Pubkey::new_unique();
        o.deposits[0].deposited_amount = 1;
        o.deposits[0].market_value_sf = sf(3_000.0);
        o.borrows[0].borrow_reserve = Pubkey::new_unique();
        o.borrows[0].borrowed_amount_sf = sf(2_000.0 * 1e6);
        o.deposited_value_sf = sf(3_000.0);
        o.borrowed_assets_market_value_sf = sf(2_000.0);
        o.borrow_factor_adjusted_debt_value_sf = sf(2_000.0 * borrow_factor);
        o.unhealthy_borrow_value_sf = sf(3_000.0 * 0.80);
        o.allowed_borrow_value_sf = sf(3_000.0 * 0.75);
        o.last_update.price_status = 0b0011_1111;
        o
    }

    fn reserves() -> (Reserve, Reserve) {
        (
            parse_reserve(&reserve_bytes("SOL", 100.0, 9)).unwrap(),
            parse_reserve(&reserve_bytes("USDC", 1.0, 6)).unwrap(),
        )
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn snapshot_reads_amounts_and_thresholds() {
        let (sol, usdc) = reserves();
        let snap = snapshot_from(&obligation(1.0), &sol, &usdc, 42).unwrap();
        assert_eq!((snap.collateral_symbol.as_str(), snap.debt_symbol.as_str()), ("SOL", "USDC"));
        assert!(close(snap.collateral, 30.0));
        assert!(close(snap.debt, 2_000.0));
        assert!(close(snap.position().leverage(snap.price()).unwrap(), 3.0));
        assert!(close(snap.liquidation_ltv, 0.80));
        assert!(close(snap.liquidation_leverage(), 5.0));
        assert!(close(snap.max_ltv, 0.75));
        assert!(snap.prices_ok);
        assert_eq!(snap.slot, 42);
    }

    #[test]
    fn borrow_factor_tightens_thresholds() {
        let (sol, usdc) = reserves();
        let snap = snapshot_from(&obligation(1.25), &sol, &usdc, 0).unwrap();
        assert!(close(snap.liquidation_ltv, 0.80 / 1.25));
        assert!(close(snap.max_ltv, 0.75 / 1.25));
    }

    #[test]
    fn rejects_multi_asset_obligations() {
        let (sol, usdc) = reserves();
        let mut o = obligation(1.0);
        o.deposits[1] = o.deposits[0];
        o.deposits[1].deposit_reserve = Pubkey::new_unique();
        let err = snapshot_from(&o, &sol, &usdc, 0).unwrap_err().to_string();
        assert!(err.contains("2 deposits and 1 borrows"), "{err}");
    }
}
