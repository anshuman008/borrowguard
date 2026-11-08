//! klend instruction builders. Account orders follow the klend IDL
//! (release/v1.18.0), as used by the solana-liquidator adapter.

use solana_program::{
    instruction::{AccountMeta, Instruction},
    pubkey,
    pubkey::Pubkey,
    system_program, sysvar,
};

pub use borrowguard_exec::token::{
    associated_token_address, create_ata_idempotent, ASSOCIATED_TOKEN_PROGRAM_ID,
};

use crate::layout::discriminator;
use crate::reserve::Reserve;

/// `refresh_reserve`: re-reads the reserve's oracles and accrues interest.
pub fn refresh_reserve(program_id: &Pubkey, reserve: &Pubkey, state: &Reserve) -> Instruction {
    // klend takes its own program id in place of an unused oracle.
    let oracle = |pk: Option<Pubkey>| AccountMeta::new_readonly(pk.unwrap_or(*program_id), false);
    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new(*reserve, false),
            AccountMeta::new_readonly(state.lending_market, false),
            oracle(state.pyth_price),
            oracle(state.switchboard_price),
            oracle(state.switchboard_twap),
            oracle(state.scope_prices),
        ],
        data: discriminator("global", "refresh_reserve").to_vec(),
    }
}

/// `refresh_obligation`: revalues deposits and borrows at refreshed reserve
/// prices. Reserves must be passed in the obligation's own order, and an
/// obligation with a referrer also needs one referrer state per borrow.
pub fn refresh_obligation(
    program_id: &Pubkey,
    lending_market: &Pubkey,
    obligation: &Pubkey,
    deposit_reserves: &[Pubkey],
    borrow_reserves: &[Pubkey],
    referrer: Option<&Pubkey>,
) -> Instruction {
    let mut accounts = vec![
        AccountMeta::new_readonly(*lending_market, false),
        AccountMeta::new(*obligation, false),
    ];
    accounts.extend(deposit_reserves.iter().map(|r| AccountMeta::new_readonly(*r, false)));
    accounts.extend(borrow_reserves.iter().map(|r| AccountMeta::new(*r, false)));
    if let Some(referrer) = referrer {
        accounts.extend(borrow_reserves.iter().map(|reserve| {
            AccountMeta::new(referrer_token_state(program_id, referrer, reserve), false)
        }));
    }
    Instruction {
        program_id: *program_id,
        accounts,
        data: discriminator("global", "refresh_obligation").to_vec(),
    }
}

fn referrer_token_state(program_id: &Pubkey, referrer: &Pubkey, reserve: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[b"referrer_acc", referrer.as_ref(), reserve.as_ref()],
        program_id,
    )
    .0
}

pub const FARMS_PROGRAM_ID: Pubkey = pubkey!("FarmsPZpWu9i7Kky8tPN37rs2TpmMrAZrC7S7vJa91Hr");

pub fn lending_market_authority(program_id: &Pubkey, lending_market: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"lma", lending_market.as_ref()], program_id).0
}

/// The obligation's state in a reserve's rewards farm.
pub fn obligation_farm_user_state(farm: &Pubkey, obligation: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"user", farm.as_ref(), obligation.as_ref()], &FARMS_PROGRAM_ID)
        .0
}

/// A wallet's klend user metadata, which every obligation of it points to.
pub fn user_metadata(program_id: &Pubkey, owner: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"user_meta", owner.as_ref()], program_id).0
}

/// A plain ("vanilla", tag 0) obligation: the kind the Kamino UI's borrow
/// tab opens. Its seeds are the default pubkey; `id` tells a wallet's apart.
pub fn vanilla_obligation(program_id: &Pubkey, owner: &Pubkey, market: &Pubkey, id: u8) -> Pubkey {
    let none = Pubkey::default();
    Pubkey::find_program_address(
        &[&[0], &[id], owner.as_ref(), market.as_ref(), none.as_ref(), none.as_ref()],
        program_id,
    )
    .0
}

pub fn init_user_metadata(program_id: &Pubkey, owner: &Pubkey) -> Instruction {
    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new_readonly(*owner, true),
            AccountMeta::new(*owner, true), // fee payer
            AccountMeta::new(user_metadata(program_id, owner), false),
            AccountMeta::new_readonly(*program_id, false), // no referrer
            AccountMeta::new_readonly(sysvar::rent::ID, false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        // No user lookup table.
        data: [&discriminator("global", "init_user_metadata")[..], &[0; 32]].concat(),
    }
}

pub fn init_vanilla_obligation(
    program_id: &Pubkey,
    owner: &Pubkey,
    market: &Pubkey,
    id: u8,
) -> Instruction {
    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new_readonly(*owner, true),
            AccountMeta::new(*owner, true), // fee payer
            AccountMeta::new(vanilla_obligation(program_id, owner, market, id), false),
            AccountMeta::new_readonly(*market, false),
            AccountMeta::new_readonly(Pubkey::default(), false),
            AccountMeta::new_readonly(Pubkey::default(), false),
            AccountMeta::new_readonly(user_metadata(program_id, owner), false),
            AccountMeta::new_readonly(sysvar::rent::ID, false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data: [&discriminator("global", "init_obligation")[..], &[0, id]].concat(),
    }
}

/// Which of a reserve's farms an obligation joins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FarmKind {
    Collateral = 0,
    Debt = 1,
}

/// Create the obligation's user state in `farm`, which v2 deposits / borrows
/// on that reserve then keep up to date.
pub fn init_obligation_farm(
    program_id: &Pubkey,
    owner: &Pubkey,
    obligation: &Pubkey,
    reserve_key: &Pubkey,
    reserve: &Reserve,
    farm: &Pubkey,
    kind: FarmKind,
) -> Instruction {
    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new(*owner, true), // payer
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new(*obligation, false),
            AccountMeta::new_readonly(
                lending_market_authority(program_id, &reserve.lending_market),
                false,
            ),
            AccountMeta::new(*reserve_key, false),
            AccountMeta::new(*farm, false),
            AccountMeta::new(obligation_farm_user_state(farm, obligation), false),
            AccountMeta::new_readonly(reserve.lending_market, false),
            AccountMeta::new_readonly(FARMS_PROGRAM_ID, false),
            AccountMeta::new_readonly(sysvar::rent::ID, false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data: [&discriminator("global", "init_obligation_farms_for_reserve")[..], &[kind as u8]]
            .concat(),
    }
}

fn with_amount(name: &str, amount: u64) -> Vec<u8> {
    [&discriminator("global", name)[..], &amount.to_le_bytes()].concat()
}

/// Flash loan from `reserve` into `user_destination` (`user` signs). Must be
/// paired with [`flash_repay`] later in the same transaction.
pub fn flash_borrow(
    program_id: &Pubkey,
    user: &Pubkey,
    reserve_key: &Pubkey,
    reserve: &Reserve,
    user_destination: &Pubkey,
    amount: u64,
) -> Instruction {
    Instruction {
        program_id: *program_id,
        accounts: flash_accounts(program_id, user, reserve_key, reserve, user_destination),
        data: with_amount("flash_borrow_reserve_liquidity", amount),
    }
}

/// Repays the flash loan taken by the instruction at `borrow_ix_index`.
pub fn flash_repay(
    program_id: &Pubkey,
    user: &Pubkey,
    reserve_key: &Pubkey,
    reserve: &Reserve,
    user_source: &Pubkey,
    amount: u64,
    borrow_ix_index: u8,
) -> Instruction {
    let mut data = with_amount("flash_repay_reserve_liquidity", amount);
    data.push(borrow_ix_index);
    Instruction {
        program_id: *program_id,
        accounts: flash_accounts(program_id, user, reserve_key, reserve, user_source),
        data,
    }
}

fn flash_accounts(
    program_id: &Pubkey,
    user: &Pubkey,
    reserve_key: &Pubkey,
    reserve: &Reserve,
    user_token: &Pubkey,
) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new_readonly(*user, true),
        AccountMeta::new_readonly(
            lending_market_authority(program_id, &reserve.lending_market),
            false,
        ),
        AccountMeta::new_readonly(reserve.lending_market, false),
        AccountMeta::new(*reserve_key, false),
        AccountMeta::new_readonly(reserve.mint, false),
        AccountMeta::new(reserve.supply_vault, false),
        AccountMeta::new(*user_token, false),
        AccountMeta::new(reserve.fee_vault, false),
        AccountMeta::new(*program_id, false), // referrer token state (none)
        AccountMeta::new(*program_id, false), // referrer account (none)
        AccountMeta::new_readonly(sysvar::instructions::ID, false),
        AccountMeta::new_readonly(reserve.token_program, false),
    ]
}

/// Farm accounts for a v2 instruction: the obligation's user state and the
/// reserve's farm, or klend's "none" placeholder when the reserve has no farm.
fn farm_accounts(
    program_id: &Pubkey,
    farm: Option<Pubkey>,
    obligation: &Pubkey,
) -> [AccountMeta; 2] {
    match farm {
        Some(farm) => [
            AccountMeta::new(obligation_farm_user_state(&farm, obligation), false),
            AccountMeta::new(farm, false),
        ],
        None => [AccountMeta::new(*program_id, false), AccountMeta::new(*program_id, false)],
    }
}

/// `repay_obligation_liquidity_v2`: owner repays `amount` of debt from `user_source`.
pub fn repay_v2(
    program_id: &Pubkey,
    owner: &Pubkey,
    obligation: &Pubkey,
    reserve_key: &Pubkey,
    reserve: &Reserve,
    user_source: &Pubkey,
    amount: u64,
) -> Instruction {
    let [farm_user, farm_state] = farm_accounts(program_id, reserve.farm_debt, obligation);
    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new_readonly(*owner, true),
            AccountMeta::new(*obligation, false),
            AccountMeta::new_readonly(reserve.lending_market, false),
            AccountMeta::new(*reserve_key, false),
            AccountMeta::new_readonly(reserve.mint, false),
            AccountMeta::new(reserve.supply_vault, false),
            AccountMeta::new(*user_source, false),
            AccountMeta::new_readonly(reserve.token_program, false),
            AccountMeta::new_readonly(sysvar::instructions::ID, false),
            farm_user,
            farm_state,
            AccountMeta::new_readonly(
                lending_market_authority(program_id, &reserve.lending_market),
                false,
            ),
            AccountMeta::new_readonly(FARMS_PROGRAM_ID, false),
        ],
        data: with_amount("repay_obligation_liquidity_v2", amount),
    }
}

/// `withdraw_obligation_collateral_and_redeem_reserve_collateral_v2`: owner
/// withdraws `ctokens` of collateral and receives the underlying liquidity
/// in `user_destination`.
pub fn withdraw_v2(
    program_id: &Pubkey,
    owner: &Pubkey,
    obligation: &Pubkey,
    reserve_key: &Pubkey,
    reserve: &Reserve,
    user_destination: &Pubkey,
    ctokens: u64,
) -> Instruction {
    let [farm_user, farm_state] = farm_accounts(program_id, reserve.farm_collateral, obligation);
    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new(*owner, true),
            AccountMeta::new(*obligation, false),
            AccountMeta::new_readonly(reserve.lending_market, false),
            AccountMeta::new_readonly(
                lending_market_authority(program_id, &reserve.lending_market),
                false,
            ),
            AccountMeta::new(*reserve_key, false),
            AccountMeta::new_readonly(reserve.mint, false),
            AccountMeta::new(reserve.collateral_supply_vault, false),
            AccountMeta::new(reserve.collateral_mint, false),
            AccountMeta::new(reserve.supply_vault, false),
            AccountMeta::new(*user_destination, false),
            AccountMeta::new_readonly(*program_id, false), // placeholder user destination collateral
            AccountMeta::new_readonly(spl_token_program(), false), // collateral (cToken) program
            AccountMeta::new_readonly(reserve.token_program, false),
            AccountMeta::new_readonly(sysvar::instructions::ID, false),
            farm_user,
            farm_state,
            AccountMeta::new_readonly(FARMS_PROGRAM_ID, false),
        ],
        data: with_amount(
            "withdraw_obligation_collateral_and_redeem_reserve_collateral_v2",
            ctokens,
        ),
    }
}

/// `deposit_reserve_liquidity_and_obligation_collateral_v2`: owner deposits
/// `amount` of liquidity from `user_source` as collateral.
pub fn deposit_v2(
    program_id: &Pubkey,
    owner: &Pubkey,
    obligation: &Pubkey,
    reserve_key: &Pubkey,
    reserve: &Reserve,
    user_source: &Pubkey,
    amount: u64,
) -> Instruction {
    let [farm_user, farm_state] = farm_accounts(program_id, reserve.farm_collateral, obligation);
    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new(*owner, true),
            AccountMeta::new(*obligation, false),
            AccountMeta::new_readonly(reserve.lending_market, false),
            AccountMeta::new_readonly(
                lending_market_authority(program_id, &reserve.lending_market),
                false,
            ),
            AccountMeta::new(*reserve_key, false),
            AccountMeta::new_readonly(reserve.mint, false),
            AccountMeta::new(reserve.supply_vault, false),
            AccountMeta::new(reserve.collateral_mint, false),
            AccountMeta::new(reserve.collateral_supply_vault, false),
            AccountMeta::new(*user_source, false),
            AccountMeta::new_readonly(*program_id, false), // placeholder user destination collateral
            AccountMeta::new_readonly(spl_token_program(), false), // collateral (cToken) program
            AccountMeta::new_readonly(reserve.token_program, false),
            AccountMeta::new_readonly(sysvar::instructions::ID, false),
            farm_user,
            farm_state,
            AccountMeta::new_readonly(FARMS_PROGRAM_ID, false),
        ],
        data: with_amount("deposit_reserve_liquidity_and_obligation_collateral_v2", amount),
    }
}

/// `borrow_obligation_liquidity_v2`: owner borrows exactly `amount` into
/// `user_destination` (the borrow fee is added to the debt, not deducted).
/// Obligations in an elevation group must also pass their deposit reserves.
#[allow(clippy::too_many_arguments)]
pub fn borrow_v2(
    program_id: &Pubkey,
    owner: &Pubkey,
    obligation: &Pubkey,
    reserve_key: &Pubkey,
    reserve: &Reserve,
    user_destination: &Pubkey,
    amount: u64,
    elevation_group_deposits: &[Pubkey],
) -> Instruction {
    let [farm_user, farm_state] = farm_accounts(program_id, reserve.farm_debt, obligation);
    let mut accounts = vec![
        AccountMeta::new_readonly(*owner, true),
        AccountMeta::new(*obligation, false),
        AccountMeta::new_readonly(reserve.lending_market, false),
        AccountMeta::new_readonly(
            lending_market_authority(program_id, &reserve.lending_market),
            false,
        ),
        AccountMeta::new(*reserve_key, false),
        AccountMeta::new_readonly(reserve.mint, false),
        AccountMeta::new(reserve.supply_vault, false),
        AccountMeta::new(reserve.fee_vault, false),
        AccountMeta::new(*user_destination, false),
        AccountMeta::new_readonly(*program_id, false), // referrer token state (none)
        AccountMeta::new_readonly(reserve.token_program, false),
        AccountMeta::new_readonly(sysvar::instructions::ID, false),
        farm_user,
        farm_state,
        AccountMeta::new_readonly(FARMS_PROGRAM_ID, false),
    ];
    accounts.extend(elevation_group_deposits.iter().map(|r| AccountMeta::new(*r, false)));
    Instruction {
        program_id: *program_id,
        accounts,
        data: with_amount("borrow_obligation_liquidity_v2", amount),
    }
}

/// Reserve collateral mints are always classic SPL Token.
fn spl_token_program() -> Pubkey {
    pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_obligation_orders_accounts() {
        let [program, market, obligation, dep, bor, referrer] =
            std::array::from_fn(|_| Pubkey::new_unique());
        let ix = refresh_obligation(&program, &market, &obligation, &[dep], &[bor], None);
        let keys: Vec<_> = ix.accounts.iter().map(|a| a.pubkey).collect();
        assert_eq!(keys, vec![market, obligation, dep, bor]);
        assert!(ix.accounts[3].is_writable && !ix.accounts[2].is_writable);

        let ix =
            refresh_obligation(&program, &market, &obligation, &[dep], &[bor], Some(&referrer));
        assert_eq!(ix.accounts.len(), 5);
    }

    #[test]
    fn farm_accounts_use_placeholders_without_a_farm() {
        let program = Pubkey::new_unique();
        let obligation = Pubkey::new_unique();
        let [a, b] = farm_accounts(&program, None, &obligation);
        assert_eq!((a.pubkey, b.pubkey), (program, program));
        let farm = Pubkey::new_unique();
        let [a, b] = farm_accounts(&program, Some(farm), &obligation);
        assert_eq!(a.pubkey, obligation_farm_user_state(&farm, &obligation));
        assert_eq!(b.pubkey, farm);
    }

    #[test]
    fn ata_matches_mainnet() {
        // A mainnet wallet holding USDC in two token accounts; one is its ATA.
        let owner: Pubkey = "MriyaNN8TMp6qRWjfr723PK7xgQK7yCt7Kg2v2PQu7X".parse().unwrap();
        let usdc: Pubkey = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".parse().unwrap();
        let ata = associated_token_address(&owner, &usdc, &spl_token_program()).to_string();
        assert!(
            [
                "DAzCHBKWy1j9vktC7AtAAPN4wvQB1sFkpyyfZAP9BruB",
                "Gsc3UenVAAnwpFxBBrTRKXpyaZ5DYRxAGTSDuW58sNSD"
            ]
            .contains(&ata.as_str()),
            "{ata}"
        );
    }
}
