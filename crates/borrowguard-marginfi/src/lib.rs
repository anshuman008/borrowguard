//! Project 0 (marginfi v2) venue for BorrowGuard.
//!
//! Snapshots are read by simulating `lending_account_pulse_health` and decoding
//! the account's post-simulation health cache. That makes marginfi itself read
//! the oracles and apply weights, e-mode boosts and confidence haircuts, so
//! BorrowGuard sees the numbers marginfi would liquidate on.

pub mod layout;

use anyhow::{bail, ensure, Context, Result};
use borrowguard_core::{token, Snapshot, Venue};
use solana_account_decoder::UiAccountEncoding;
use solana_client::{
    rpc_client::RpcClient,
    rpc_config::{
        RpcAccountInfoConfig, RpcProgramAccountsConfig, RpcSimulateTransactionAccountsConfig,
        RpcSimulateTransactionConfig,
    },
    rpc_filter::{Memcmp, RpcFilterType},
};
pub use solana_program::pubkey::Pubkey;
use solana_program::{
    instruction::{AccountMeta, Instruction},
    pubkey,
};
use solana_sdk::{commitment_config::CommitmentConfig, message::Message, transaction::Transaction};

use crate::layout::{
    parse_account, parse_bank, Balance, Bank, MarginfiAccount, ACCOUNT_LEN, AUTHORITY_OFFSET,
};

/// marginfi v2 mainnet program (Project 0 runs on it).
pub const PROGRAM_ID: Pubkey = pubkey!("MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA");

/// `sha256("global:lending_account_pulse_health")[..8]`
const PULSE_HEALTH: [u8; 8] = [186, 52, 117, 97, 34, 74, 39, 253];

pub struct MarginfiVenue {
    rpc: RpcClient,
    account: Pubkey,
    /// Fee payer for simulations; defaults to the account authority.
    payer: Option<Pubkey>,
}

impl MarginfiVenue {
    pub fn new(rpc_url: &str, account: Pubkey, payer: Option<Pubkey>) -> Self {
        let rpc =
            RpcClient::new_with_commitment(rpc_url.to_string(), CommitmentConfig::confirmed());
        Self { rpc, account, payer }
    }

    /// The wallet that controls the account.
    pub fn owner(&self) -> Result<Pubkey> {
        Ok(parse_account(&self.rpc.get_account_data(&self.account)?)?.authority)
    }
}

impl Venue for MarginfiVenue {
    fn name(&self) -> &str {
        "marginfi"
    }

    fn snapshot(&self) -> Result<Snapshot> {
        let data = self.rpc.get_account_data(&self.account).with_context(|| {
            format!("fetch marginfi account {} (closed, or not a mainnet address?)", self.account)
        })?;
        let account = parse_account(&data)?;
        single_pair(&account)?;
        let keys: Vec<Pubkey> = account.balances.iter().map(|b| b.bank).collect();
        let mut banks = Vec::with_capacity(keys.len());
        for (key, bank) in keys.iter().zip(self.rpc.get_multiple_accounts(&keys)?) {
            let bank = bank.with_context(|| format!("bank {key} not found"))?;
            banks.push((*key, parse_bank(&bank.data)?));
        }

        let ix = pulse_health(&self.account, &account, &banks)?;
        let payer = self.payer.unwrap_or(account.authority);
        let tx = Transaction::new_unsigned(Message::new(&[ix], Some(&payer)));
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
                        addresses: vec![self.account.to_string()],
                    }),
                    ..Default::default()
                },
            )
            .context("simulate pulse_health")?;
        if let Some(err) = sim.value.err {
            let logs = sim.value.logs.unwrap_or_default().join("\n  ");
            bail!("pulse_health simulation failed: {err}\n  {logs}");
        }
        let fresh = sim
            .value
            .accounts
            .and_then(|accounts| accounts.into_iter().next().flatten())
            .and_then(|a| a.data.decode())
            .context("RPC returned no simulated account")?;
        let banks: Vec<Bank> = banks.into_iter().map(|(_, bank)| bank).collect();
        snapshot_from(&parse_account(&fresh)?, &banks, sim.context.slot)
    }
}

/// Build a snapshot from a freshly pulsed account and its banks, in balance order.
pub fn snapshot_from(account: &MarginfiAccount, banks: &[Bank], slot: u64) -> Result<Snapshot> {
    let h = &account.health;
    ensure!(
        h.engine_ok(),
        "marginfi's risk engine failed (error {}, internal {})",
        h.mrgn_err,
        h.internal_err
    );
    let (deposit, borrow) = single_pair(account)?;
    let bank_of = |b: &Balance| {
        let i = account.balances.iter().position(|x| x.index == b.index).unwrap();
        banks.get(i).context("missing bank for balance")
    };
    let (coll_bank, debt_bank) = (bank_of(deposit)?, bank_of(borrow)?);
    let collateral =
        deposit.asset_shares * coll_bank.asset_share_value / 10f64.powi(coll_bank.decimals as i32);
    let debt = borrow.liability_shares * debt_bank.liability_share_value
        / 10f64.powi(debt_bank.decimals as i32);
    let (collateral_price, debt_price) = (h.prices[deposit.index], h.prices[borrow.index]);
    ensure!(
        collateral_price > 0.0 && debt_price > 0.0,
        "marginfi priced the position at {collateral_price} / {debt_price}"
    );
    let (coll_value, debt_value) = (collateral * collateral_price, debt * debt_price);
    ensure!(
        coll_value > 0.0 && debt_value > 0.0 && h.liability_value_maint > 0.0,
        "account has no collateral value or no debt"
    );
    // The weights the risk engine actually applied (bank weights, e-mode boosts
    // and confidence haircuts), so the liquidation line matches marginfi's.
    let liquidation_ltv =
        (h.asset_value_maint / coll_value) / (h.liability_value_maint / debt_value);
    // Initial weights are applied to EMA prices, so this one is approximate.
    let max_ltv = if h.liability_value > 0.0 {
        (h.asset_value / coll_value) / (h.liability_value / debt_value)
    } else {
        0.0
    };

    Ok(Snapshot {
        collateral_symbol: token::symbol(&coll_bank.mint.to_string()),
        debt_symbol: token::symbol(&debt_bank.mint.to_string()),
        collateral,
        debt,
        collateral_price,
        debt_price,
        liquidation_ltv,
        max_ltv,
        prices_ok: h.oracle_ok(),
        slot,
    })
}

/// BorrowGuard handles exactly one collateral and one debt per account.
fn single_pair(account: &MarginfiAccount) -> Result<(&Balance, &Balance)> {
    let deposits: Vec<_> = account.deposits().collect();
    let borrows: Vec<_> = account.borrows().collect();
    match (deposits.as_slice(), borrows.as_slice()) {
        ([deposit], [borrow]) => Ok((*deposit, *borrow)),
        _ => bail!(
            "BorrowGuard needs exactly one collateral and one debt; account has {} deposits and {} borrows",
            deposits.len(),
            borrows.len()
        ),
    }
}

/// `lending_account_pulse_health`, with each active balance's bank and price
/// accounts in balance order. `banks` must follow `account.balances`.
pub fn pulse_health(
    key: &Pubkey,
    account: &MarginfiAccount,
    banks: &[(Pubkey, Bank)],
) -> Result<Instruction> {
    let mut accounts =
        vec![AccountMeta::new(*key, false), AccountMeta::new_readonly(account.group, false)];
    for (balance, (bank_key, bank)) in account.balances.iter().zip(banks) {
        ensure!(balance.bank == *bank_key, "banks out of order");
        accounts.push(AccountMeta::new_readonly(*bank_key, false));
        for oracle in observation_keys(bank)? {
            accounts.push(AccountMeta::new_readonly(oracle, false));
        }
    }
    Ok(Instruction { program_id: PROGRAM_ID, accounts, data: PULSE_HEALTH.to_vec() })
}

/// Price accounts the risk engine reads after a bank. Port of
/// `bank_observation_keys` in marginfi-v2's p0-cli.
fn observation_keys(bank: &Bank) -> Result<Vec<Pubkey>> {
    let k = &bank.oracle_keys;
    let keys = match bank.oracle_setup {
        // None, Fixed
        0 | 8 => vec![],
        // FixedKamino, FixedDrift, FixedJuplend
        13 | 14 | 17 => vec![k[1]],
        // StakedWithPythPush; the onramp is derived when unset, which we don't do yet.
        5 => {
            ensure!(
                k[3] != Pubkey::default(),
                "native-stake collateral isn't supported yet (bank of {})",
                bank.mint
            );
            vec![k[0], k[1], k[2], k[3]]
        }
        // PythLegacy, SwitchboardV2, PythPushOracle, SwitchboardPull, Scope, PTFixed
        1 | 2 | 3 | 4 | 18 | 26 => vec![k[0]],
        // Kamino/Drift/Solend/Juplend Pyth or Switchboard, ScopeKamino, ScopeJuplend,
        // PythMSOL, PythLST, PTPyth
        6 | 7 | 9 | 10 | 11 | 12 | 15 | 16 | 27 | 28 | 19 | 22 | 25 => vec![k[0], k[1]],
        // KaminoMSOL, JuplendMSOL, KaminoLST, JuplendLST
        20 | 21 | 23 | 24 => vec![k[0], k[1], k[2]],
        other => bail!("unknown marginfi oracle setup {other} (bank of {})", bank.mint),
    };
    Ok(keys.into_iter().filter(|key| *key != Pubkey::default()).collect())
}

/// Every marginfi account controlled by `owner`, as last written on chain.
pub fn find_accounts(rpc_url: &str, owner: &Pubkey) -> Result<Vec<(Pubkey, MarginfiAccount)>> {
    let rpc = RpcClient::new_with_commitment(rpc_url.to_string(), CommitmentConfig::confirmed());
    let accounts = rpc
        .get_program_accounts_with_config(
            &PROGRAM_ID,
            RpcProgramAccountsConfig {
                filters: Some(vec![
                    RpcFilterType::DataSize(ACCOUNT_LEN as u64),
                    RpcFilterType::Memcmp(Memcmp::new_base58_encoded(
                        AUTHORITY_OFFSET,
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
        .context("getProgramAccounts on marginfi (needs an RPC that allows it, e.g. Helius)")?;
    accounts.into_iter().map(|(key, account)| Ok((key, parse_account(&account.data)?))).collect()
}

/// Every bank the accounts use, keyed by address.
pub fn fetch_banks(rpc_url: &str, keys: &[Pubkey]) -> Result<Vec<(Pubkey, Bank)>> {
    let rpc = RpcClient::new_with_commitment(rpc_url.to_string(), CommitmentConfig::confirmed());
    let mut out = Vec::with_capacity(keys.len());
    for chunk in keys.chunks(100) {
        for (key, bank) in chunk.iter().zip(rpc.get_multiple_accounts(chunk)?) {
            if let Some(bank) = bank {
                out.push((*key, parse_bank(&bank.data)?));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::tests::bank_bytes;
    use crate::layout::HealthCache;

    const SOL: &str = "So11111111111111111111111111111111111111112";
    const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

    fn banks() -> Vec<Bank> {
        vec![
            parse_bank(&bank_bytes(SOL.parse().unwrap(), 9, [0.8, 0.9, 1.0, 1.0])).unwrap(),
            parse_bank(&bank_bytes(USDC.parse().unwrap(), 6, [1.0, 1.0, 1.1, 1.05])).unwrap(),
        ]
    }

    /// 30 SOL at $100 against 2,000 USDC, priced with SOL weights 0.8 / 0.9
    /// and USDC liability weights 1.1 / 1.05.
    fn account() -> MarginfiAccount {
        let mut prices = [0.0; 16];
        prices[2] = 100.0;
        prices[5] = 1.0;
        MarginfiAccount {
            group: Pubkey::new_unique(),
            authority: Pubkey::new_unique(),
            balances: vec![
                Balance {
                    index: 2,
                    bank: Pubkey::new_unique(),
                    asset_shares: 30e9,
                    liability_shares: 0.0,
                },
                Balance {
                    index: 5,
                    bank: Pubkey::new_unique(),
                    asset_shares: 0.0,
                    liability_shares: 2_000e6,
                },
            ],
            health: HealthCache {
                asset_value: 3_000.0 * 0.8,
                liability_value: 2_000.0 * 1.1,
                asset_value_maint: 3_000.0 * 0.9,
                liability_value_maint: 2_000.0 * 1.05,
                flags: 7,
                mrgn_err: 0,
                internal_err: 0,
                prices,
            },
        }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn snapshot_reads_amounts_prices_and_thresholds() {
        let snap = snapshot_from(&account(), &banks(), 7).unwrap();
        assert_eq!((snap.collateral_symbol.as_str(), snap.debt_symbol.as_str()), ("SOL", "USDC"));
        assert!(close(snap.collateral, 30.0) && close(snap.debt, 2_000.0));
        assert!(close(snap.price(), 100.0));
        assert!(close(snap.liquidation_ltv, 0.9 / 1.05));
        assert!(close(snap.max_ltv, 0.8 / 1.1));
        assert!(snap.prices_ok);
        assert_eq!(snap.slot, 7);
    }

    #[test]
    fn liquidation_line_matches_the_health_cache() {
        // At the current price, LTV / liquidation LTV must equal what marginfi
        // compares: weighted debt / weighted collateral.
        let a = account();
        let snap = snapshot_from(&a, &banks(), 0).unwrap();
        let ltv = snap.debt * snap.debt_price / (snap.collateral * snap.collateral_price);
        assert!(close(
            ltv / snap.liquidation_ltv,
            a.health.liability_value_maint / a.health.asset_value_maint
        ));
    }

    #[test]
    fn bad_oracles_are_flagged_and_engine_errors_fail() {
        let mut a = account();
        a.health.flags = 3;
        assert!(!snapshot_from(&a, &banks(), 0).unwrap().prices_ok);
        a.health.flags = 0;
        a.health.mrgn_err = 6009;
        let err = snapshot_from(&a, &banks(), 0).unwrap_err().to_string();
        assert!(err.contains("6009"), "{err}");
    }

    #[test]
    fn rejects_multi_asset_accounts() {
        let mut a = account();
        a.balances.push(Balance {
            index: 7,
            bank: Pubkey::new_unique(),
            asset_shares: 1.0,
            liability_shares: 0.0,
        });
        let err = snapshot_from(&a, &banks(), 0).unwrap_err().to_string();
        assert!(err.contains("2 deposits and 1 borrows"), "{err}");
    }

    #[test]
    fn pulse_health_lists_banks_then_their_oracles() {
        let a = account();
        let bank_list: Vec<(Pubkey, Bank)> =
            a.balances.iter().map(|b| b.bank).zip(banks()).collect();
        let ix = pulse_health(&Pubkey::new_unique(), &a, &bank_list).unwrap();
        assert_eq!(ix.data, PULSE_HEALTH);
        let keys: Vec<Pubkey> = ix.accounts.iter().map(|m| m.pubkey).collect();
        assert_eq!(keys.len(), 2 + 2 * 2);
        assert_eq!(keys[1], a.group);
        assert_eq!(keys[2], a.balances[0].bank);
        assert_eq!(keys[3], bank_list[0].1.oracle_keys[0]);
        assert_eq!(keys[4], a.balances[1].bank);
        assert!(ix.accounts[0].is_writable && ix.accounts[2..].iter().all(|m| !m.is_writable));
    }

    #[test]
    fn observation_keys_follow_oracle_setup() {
        let mut bank = banks().remove(0);
        bank.oracle_keys = std::array::from_fn(|_| Pubkey::new_unique());
        for (setup, want) in [(8, 0), (3, 1), (13, 1), (6, 2), (23, 3), (5, 4)] {
            bank.oracle_setup = setup;
            assert_eq!(observation_keys(&bank).unwrap().len(), want, "setup {setup}");
        }
        bank.oracle_setup = 13;
        assert_eq!(observation_keys(&bank).unwrap(), vec![bank.oracle_keys[1]]);
        bank.oracle_setup = 99;
        assert!(observation_keys(&bank).is_err());
    }
}
