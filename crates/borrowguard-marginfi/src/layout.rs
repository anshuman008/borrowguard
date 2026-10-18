//! marginfi v2 account layouts, read at fixed byte offsets.
//!
//! Offsets follow `type-crate` in mrgnlabs/marginfi-v2: `MarginfiAccount` is
//! 2304 bytes and `Bank` 3904, each after an 8-byte Anchor discriminator. Only
//! the fields BorrowGuard needs are decoded.

use anyhow::{ensure, Result};
use solana_program::pubkey::Pubkey;

pub const ACCOUNT_LEN: usize = 8 + 2304;
pub const BANK_LEN: usize = 8 + 3904;
pub const ACCOUNT_DISCRIMINATOR: [u8; 8] = [67, 178, 130, 109, 126, 114, 28, 42];
pub const BANK_DISCRIMINATOR: [u8; 8] = [142, 49, 166, 242, 50, 66, 97, 188];
/// Offset of `MarginfiAccount::authority`, for getProgramAccounts filters.
pub const AUTHORITY_OFFSET: usize = 40;

const BALANCES_OFFSET: usize = 72;
const BALANCE_LEN: usize = 104;
pub const MAX_BALANCES: usize = 16;
const HEALTH_CACHE_OFFSET: usize = 1840;

const HEALTHY: u32 = 1;
const ENGINE_OK: u32 = 2;
const ORACLE_OK: u32 = 4;

/// One active slot of an account's lending balances.
#[derive(Debug, Clone, PartialEq)]
pub struct Balance {
    /// Slot in the balances array; health-cache prices use the same index.
    pub index: usize,
    pub bank: Pubkey,
    pub asset_shares: f64,
    pub liability_shares: f64,
}

/// The risk engine's last verdict, written by `lending_account_pulse_health`.
/// Values are in USD.
#[derive(Debug, Clone, PartialEq)]
pub struct HealthCache {
    /// Collateral at initial (borrowing) weights.
    pub asset_value: f64,
    /// Debt at initial (borrowing) weights.
    pub liability_value: f64,
    /// Collateral at maintenance (liquidation) weights.
    pub asset_value_maint: f64,
    /// Debt at maintenance (liquidation) weights.
    pub liability_value_maint: f64,
    pub flags: u32,
    pub mrgn_err: u32,
    pub internal_err: u32,
    /// USD price of each balance, by slot.
    pub prices: [f64; MAX_BALANCES],
}

impl HealthCache {
    pub fn healthy(&self) -> bool {
        self.flags & HEALTHY != 0
    }

    pub fn engine_ok(&self) -> bool {
        self.flags & ENGINE_OK != 0
    }

    pub fn oracle_ok(&self) -> bool {
        self.flags & ORACLE_OK != 0
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MarginfiAccount {
    pub group: Pubkey,
    pub authority: Pubkey,
    /// Active balances only, in slot order.
    pub balances: Vec<Balance>,
    pub health: HealthCache,
}

impl MarginfiAccount {
    pub fn deposits(&self) -> impl Iterator<Item = &Balance> {
        self.balances.iter().filter(|b| b.asset_shares > 0.0)
    }

    pub fn borrows(&self) -> impl Iterator<Item = &Balance> {
        self.balances.iter().filter(|b| b.liability_shares > 0.0)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Bank {
    pub mint: Pubkey,
    pub decimals: u8,
    pub group: Pubkey,
    pub asset_share_value: f64,
    pub liability_share_value: f64,
    pub asset_weight_init: f64,
    pub asset_weight_maint: f64,
    pub liability_weight_init: f64,
    pub liability_weight_maint: f64,
    pub oracle_setup: u8,
    pub oracle_keys: [Pubkey; 5],
    pub asset_tag: u8,
}

pub fn parse_account(data: &[u8]) -> Result<MarginfiAccount> {
    ensure!(
        data.len() == ACCOUNT_LEN && data[..8] == ACCOUNT_DISCRIMINATOR,
        "not a marginfi account ({} bytes)",
        data.len()
    );
    let balances = (0..MAX_BALANCES)
        .filter_map(|index| {
            let b = &data[BALANCES_OFFSET + index * BALANCE_LEN..][..BALANCE_LEN];
            (b[0] != 0).then(|| Balance {
                index,
                bank: pubkey(&b[1..]),
                asset_shares: i80f48(&b[40..]),
                liability_shares: i80f48(&b[56..]),
            })
        })
        .collect();
    let h = &data[HEALTH_CACHE_OFFSET..];
    Ok(MarginfiAccount {
        group: pubkey(&data[8..]),
        authority: pubkey(&data[AUTHORITY_OFFSET..]),
        balances,
        health: HealthCache {
            asset_value: i80f48(h),
            liability_value: i80f48(&h[16..]),
            asset_value_maint: i80f48(&h[32..]),
            liability_value_maint: i80f48(&h[48..]),
            flags: u32_at(h, 104),
            mrgn_err: u32_at(h, 108),
            internal_err: u32_at(h, 240),
            prices: std::array::from_fn(|i| {
                f64::from_le_bytes(h[112 + 8 * i..][..8].try_into().unwrap())
            }),
        },
    })
}

pub fn parse_bank(data: &[u8]) -> Result<Bank> {
    ensure!(
        data.len() == BANK_LEN && data[..8] == BANK_DISCRIMINATOR,
        "not a marginfi bank ({} bytes)",
        data.len()
    );
    Ok(Bank {
        mint: pubkey(&data[8..]),
        decimals: data[40],
        group: pubkey(&data[41..]),
        asset_share_value: i80f48(&data[80..]),
        liability_share_value: i80f48(&data[96..]),
        asset_weight_init: i80f48(&data[296..]),
        asset_weight_maint: i80f48(&data[312..]),
        liability_weight_init: i80f48(&data[328..]),
        liability_weight_maint: i80f48(&data[344..]),
        oracle_setup: data[609],
        oracle_keys: std::array::from_fn(|i| pubkey(&data[610 + 32 * i..])),
        asset_tag: data[785],
    })
}

fn pubkey(b: &[u8]) -> Pubkey {
    Pubkey::new_from_array(b[..32].try_into().unwrap())
}

fn u32_at(b: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(b[offset..][..4].try_into().unwrap())
}

/// marginfi's `WrappedI80F48`: a little-endian i128 with 48 fractional bits.
fn i80f48(b: &[u8]) -> f64 {
    i128::from_le_bytes(b[..16].try_into().unwrap()) as f64 / (1u64 << 48) as f64
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn put_i80f48(buf: &mut [u8], offset: usize, v: f64) {
        let raw = (v * (1u64 << 48) as f64) as i128;
        buf[offset..offset + 16].copy_from_slice(&raw.to_le_bytes());
    }

    pub(crate) fn bank_bytes(mint: Pubkey, decimals: u8, weights: [f64; 4]) -> Vec<u8> {
        let mut d = vec![0u8; BANK_LEN];
        d[..8].copy_from_slice(&BANK_DISCRIMINATOR);
        d[8..40].copy_from_slice(mint.as_ref());
        d[40] = decimals;
        put_i80f48(&mut d, 80, 1.0);
        put_i80f48(&mut d, 96, 1.0);
        for (i, w) in weights.into_iter().enumerate() {
            put_i80f48(&mut d, 296 + 16 * i, w);
        }
        d[609] = 3; // PythPushOracle
        d[610..642].copy_from_slice(Pubkey::new_unique().as_ref());
        d
    }

    #[test]
    fn i80f48_decodes_fractions_and_negatives() {
        let mut b = [0u8; 16];
        put_i80f48(&mut b, 0, 1.5);
        assert_eq!(i80f48(&b), 1.5);
        put_i80f48(&mut b, 0, -2.25);
        assert_eq!(i80f48(&b), -2.25);
    }

    #[test]
    fn rejects_wrong_sizes_and_discriminators() {
        assert!(parse_account(&[0u8; 100]).is_err());
        let mut d = vec![0u8; ACCOUNT_LEN];
        assert!(parse_account(&d).is_err());
        d[..8].copy_from_slice(&ACCOUNT_DISCRIMINATOR);
        assert!(parse_account(&d).is_ok());
        assert!(parse_bank(&d).is_err());
    }

    #[test]
    fn bank_round_trip() {
        let mint = Pubkey::new_unique();
        let bank = parse_bank(&bank_bytes(mint, 9, [0.8, 0.9, 1.1, 1.05])).unwrap();
        assert_eq!((bank.mint, bank.decimals, bank.oracle_setup), (mint, 9, 3));
        assert!((bank.asset_weight_maint - 0.9).abs() < 1e-12);
        assert!((bank.liability_weight_maint - 1.05).abs() < 1e-12);
    }
}
