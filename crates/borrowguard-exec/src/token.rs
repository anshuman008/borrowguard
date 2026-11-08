//! SPL associated token accounts, and wrapping native SOL.

use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey,
    pubkey::Pubkey,
    system_instruction, system_program,
};

pub const ASSOCIATED_TOKEN_PROGRAM_ID: Pubkey =
    pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
pub const TOKEN_PROGRAM_ID: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
/// Wrapped SOL: lending programs move SOL as this SPL token.
pub const NATIVE_MINT: Pubkey = pubkey!("So11111111111111111111111111111111111111112");

pub fn associated_token_address(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ASSOCIATED_TOKEN_PROGRAM_ID,
    )
    .0
}

/// Associated-token-program `CreateIdempotent`: no-op if the account exists.
pub fn create_ata_idempotent(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Instruction {
    Instruction {
        program_id: ASSOCIATED_TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*owner, true),
            AccountMeta::new(associated_token_address(owner, mint, token_program), false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(system_program::ID, false),
            AccountMeta::new_readonly(*token_program, false),
        ],
        data: vec![1],
    }
}

/// Move `lamports` of the owner's SOL into their wSOL account (which must
/// exist) and credit it as tokens.
pub fn wrap_sol(owner: &Pubkey, lamports: u64) -> [Instruction; 2] {
    let ata = associated_token_address(owner, &NATIVE_MINT, &TOKEN_PROGRAM_ID);
    [
        system_instruction::transfer(owner, &ata, lamports),
        Instruction {
            program_id: TOKEN_PROGRAM_ID,
            accounts: vec![AccountMeta::new(ata, false)],
            data: vec![17], // SyncNative
        },
    ]
}

/// Close the owner's wSOL account, turning its whole balance back into SOL.
pub fn unwrap_sol(owner: &Pubkey) -> Instruction {
    Instruction {
        program_id: TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(
                associated_token_address(owner, &NATIVE_MINT, &TOKEN_PROGRAM_ID),
                false,
            ),
            AccountMeta::new(*owner, false),
            AccountMeta::new_readonly(*owner, true),
        ],
        data: vec![9], // CloseAccount
    }
}
