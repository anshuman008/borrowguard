//! Wire-exact klend `Obligation` layout (klend release/v1.18.0).
//!
//! `u128` fields use [`U128Le`] (8-byte aligned, like BPF) so zero-copy
//! casts match on-chain bytes on every host. Size asserts at the bottom fail
//! the build if a field drifts.

use bytemuck::{Pod, Zeroable};
use solana_program::{hash::hash, pubkey::Pubkey};

pub const OBLIGATION_ACCOUNT_LEN: usize = 3344;
/// Byte offset of `Obligation::owner` in the full account (for gPA filters).
pub const OBLIGATION_OWNER_OFFSET: usize = 64;
pub(crate) const DISCRIMINATOR_LEN: usize = 8;

/// klend scaled fractions (`*_sf`) carry 60 fractional bits.
pub(crate) fn sf_to_f64(sf: u128) -> f64 {
    sf as f64 / (1u128 << 60) as f64
}

/// Anchor discriminator: `sha256("<namespace>:<name>")[..8]`.
pub(crate) fn discriminator(namespace: &str, name: &str) -> [u8; 8] {
    let digest = hash(format!("{namespace}:{name}").as_bytes()).to_bytes();
    digest[..8].try_into().expect("8 bytes")
}

#[derive(Debug, thiserror::Error)]
pub enum LayoutError {
    #[error("{account} account is {got} bytes, expected {expected}")]
    WrongSize { account: &'static str, got: usize, expected: usize },
    #[error("account is not a klend {0}")]
    WrongDiscriminator(&'static str),
}

pub(crate) fn check_account<'a>(
    data: &'a [u8],
    name: &'static str,
    len: usize,
) -> Result<&'a [u8], LayoutError> {
    if data.len() != len {
        return Err(LayoutError::WrongSize { account: name, got: data.len(), expected: len });
    }
    let (disc, payload) = data.split_at(DISCRIMINATOR_LEN);
    if disc != discriminator("account", name) {
        return Err(LayoutError::WrongDiscriminator(name));
    }
    Ok(payload)
}

/// u128 with BPF's 8-byte alignment, little-endian.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Zeroable, Pod)]
pub struct U128Le(pub [u64; 2]);

impl U128Le {
    pub const fn new(v: u128) -> Self {
        Self([v as u64, (v >> 64) as u64])
    }

    pub fn get(self) -> u128 {
        self.0[0] as u128 | (self.0[1] as u128) << 64
    }

    /// Value of a scaled fraction (`*_sf`) field.
    pub fn to_f64(self) -> f64 {
        sf_to_f64(self.get())
    }
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Zeroable, Pod)]
pub struct BigFractionBytes {
    pub value: [u64; 4],
    pub padding: [u64; 2],
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Zeroable, Pod)]
pub struct LastUpdate {
    pub slot: u64,
    pub stale: u8,
    pub price_status: u8,
    pub placeholder: [u8; 6],
}

impl LastUpdate {
    /// klend's `LIQUIDATION_CHECKS`: price loaded, age checked, usage allowed.
    const LIQUIDATION_CHECKS: u8 = 0b0010_0011;

    /// Prices behind this update are good enough for klend to liquidate on.
    pub fn prices_ok(&self) -> bool {
        self.price_status & Self::LIQUIDATION_CHECKS == Self::LIQUIDATION_CHECKS
    }
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Zeroable, Pod)]
pub struct ObligationCollateral {
    pub deposit_reserve: Pubkey,
    /// Reserve collateral (cToken) amount, not underlying tokens.
    pub deposited_amount: u64,
    /// USD value as of the obligation's last refresh.
    pub market_value_sf: U128Le,
    pub borrowed_amount_against_this_collateral_in_elevation_group: u64,
    pub padding: [u64; 9],
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Zeroable, Pod)]
pub struct ObligationLiquidity {
    pub borrow_reserve: Pubkey,
    pub cumulative_borrow_rate_bsf: BigFractionBytes,
    pub last_borrowed_at_timestamp: u64,
    /// Debt in token base units, scaled by 2^60.
    pub borrowed_amount_sf: U128Le,
    pub market_value_sf: U128Le,
    pub borrow_factor_adjusted_market_value_sf: U128Le,
    pub borrowed_amount_outside_elevation_groups: u64,
    pub fixed_term_borrow_rollover_config: FixedTermBorrowRolloverConfig,
    pub borrowed_amount_at_expiration: u64,
    pub padding2: [u64; 4],
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Zeroable, Pod)]
pub struct FixedTermBorrowRolloverConfig {
    pub auto_rollover_enabled: u8,
    pub open_term_allowed: u8,
    pub migration_to_fixed_enabled: u8,
    pub alignment_padding: [u8; 1],
    pub max_borrow_rate_bps: u32,
    pub min_debt_term_seconds: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Zeroable, Pod)]
pub struct ObligationOrder {
    pub condition_threshold_sf: U128Le,
    pub opportunity_parameter_sf: U128Le,
    pub min_execution_bonus_bps: u16,
    pub max_execution_bonus_bps: u16,
    pub condition_type: u8,
    pub opportunity_type: u8,
    pub padding1: [u8; 10],
    pub padding2: [U128Le; 5],
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Zeroable, Pod)]
pub struct BorrowOrder {
    pub debt_liquidity_mint: Pubkey,
    pub remaining_debt_amount: u64,
    pub filled_debt_destination: Pubkey,
    pub min_debt_term_seconds: u64,
    pub fillable_until_timestamp: u64,
    pub placed_at_timestamp: u64,
    pub last_updated_at_timestamp: u64,
    pub requested_debt_amount: u64,
    pub max_borrow_rate_bps: u32,
    pub active: u8,
    pub enable_auto_rollover_on_filled_borrows: u8,
    pub padding1: [u8; 2],
    pub end_padding: [u64; 5],
}

/// `Obligation` payload (account bytes after the 8-byte discriminator).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Zeroable, Pod)]
pub struct Obligation {
    pub tag: u64,
    pub last_update: LastUpdate,
    pub lending_market: Pubkey,
    pub owner: Pubkey,
    pub deposits: [ObligationCollateral; 8],
    pub lowest_reserve_deposit_liquidation_ltv: u64,
    pub deposited_value_sf: U128Le,
    pub borrows: [ObligationLiquidity; 5],
    pub borrow_factor_adjusted_debt_value_sf: U128Le,
    pub borrowed_assets_market_value_sf: U128Le,
    pub allowed_borrow_value_sf: U128Le,
    pub unhealthy_borrow_value_sf: U128Le,
    pub padding_deprecated_asset_tiers: [u8; 13],
    pub elevation_group: u8,
    pub num_of_obsolete_deposit_reserves: u8,
    pub has_debt: u8,
    pub referrer: Pubkey,
    pub borrowing_disabled: u8,
    pub autodeleverage_target_ltv_pct: u8,
    pub lowest_reserve_deposit_max_ltv_pct: u8,
    pub num_of_obsolete_borrow_reserves: u8,
    pub reserved: [u8; 4],
    pub highest_borrow_factor_pct: u64,
    pub autodeleverage_margin_call_started_timestamp: u64,
    pub obligation_orders: [ObligationOrder; 2],
    pub borrow_order: BorrowOrder,
    pub padding_3: [u64; 73],
}

impl Obligation {
    pub fn active_deposits(&self) -> impl Iterator<Item = &ObligationCollateral> {
        self.deposits
            .iter()
            .filter(|d| d.deposit_reserve != Pubkey::default() && d.deposited_amount > 0)
    }

    pub fn active_borrows(&self) -> impl Iterator<Item = &ObligationLiquidity> {
        self.borrows
            .iter()
            .filter(|b| b.borrow_reserve != Pubkey::default() && b.borrowed_amount_sf.get() > 0)
    }

    /// Kamino product tag: what kind of position the UI opened.
    pub fn kind(&self) -> &'static str {
        match self.tag {
            0 => "vanilla",
            1 => "multiply",
            2 => "lending",
            3 => "leverage",
            _ => "unknown",
        }
    }
}

pub fn parse_obligation(data: &[u8]) -> Result<Obligation, LayoutError> {
    let payload = check_account(data, "Obligation", OBLIGATION_ACCOUNT_LEN)?;
    // Account buffers carry no alignment guarantee; copy instead of casting.
    Ok(bytemuck::pod_read_unaligned(payload))
}

const _: () = {
    use std::mem::size_of;
    assert!(size_of::<LastUpdate>() == 16);
    assert!(size_of::<ObligationCollateral>() == 136);
    assert!(size_of::<ObligationLiquidity>() == 200);
    assert!(size_of::<ObligationOrder>() == 128);
    assert!(size_of::<BorrowOrder>() == 160);
    assert!(size_of::<Obligation>() == OBLIGATION_ACCOUNT_LEN - DISCRIMINATOR_LEN);
    // owner = disc(8) + tag(8) + last_update(16) + lending_market(32)
    assert!(OBLIGATION_OWNER_OFFSET == DISCRIMINATOR_LEN + 8 + 16 + 32);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u128le_round_trips() {
        let v = (7u128 << 64) | 42;
        assert_eq!(U128Le::new(v).get(), v);
        assert_eq!(U128Le::new(3 << 60).to_f64(), 3.0);
    }

    #[test]
    fn rejects_wrong_size_and_discriminator() {
        assert!(matches!(parse_obligation(&[0; 10]), Err(LayoutError::WrongSize { .. })));
        assert!(matches!(
            parse_obligation(&[0; OBLIGATION_ACCOUNT_LEN]),
            Err(LayoutError::WrongDiscriminator(_))
        ));
    }

    #[test]
    fn prices_ok_needs_liquidation_checks() {
        let mut lu = LastUpdate::default();
        assert!(!lu.prices_ok());
        lu.price_status = 0b0011_1111;
        assert!(lu.prices_ok());
    }
}
