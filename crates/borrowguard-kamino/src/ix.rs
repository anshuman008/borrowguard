//! klend instruction builders.

use solana_program::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
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
}
