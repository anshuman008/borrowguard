//! Jupiter Lend (Fluid vaults) venue for BorrowGuard.
//!
//! A position stores its collateral and a tick (debt/collateral ratio
//! bucket); debt is derived from the two. Snapshots simulate the protocol's
//! own view instructions instead of re-implementing them:
//! - vaults `get_exchange_prices`: raw units → token amounts, with interest
//!   accrued up to now;
//! - oracle `get_exchange_rate_liquidate`: the exact price the vault
//!   liquidates on (fails if any oracle source is stale).

pub mod exec;
pub mod ix;
pub mod layout;
pub mod tick;

use std::sync::OnceLock;

use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use borrowguard_core::{Snapshot, Venue};
use solana_account_decoder::{UiAccountData, UiAccountEncoding};
use solana_client::{
    rpc_client::RpcClient,
    rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig, RpcSimulateTransactionConfig},
    rpc_filter::{Memcmp, RpcFilterType},
    rpc_request::TokenAccountsFilter,
};
use solana_program::{
    instruction::{AccountMeta, Instruction},
    pubkey,
    pubkey::Pubkey,
};
use solana_sdk::{commitment_config::CommitmentConfig, message::Message, transaction::Transaction};

use crate::layout::{
    discriminator, parse_oracle, parse_position, parse_tick, parse_vault_config,
    token_account_owner, Oracle, Position, Tick, VaultConfig, POSITION_ACCOUNT_LEN,
    POSITION_MINT_OFFSET,
};

pub use crate::ix::{position_pda, LIQUIDITY_PROGRAM_ID, ORACLE_PROGRAM_ID, VAULTS_PROGRAM_ID};
use crate::ix::{tick_pda, token_reserve_pda, vault_config_pda, vault_state_pda};
use crate::tick::MIN_TICK;

/// Vault amounts are scaled to 9 decimals regardless of the token's own.
const AMOUNT_SCALE: f64 = 1e9;
const EXCHANGE_PRICES_PRECISION: f64 = 1e12;
/// Oracle rates are debt-per-collateral with 15 decimals.
const RATE_SCALE: f64 = 1e15;

const TOKEN_PROGRAMS: [Pubkey; 2] = [
    pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"),
    pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"),
];

pub struct JupiterVenue {
    rpc: RpcClient,
    position: Pubkey,
    /// Fee payer for simulations. Defaults to the position NFT's holder,
    /// looked up once: public RPCs throttle that lookup hard.
    payer: OnceLock<Pubkey>,
}

impl JupiterVenue {
    pub fn new(rpc_url: &str, position: Pubkey, payer: Option<Pubkey>) -> Self {
        let rpc =
            RpcClient::new_with_commitment(rpc_url.to_string(), CommitmentConfig::confirmed());
        Self { rpc, position, payer: payer.map(OnceLock::from).unwrap_or_default() }
    }

    /// Simulate one view instruction and return its Anchor return data.
    fn view(&self, ix: Instruction, payer: &Pubkey) -> Result<(Vec<u8>, u64)> {
        let program = ix.program_id;
        let tx = Transaction::new_unsigned(Message::new(&[ix], Some(payer)));
        let sim = self.rpc.simulate_transaction_with_config(
            &tx,
            RpcSimulateTransactionConfig {
                sig_verify: false,
                replace_recent_blockhash: true,
                commitment: Some(CommitmentConfig::confirmed()),
                ..Default::default()
            },
        )?;
        if let Some(err) = sim.value.err {
            let logs = sim.value.logs.unwrap_or_default().join("\n  ");
            bail!("{program} view simulation failed: {err}\n  {logs}");
        }
        let data = sim.value.return_data.context("view instruction returned no data")?;
        ensure!(
            data.program_id == program.to_string(),
            "return data came from {}",
            data.program_id
        );
        Ok((STANDARD.decode(data.data.0)?, sim.context.slot))
    }
}

/// A position and its vault, priced as the vault program sees them now.
pub(crate) struct Reading {
    pub pos: Position,
    pub config: VaultConfig,
    pub tick: Tick,
    pub oracle: Oracle,
    /// Vault supply / borrow exchange prices, 1e12, interest accrued to now.
    pub supply_ex: u128,
    pub borrow_ex: u128,
    /// Collateral price in debt tokens, 1e15.
    pub rate: u128,
    pub slot: u64,
}

impl Venue for JupiterVenue {
    fn name(&self) -> &str {
        "jupiter"
    }

    fn snapshot(&self) -> Result<Snapshot> {
        let r = self.read()?;
        snapshot_from(&r.pos, &r.config, r.supply_ex, r.borrow_ex, r.rate, r.slot)
    }

    fn watch_accounts(&self) -> Result<Vec<[u8; 32]>> {
        let pos = parse_position(&self.rpc.get_account_data(&self.position)?)?;
        let config =
            parse_vault_config(&self.rpc.get_account_data(&vault_config_pda(pos.vault_id))?)?;
        let oracle = parse_oracle(&self.rpc.get_account_data(&config.oracle)?)?;
        Ok(std::iter::once(self.position).chain(oracle.sources).map(|k| k.to_bytes()).collect())
    }
}

impl JupiterVenue {
    pub(crate) fn read(&self) -> Result<Reading> {
        let data = self.rpc.get_account_data(&self.position).with_context(|| {
            format!("fetch position {} (closed, or not a mainnet address?)", self.position)
        })?;
        let pos = parse_position(&data)?;
        // A position without debt sits below every tick, as if at the bottom
        // one with no liquidation history.
        let tick_key = (!pos.supply_only).then(|| tick_pda(pos.vault_id, pos.tick));
        let keys: Vec<Pubkey> =
            std::iter::once(vault_config_pda(pos.vault_id)).chain(tick_key).collect();
        let mut accounts = self.rpc.get_multiple_accounts(&keys)?.into_iter();
        let mut next = |what: &str| {
            accounts.next().flatten().map(|a| a.data).with_context(|| format!("{what} not found"))
        };
        let config = parse_vault_config(&next("vault config")?)?;
        let tick = match tick_key {
            None => Tick { tick: MIN_TICK, is_liquidated: false, total_ids: 0 },
            Some(_) => {
                let tick = parse_tick(&next("position tick")?)?;
                // The vault program rebuilds a liquidated position from branch
                // history; we don't, so refuse rather than show stale numbers.
                ensure!(
                    !tick.is_liquidated && tick.total_ids <= pos.tick_id,
                    "position's tick was liquidated since it was opened; its debt can't be read from the position alone"
                );
                tick
            }
        };
        let oracle = parse_oracle(&self.rpc.get_account_data(&config.oracle)?)?;
        let payer = match self.payer.get() {
            Some(payer) => *payer,
            None => {
                let holder = self.nft_holder(&pos.position_mint)?;
                *self.payer.get_or_init(|| holder)
            }
        };

        let (prices, _) = self.view(
            Instruction {
                program_id: VAULTS_PROGRAM_ID,
                accounts: [
                    vault_state_pda(pos.vault_id),
                    keys[0],
                    token_reserve_pda(&config.supply_token),
                    token_reserve_pda(&config.borrow_token),
                ]
                .into_iter()
                .map(|k| AccountMeta::new_readonly(k, false))
                .collect(),
                data: discriminator("global", "get_exchange_prices").to_vec(),
            },
            &payer,
        )?;
        ensure!(prices.len() == 64, "get_exchange_prices returned {} bytes", prices.len());
        let price =
            |i: usize| u128::from_le_bytes(prices[i * 16..(i + 1) * 16].try_into().unwrap());
        // (liquidity supply, liquidity borrow, vault supply, vault borrow)
        let (supply_ex, borrow_ex) = (price(2), price(3));

        let (rate, slot) = self.view(
            Instruction {
                program_id: ORACLE_PROGRAM_ID,
                accounts: std::iter::once(config.oracle)
                    .chain(oracle.sources.iter().copied())
                    .map(|k| AccountMeta::new_readonly(k, false))
                    .collect(),
                data: [
                    &discriminator("global", "get_exchange_rate_liquidate")[..],
                    &oracle.nonce.to_le_bytes(),
                ]
                .concat(),
            },
            &payer,
        )?;
        let rate = u128::from_le_bytes(rate.try_into().ok().context("oracle rate is not a u128")?);
        Ok(Reading { pos, config, tick, oracle, supply_ex, borrow_ex, rate, slot })
    }

    /// The wallet holding the position's NFT, which controls the position.
    pub fn owner(&self) -> Result<Pubkey> {
        let pos = parse_position(&self.rpc.get_account_data(&self.position)?)?;
        self.nft_holder(&pos.position_mint)
    }

    /// Wallet holding the position NFT (the position's owner).
    fn nft_holder(&self, mint: &Pubkey) -> Result<Pubkey> {
        let holders = self.rpc.get_token_largest_accounts(mint).context(
            "look up the position's owner (public RPCs throttle this); pass --payer <your wallet>",
        )?;
        let holder = holders
            .iter()
            .find(|h| h.amount.amount == "1")
            .with_context(|| format!("nobody holds position NFT {mint}; pass --payer"))?;
        token_account_owner(&self.rpc.get_account_data(&holder.address.parse()?)?)
    }
}

/// Build a snapshot from a position, its vault config, the vault exchange
/// prices (1e12) and the oracle's debt-per-collateral rate (1e15).
pub fn snapshot_from(
    pos: &Position,
    config: &VaultConfig,
    supply_ex_price: u128,
    borrow_ex_price: u128,
    rate: u128,
    slot: u64,
) -> Result<Snapshot> {
    ensure!(!pos.supply_only, "position has no debt (supply only), nothing to guard");
    let net_debt_raw = tick::debt_raw(pos.supply_raw, pos.tick)?
        .checked_sub(pos.dust_debt_raw as u128)
        .context("dust debt exceeds position debt")?;
    ensure!(net_debt_raw > 0 && rate > 0, "position has no debt or the oracle returned 0");
    let to_tokens = |raw: u128, ex_price: u128| {
        raw as f64 * ex_price as f64 / EXCHANGE_PRICES_PRECISION / AMOUNT_SCALE
    };
    Ok(Snapshot {
        collateral_symbol: symbol(&config.supply_token),
        debt_symbol: symbol(&config.borrow_token),
        collateral: to_tokens(pos.supply_raw as u128, supply_ex_price),
        debt: to_tokens(net_debt_raw, borrow_ex_price),
        // Jupiter prices collateral in debt tokens, not USD.
        collateral_price: rate as f64 / RATE_SCALE,
        debt_price: 1.0,
        liquidation_ltv: config.liquidation_threshold as f64 / 1_000.0,
        max_ltv: config.collateral_factor as f64 / 1_000.0,
        // The oracle view fails outright on stale sources.
        prices_ok: true,
        slot,
    })
}

/// Display symbol for common mints; otherwise the mint's first characters.
fn symbol(mint: &Pubkey) -> String {
    borrowguard_core::token::symbol(&mint.to_string())
}

/// A position found by [`find_positions`], with its vault's pair.
pub struct FoundPosition {
    pub key: Pubkey,
    pub position: Position,
    /// e.g. "SOL/USDC".
    pub pair: String,
    pub liquidation_ltv: f64,
}

/// Jupiter Lend positions whose NFT `owner` holds.
pub fn find_positions(rpc_url: &str, owner: &Pubkey) -> Result<Vec<FoundPosition>> {
    let rpc = RpcClient::new_with_commitment(rpc_url.to_string(), CommitmentConfig::confirmed());
    let mut mints = Vec::new();
    for program in TOKEN_PROGRAMS {
        for account in
            rpc.get_token_accounts_by_owner(owner, TokenAccountsFilter::ProgramId(program))?
        {
            // jsonParsed: info.tokenAmount.{amount, decimals}, info.mint
            if let UiAccountData::Json(parsed) = account.account.data {
                let info = &parsed.parsed["info"];
                let amount = &info["tokenAmount"];
                if amount["amount"] == "1" && amount["decimals"] == 0 {
                    if let Some(mint) = info["mint"].as_str() {
                        mints.push(mint.parse::<Pubkey>()?);
                    }
                }
            }
        }
    }
    let mut positions = Vec::new();
    // Each candidate NFT costs one filtered gPA; position NFTs are a handful.
    for mint in mints.iter().take(50) {
        let found = rpc.get_program_accounts_with_config(
            &VAULTS_PROGRAM_ID,
            RpcProgramAccountsConfig {
                filters: Some(vec![
                    RpcFilterType::DataSize(POSITION_ACCOUNT_LEN as u64),
                    RpcFilterType::Memcmp(Memcmp::new_base58_encoded(
                        POSITION_MINT_OFFSET,
                        mint.as_ref(),
                    )),
                ]),
                account_config: RpcAccountInfoConfig {
                    encoding: Some(UiAccountEncoding::Base64),
                    ..Default::default()
                },
                ..Default::default()
            },
        )?;
        for (key, account) in found {
            positions.push((key, parse_position(&account.data)?));
        }
    }
    let configs = rpc.get_multiple_accounts(
        &positions.iter().map(|(_, p)| vault_config_pda(p.vault_id)).collect::<Vec<_>>(),
    )?;
    positions
        .into_iter()
        .zip(configs)
        .map(|((key, position), config)| {
            let config = parse_vault_config(&config.context("vault config not found")?.data)?;
            Ok(FoundPosition {
                key,
                position,
                pair: format!("{}/{}", symbol(&config.supply_token), symbol(&config.borrow_token)),
                liquidation_ltv: config.liquidation_threshold as f64 / 1_000.0,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::tests::position_bytes;

    fn config() -> VaultConfig {
        VaultConfig {
            vault_id: 1,
            collateral_factor: 800,
            liquidation_threshold: 850,
            borrow_fee: 0,
            oracle: Pubkey::new_unique(),
            oracle_program: Pubkey::new_unique(),
            supply_token: "So11111111111111111111111111111111111111112".parse().unwrap(),
            borrow_token: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".parse().unwrap(),
        }
    }

    #[test]
    fn snapshot_converts_raw_units() {
        // Tick 0 is a raw debt/collateral ratio of 1.0: 10 SOL of raw collateral
        // carries 10 of raw debt. Exchange prices 1.0 (supply) and 200 (borrow)
        // make that 10 SOL against 2,000 USDC, with SOL at 300 USDC.
        let pos = parse_position(&position_bytes(1, 0, 10_000_000_000, 2)).unwrap();
        let snap = snapshot_from(
            &pos,
            &config(),
            1_000_000_000_000,
            200_000_000_000_000,
            300_000_000_000_000_000,
            9,
        )
        .unwrap();
        assert_eq!((snap.collateral_symbol.as_str(), snap.debt_symbol.as_str()), ("SOL", "USDC"));
        assert!((snap.collateral - 10.0).abs() < 1e-9);
        // debt_raw = (col + 1) + 1 - dust(2) = col
        assert!((snap.debt - 2_000.0).abs() < 1e-6);
        assert!((snap.price() - 300.0).abs() < 1e-9);
        let leverage = snap.position().leverage(snap.price()).unwrap();
        assert!((leverage - 3.0).abs() < 1e-6);
        assert!((snap.liquidation_ltv - 0.85).abs() < 1e-12);
        assert!((snap.max_ltv - 0.80).abs() < 1e-12);
    }

    #[test]
    fn unknown_mint_gets_short_symbol() {
        assert_eq!(symbol(&Pubkey::new_from_array([0; 32])), "1111…");
    }
}
