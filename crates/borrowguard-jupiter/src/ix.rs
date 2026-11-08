//! Jupiter Lend PDAs and instruction builders. Account orders follow the
//! Anchor contexts in fluid-solana-programs (`programs/vaults/src/state/context.rs`,
//! `programs/flashloan/src/state/context.rs`).

use solana_program::{
    instruction::{AccountMeta, Instruction},
    pubkey,
    pubkey::Pubkey,
    system_program, sysvar,
};

use borrowguard_exec::token::{
    associated_token_address, ASSOCIATED_TOKEN_PROGRAM_ID, TOKEN_PROGRAM_ID,
};

use crate::layout::discriminator;
use crate::tick::MIN_TICK;

pub const VAULTS_PROGRAM_ID: Pubkey = pubkey!("jupr81YtYssSyPt8jbnGuiWon5f6x9TcDEFxYe3Bdzi");
pub const LIQUIDITY_PROGRAM_ID: Pubkey = pubkey!("jupeiUmn818Jg1ekPURTpr4mFo29p46vygyykFJ3wZC");
pub const ORACLE_PROGRAM_ID: Pubkey = pubkey!("jupnw4B6Eqs7ft6rxpzYLJZYSnrpRgPcr589n5Kv4oc");
pub const FLASHLOAN_PROGRAM_ID: Pubkey = pubkey!("jupgfSgfuAXv4B6R2Uxu85Z1qdzgju79s6MfZekN6XS");
/// Metaplex token metadata, which describes each position NFT.
pub const METADATA_PROGRAM_ID: Pubkey = pubkey!("metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s");

fn vault_pda(seeds: &[&[u8]]) -> Pubkey {
    Pubkey::find_program_address(seeds, &VAULTS_PROGRAM_ID).0
}

fn liquidity_pda(seeds: &[&[u8]]) -> Pubkey {
    Pubkey::find_program_address(seeds, &LIQUIDITY_PROGRAM_ID).0
}

/// Ticks are seeded as `tick - MIN_TICK`, so the seed is never negative.
fn tick_seed(tick: i32) -> [u8; 4] {
    (tick - MIN_TICK).to_le_bytes()
}

pub fn position_pda(vault_id: u16, nft_id: u32) -> Pubkey {
    vault_pda(&[b"position", &vault_id.to_le_bytes(), &nft_id.to_le_bytes()])
}

/// The NFT whose holder owns a position.
pub fn position_mint_pda(vault_id: u16, nft_id: u32) -> Pubkey {
    vault_pda(&[b"position_mint", &vault_id.to_le_bytes(), &nft_id.to_le_bytes()])
}

pub fn vault_config_pda(vault_id: u16) -> Pubkey {
    vault_pda(&[b"vault_config", &vault_id.to_le_bytes()])
}

pub fn vault_state_pda(vault_id: u16) -> Pubkey {
    vault_pda(&[b"vault_state", &vault_id.to_le_bytes()])
}

pub fn vault_metadata_pda(vault_id: u16) -> Pubkey {
    vault_pda(&[b"vault_metadata", &vault_id.to_le_bytes()])
}

pub fn tick_pda(vault_id: u16, tick: i32) -> Pubkey {
    vault_pda(&[b"tick", &vault_id.to_le_bytes(), &tick_seed(tick)])
}

/// A tick's liquidation history; each account covers three tick IDs.
pub fn tick_id_liquidation_pda(vault_id: u16, tick: i32, total_ids: u32) -> Pubkey {
    let group = total_ids.div_ceil(3); // the program writes (total_ids + 2) / 3
    vault_pda(&[
        b"tick_id_liquidation",
        &vault_id.to_le_bytes(),
        &tick_seed(tick),
        &group.to_le_bytes(),
    ])
}

pub fn tick_has_debt_pda(vault_id: u16, index: u8) -> Pubkey {
    vault_pda(&[b"tick_has_debt", &vault_id.to_le_bytes(), &[index]])
}

pub fn branch_pda(vault_id: u16, branch_id: u32) -> Pubkey {
    vault_pda(&[b"branch", &vault_id.to_le_bytes(), &branch_id.to_le_bytes()])
}

pub fn liquidity() -> Pubkey {
    liquidity_pda(&[b"liquidity"])
}

pub fn token_reserve_pda(mint: &Pubkey) -> Pubkey {
    liquidity_pda(&[b"reserve", mint.as_ref()])
}

pub fn rate_model_pda(mint: &Pubkey) -> Pubkey {
    liquidity_pda(&[b"rate_model", mint.as_ref()])
}

/// A protocol's (vault's, or the flashloan program's) supply position at the liquidity layer.
pub fn user_supply_position_pda(mint: &Pubkey, protocol: &Pubkey) -> Pubkey {
    liquidity_pda(&[b"user_supply_position", mint.as_ref(), protocol.as_ref()])
}

/// A protocol's borrow position at the liquidity layer.
pub fn user_borrow_position_pda(mint: &Pubkey, protocol: &Pubkey) -> Pubkey {
    liquidity_pda(&[b"user_borrow_position", mint.as_ref(), protocol.as_ref()])
}

pub fn flashloan_admin() -> Pubkey {
    Pubkey::find_program_address(&[b"flashloan_admin"], &FLASHLOAN_PROGRAM_ID).0
}

/// A mint as the vault uses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub mint: Pubkey,
    pub program: Pubkey,
    pub decimals: u8,
}

impl Token {
    pub fn ata(&self, owner: &Pubkey) -> Pubkey {
        associated_token_address(owner, &self.mint, &self.program)
    }

    /// Where the liquidity layer keeps this token.
    pub fn liquidity_vault(&self) -> Pubkey {
        self.ata(&liquidity())
    }
}

/// Everything `operate` needs besides the amounts.
pub struct OperateAccounts {
    pub signer: Pubkey,
    pub vault_id: u16,
    pub oracle: Pubkey,
    pub oracle_program: Pubkey,
    pub supply: Token,
    pub borrow: Token,
    pub position: Pubkey,
    pub position_token_account: Pubkey,
    pub current_tick: i32,
    pub current_tick_id: Pubkey,
    pub final_tick: i32,
    pub final_tick_id: Pubkey,
    pub new_branch: Pubkey,
    pub oracle_sources: Vec<Pubkey>,
    pub branches: Vec<Pubkey>,
    pub tick_has_debt: Vec<Pubkey>,
}

/// `operate(new_col, new_debt)`: amounts in token base units, positive to
/// deposit / borrow, negative to withdraw / pay back. Funds move to and from
/// the signer's associated token accounts.
pub fn operate(a: &OperateAccounts, new_col: i128, new_debt: i128) -> Instruction {
    let config = vault_config_pda(a.vault_id);
    let (supply_ata, borrow_ata) = (a.supply.ata(&a.signer), a.borrow.ata(&a.signer));
    let mut accounts = vec![
        AccountMeta::new(a.signer, true),
        AccountMeta::new(supply_ata, false),
        AccountMeta::new(borrow_ata, false),
        AccountMeta::new_readonly(a.signer, false), // recipient
        AccountMeta::new(borrow_ata, false),        // recipient_borrow_token_account
        AccountMeta::new(supply_ata, false),        // recipient_supply_token_account
        AccountMeta::new_readonly(config, false),
        AccountMeta::new(vault_state_pda(a.vault_id), false),
        AccountMeta::new_readonly(a.supply.mint, false),
        AccountMeta::new_readonly(a.borrow.mint, false),
        AccountMeta::new_readonly(a.oracle, false),
        AccountMeta::new(a.position, false),
        AccountMeta::new_readonly(a.position_token_account, false),
        AccountMeta::new(tick_pda(a.vault_id, a.current_tick), false),
        AccountMeta::new(tick_pda(a.vault_id, a.final_tick), false),
        AccountMeta::new_readonly(a.current_tick_id, false),
        AccountMeta::new(a.final_tick_id, false),
        AccountMeta::new(a.new_branch, false),
        AccountMeta::new(token_reserve_pda(&a.supply.mint), false),
        AccountMeta::new(token_reserve_pda(&a.borrow.mint), false),
        AccountMeta::new(user_supply_position_pda(&a.supply.mint, &config), false),
        AccountMeta::new(user_borrow_position_pda(&a.borrow.mint, &config), false),
        AccountMeta::new_readonly(rate_model_pda(&a.supply.mint), false),
        AccountMeta::new_readonly(rate_model_pda(&a.borrow.mint), false),
        AccountMeta::new(a.supply.liquidity_vault(), false),
        AccountMeta::new(a.borrow.liquidity_vault(), false),
        // Optional claim accounts, unused: Anchor reads the program id as None.
        AccountMeta::new_readonly(VAULTS_PROGRAM_ID, false),
        AccountMeta::new_readonly(VAULTS_PROGRAM_ID, false),
        AccountMeta::new_readonly(liquidity(), false),
        AccountMeta::new_readonly(LIQUIDITY_PROGRAM_ID, false),
        AccountMeta::new_readonly(a.oracle_program, false),
        AccountMeta::new_readonly(a.supply.program, false),
        AccountMeta::new_readonly(a.borrow.program, false),
        AccountMeta::new_readonly(ASSOCIATED_TOKEN_PROGRAM_ID, false),
        AccountMeta::new_readonly(system_program::ID, false),
    ];
    accounts.extend(a.oracle_sources.iter().map(|k| AccountMeta::new_readonly(*k, false)));
    accounts.extend(a.branches.iter().map(|k| AccountMeta::new(*k, false)));
    accounts.extend(a.tick_has_debt.iter().map(|k| AccountMeta::new(*k, false)));

    let mut data = discriminator("global", "operate").to_vec();
    data.extend_from_slice(&new_col.to_le_bytes());
    data.extend_from_slice(&new_debt.to_le_bytes());
    data.push(0); // transfer_type: None (direct)
    data.extend_from_slice(&3u32.to_le_bytes());
    data.extend_from_slice(&[
        a.oracle_sources.len() as u8,
        a.branches.len() as u8,
        a.tick_has_debt.len() as u8,
    ]);
    Instruction { program_id: VAULTS_PROGRAM_ID, accounts, data }
}

/// Create position `nft_id` (the vault's `next_position_id`) in `vault_id`
/// and mint its NFT to the signer.
pub fn init_position(signer: &Pubkey, vault_id: u16, nft_id: u32) -> Instruction {
    let mint = position_mint_pda(vault_id, nft_id);
    let metadata = Pubkey::find_program_address(
        &[b"metadata", METADATA_PROGRAM_ID.as_ref(), mint.as_ref()],
        &METADATA_PROGRAM_ID,
    )
    .0;
    let mut data = discriminator("global", "init_position").to_vec();
    data.extend_from_slice(&vault_id.to_le_bytes());
    data.extend_from_slice(&nft_id.to_le_bytes());
    Instruction {
        program_id: VAULTS_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*signer, true),
            AccountMeta::new_readonly(vault_pda(&[b"vault_admin"]), false),
            AccountMeta::new(vault_state_pda(vault_id), false),
            AccountMeta::new(position_pda(vault_id, nft_id), false),
            AccountMeta::new(mint, false),
            AccountMeta::new(associated_token_address(signer, &mint, &TOKEN_PROGRAM_ID), false),
            AccountMeta::new(metadata, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
            AccountMeta::new_readonly(ASSOCIATED_TOKEN_PROGRAM_ID, false),
            AccountMeta::new_readonly(system_program::ID, false),
            AccountMeta::new_readonly(sysvar::instructions::ID, false),
            AccountMeta::new_readonly(METADATA_PROGRAM_ID, false),
            AccountMeta::new_readonly(sysvar::rent::ID, false),
        ],
        data,
    }
}

/// Create a tick account (permissionless; the signer pays rent).
pub fn init_tick(signer: &Pubkey, vault_id: u16, tick: i32) -> Instruction {
    let mut data = discriminator("global", "init_tick").to_vec();
    data.extend_from_slice(&vault_id.to_le_bytes());
    data.extend_from_slice(&tick.to_le_bytes());
    Instruction {
        program_id: VAULTS_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*signer, true),
            AccountMeta::new_readonly(vault_config_pda(vault_id), false),
            AccountMeta::new(tick_pda(vault_id, tick), false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data,
    }
}

/// Create the liquidation-history account a tick's `total_ids` points at.
pub fn init_tick_id_liquidation(
    signer: &Pubkey,
    vault_id: u16,
    tick: i32,
    total_ids: u32,
) -> Instruction {
    let mut data = discriminator("global", "init_tick_id_liquidation").to_vec();
    data.extend_from_slice(&vault_id.to_le_bytes());
    data.extend_from_slice(&tick.to_le_bytes());
    data.extend_from_slice(&total_ids.to_le_bytes());
    Instruction {
        program_id: VAULTS_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*signer, true),
            AccountMeta::new_readonly(tick_pda(vault_id, tick), false),
            AccountMeta::new(tick_id_liquidation_pda(vault_id, tick, total_ids), false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data,
    }
}

/// Create a branch account (needed when the current branch was liquidated).
pub fn init_branch(signer: &Pubkey, vault_id: u16, branch_id: u32) -> Instruction {
    let mut data = discriminator("global", "init_branch").to_vec();
    data.extend_from_slice(&vault_id.to_le_bytes());
    data.extend_from_slice(&branch_id.to_le_bytes());
    Instruction {
        program_id: VAULTS_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*signer, true),
            AccountMeta::new_readonly(vault_config_pda(vault_id), false),
            AccountMeta::new(branch_pda(vault_id, branch_id), false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data,
    }
}

fn flashloan(name: &str, signer: &Pubkey, token: &Token, amount: u64) -> Instruction {
    let admin = flashloan_admin();
    let mut data = discriminator("global", name).to_vec();
    data.extend_from_slice(&amount.to_le_bytes());
    Instruction {
        program_id: FLASHLOAN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*signer, true),
            AccountMeta::new(admin, false),
            AccountMeta::new(token.ata(signer), false),
            AccountMeta::new_readonly(token.mint, false),
            AccountMeta::new(token_reserve_pda(&token.mint), false),
            AccountMeta::new(user_borrow_position_pda(&token.mint, &admin), false),
            AccountMeta::new_readonly(rate_model_pda(&token.mint), false),
            AccountMeta::new(token.liquidity_vault(), false),
            AccountMeta::new_readonly(liquidity(), false),
            AccountMeta::new_readonly(LIQUIDITY_PROGRAM_ID, false),
            AccountMeta::new_readonly(token.program, false),
            AccountMeta::new_readonly(ASSOCIATED_TOKEN_PROGRAM_ID, false),
            AccountMeta::new_readonly(system_program::ID, false),
            AccountMeta::new_readonly(sysvar::instructions::ID, false),
        ],
        data,
    }
}

/// Borrow `amount` into the signer's account; a payback for the same amount
/// must follow, as a top-level instruction, in the same transaction.
pub fn flashloan_borrow(signer: &Pubkey, token: &Token, amount: u64) -> Instruction {
    flashloan("flashloan_borrow", signer, token, amount)
}

/// Return `amount` plus the flashloan fee.
pub fn flashloan_payback(signer: &Pubkey, token: &Token, amount: u64) -> Instruction {
    flashloan("flashloan_payback", signer, token, amount)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_mainnet_addresses() {
        // Liquidity PDA and the USDC reserve, as seen in Jupiter Lend transactions.
        assert_eq!(liquidity().to_string(), "7s1da8DduuBFqGra5bJBjpnvL5E9mGzCuMk1Qkh4or2Z");
        let usdc: Pubkey = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".parse().unwrap();
        let token = Token {
            mint: usdc,
            program: pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"),
            decimals: 6,
        };
        assert_eq!(
            token.liquidity_vault().to_string(),
            "BmkUoKMFYBxNSzWXyUjyMJjMAaVz4d8ZnxwwmhDCUXFB"
        );
    }

    #[test]
    fn init_position_matches_the_idl() {
        let ix = init_position(&Pubkey::new_unique(), 1, 7);
        assert_eq!(ix.data[..8], [197, 20, 10, 1, 97, 160, 177, 91]);
        assert_eq!(ix.data[8..], [1, 0, 7, 0, 0, 0]);
        assert_eq!(ix.accounts.len(), 13);
        assert_eq!(ix.accounts[3].pubkey, position_pda(1, 7));
    }

    #[test]
    fn operate_data_layout() {
        let t = Token { mint: Pubkey::new_unique(), program: Pubkey::new_unique(), decimals: 9 };
        let a = OperateAccounts {
            signer: Pubkey::new_unique(),
            vault_id: 1,
            oracle: Pubkey::new_unique(),
            oracle_program: ORACLE_PROGRAM_ID,
            supply: t,
            borrow: t,
            position: Pubkey::new_unique(),
            position_token_account: Pubkey::new_unique(),
            current_tick: 10,
            current_tick_id: Pubkey::new_unique(),
            final_tick: 12,
            final_tick_id: Pubkey::new_unique(),
            new_branch: Pubkey::new_unique(),
            oracle_sources: vec![Pubkey::new_unique()],
            branches: vec![Pubkey::new_unique()],
            tick_has_debt: vec![Pubkey::new_unique(), Pubkey::new_unique()],
        };
        let ix = operate(&a, -5, 7);
        assert_eq!(ix.accounts.len(), 35 + 4);
        assert_eq!(ix.data.len(), 8 + 16 + 16 + 1 + 4 + 3);
        assert_eq!(i128::from_le_bytes(ix.data[8..24].try_into().unwrap()), -5);
        assert_eq!(i128::from_le_bytes(ix.data[24..40].try_into().unwrap()), 7);
        assert_eq!(&ix.data[40..], &[0, 3, 0, 0, 0, 1, 1, 2]);
    }
}
