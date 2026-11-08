//! Jupiter Lend account layouts, from Instadapp/fluid-solana-programs at
//! 626b177 (the Code4rena-audited commit). On-chain structs are
//! `#[repr(C, packed)]`, so fields are read at fixed byte offsets.

use anyhow::{bail, ensure, Result};
use solana_program::{hash::hash, pubkey::Pubkey};

pub const POSITION_ACCOUNT_LEN: usize = 71;
const DISCRIMINATOR_LEN: usize = 8;
/// Byte offset of `Position::position_mint` in the full account (for gPA filters).
pub const POSITION_MINT_OFFSET: usize = DISCRIMINATOR_LEN + 6;
const VAULT_CONFIG_PAYLOAD_LEN: usize = 211;
const VAULT_STATE_PAYLOAD_LEN: usize = 119;
const VAULT_METADATA_PAYLOAD_LEN: usize = 34;
const TICK_PAYLOAD_LEN: usize = 32;
const TICK_HAS_DEBT_PAYLOAD_LEN: usize = 3 + 8 * 32;
const FLASHLOAN_ADMIN_PAYLOAD_LEN: usize = 67;
const ORACLE_SOURCE_LEN: usize = 66;

/// Anchor discriminator: `sha256("<namespace>:<name>")[..8]`.
pub(crate) fn discriminator(namespace: &str, name: &str) -> [u8; 8] {
    let digest = hash(format!("{namespace}:{name}").as_bytes()).to_bytes();
    digest[..8].try_into().expect("8 bytes")
}

fn payload<'a>(data: &'a [u8], name: &str, min_len: usize) -> Result<&'a [u8]> {
    ensure!(
        data.len() >= DISCRIMINATOR_LEN + min_len,
        "{name} account is {} bytes, expected at least {}",
        data.len(),
        DISCRIMINATOR_LEN + min_len
    );
    let (disc, payload) = data.split_at(DISCRIMINATOR_LEN);
    ensure!(disc == discriminator("account", name), "account is not a Jupiter Lend {name}");
    Ok(payload)
}

fn bytes<const N: usize>(p: &[u8], at: usize) -> [u8; N] {
    p[at..at + N].try_into().expect("bounds checked by payload()")
}

fn pubkey(p: &[u8], at: usize) -> Pubkey {
    Pubkey::new_from_array(bytes(p, at))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub vault_id: u16,
    pub nft_id: u32,
    /// NFT whose holder owns the position.
    pub position_mint: Pubkey,
    pub supply_only: bool,
    pub tick: i32,
    pub tick_id: u32,
    /// Collateral in raw vault units (9-decimal scale, before exchange price).
    pub supply_raw: u64,
    pub dust_debt_raw: u64,
}

pub fn parse_position(data: &[u8]) -> Result<Position> {
    ensure!(
        data.len() == POSITION_ACCOUNT_LEN,
        "Position account is {} bytes, expected {POSITION_ACCOUNT_LEN}",
        data.len()
    );
    let p = payload(data, "Position", POSITION_ACCOUNT_LEN - DISCRIMINATOR_LEN)?;
    Ok(Position {
        vault_id: u16::from_le_bytes(bytes(p, 0)),
        nft_id: u32::from_le_bytes(bytes(p, 2)),
        position_mint: pubkey(p, 6),
        supply_only: p[38] == 1,
        tick: i32::from_le_bytes(bytes(p, 39)),
        tick_id: u32::from_le_bytes(bytes(p, 43)),
        supply_raw: u64::from_le_bytes(bytes(p, 47)),
        dust_debt_raw: u64::from_le_bytes(bytes(p, 55)),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VaultConfig {
    pub vault_id: u16,
    /// Max LTV for borrowing, per mille (750 = 75%).
    pub collateral_factor: u16,
    /// LTV at which positions get liquidated, per mille.
    pub liquidation_threshold: u16,
    /// Fee added to new borrows, in basis points.
    pub borrow_fee: u16,
    pub oracle: Pubkey,
    pub oracle_program: Pubkey,
    /// Collateral mint.
    pub supply_token: Pubkey,
    /// Debt mint.
    pub borrow_token: Pubkey,
}

pub fn parse_vault_config(data: &[u8]) -> Result<VaultConfig> {
    let p = payload(data, "VaultConfig", VAULT_CONFIG_PAYLOAD_LEN)?;
    Ok(VaultConfig {
        vault_id: u16::from_le_bytes(bytes(p, 0)),
        collateral_factor: u16::from_le_bytes(bytes(p, 6)),
        liquidation_threshold: u16::from_le_bytes(bytes(p, 8)),
        borrow_fee: u16::from_le_bytes(bytes(p, 16)),
        oracle: pubkey(p, 18),
        oracle_program: pubkey(p, 114),
        supply_token: pubkey(p, 146),
        borrow_token: pubkey(p, 178),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VaultState {
    /// The current branch was liquidated; the next debt opens a new one.
    pub branch_liquidated: bool,
    pub topmost_tick: i32,
    pub current_branch_id: u32,
    pub total_branch_id: u32,
    /// NFT id the next `init_position` must use.
    pub next_position_id: u32,
}

pub fn parse_vault_state(data: &[u8]) -> Result<VaultState> {
    let p = payload(data, "VaultState", VAULT_STATE_PAYLOAD_LEN)?;
    Ok(VaultState {
        branch_liquidated: p[2] == 1,
        topmost_tick: i32::from_le_bytes(bytes(p, 3)),
        current_branch_id: u32::from_le_bytes(bytes(p, 7)),
        total_branch_id: u32::from_le_bytes(bytes(p, 11)),
        next_position_id: u32::from_le_bytes(bytes(p, 107)),
    })
}

/// The vault's own address lookup table.
pub fn parse_vault_metadata(data: &[u8]) -> Result<Pubkey> {
    Ok(pubkey(payload(data, "VaultMetadata", VAULT_METADATA_PAYLOAD_LEN)?, 2))
}

/// Flash loan fee in basis points, from the flashloan program's admin account.
pub fn parse_flashloan_fee(data: &[u8]) -> Result<u16> {
    Ok(u16::from_le_bytes(bytes(payload(data, "FlashloanAdmin", FLASHLOAN_ADMIN_PAYLOAD_LEN)?, 65)))
}

/// SPL mint decimals (Token and Token-2022 share the base layout).
pub fn mint_decimals(data: &[u8]) -> Result<u8> {
    ensure!(data.len() >= 82, "not a mint account");
    Ok(data[44])
}

/// Ticks per `TickHasDebtArray`: 8 bitmaps of 256 ticks.
pub const TICKS_PER_ARRAY: i32 = 2_048;

/// Index of the `TickHasDebtArray` holding `tick`.
pub fn tick_has_debt_index(tick: i32) -> u8 {
    ((tick - crate::tick::MIN_TICK) / TICKS_PER_ARRAY) as u8
}

/// One `TickHasDebtArray`: a bit per tick, set while the tick holds debt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickHasDebtArray {
    pub index: u8,
    bits: [u8; 256],
}

pub fn parse_tick_has_debt(data: &[u8]) -> Result<TickHasDebtArray> {
    let p = payload(data, "TickHasDebtArray", TICK_HAS_DEBT_PAYLOAD_LEN)?;
    Ok(TickHasDebtArray { index: p[2], bits: bytes(p, 3) })
}

impl TickHasDebtArray {
    fn first_tick(&self) -> i32 {
        crate::tick::MIN_TICK + self.index as i32 * TICKS_PER_ARRAY
    }

    /// Highest tick in this array below `tick` that holds debt.
    pub fn highest_below(&self, tick: i32) -> Option<i32> {
        let first = self.first_tick();
        let top = (tick - 1).min(first + TICKS_PER_ARRAY - 1);
        (first..=top).rev().find(|t| {
            let bit = (t - first) as usize;
            self.bits[bit / 8] & (1 << (bit % 8)) != 0
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tick {
    pub tick: i32,
    pub is_liquidated: bool,
    /// Bumped every time the tick is liquidated.
    pub total_ids: u32,
}

pub fn parse_tick(data: &[u8]) -> Result<Tick> {
    let p = payload(data, "Tick", TICK_PAYLOAD_LEN)?;
    Ok(Tick {
        tick: i32::from_le_bytes(bytes(p, 2)),
        is_liquidated: p[6] == 1,
        total_ids: u32::from_le_bytes(bytes(p, 7)),
    })
}

/// Oracle config: a chain of price sources the oracle program multiplies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Oracle {
    pub nonce: u16,
    /// Source accounts, in the order the oracle program expects them.
    pub sources: Vec<Pubkey>,
}

/// Borsh: `nonce: u16, sources: Vec<Sources>, bump: u8`, where each
/// `Sources` row is `source: Pubkey, invert: bool, multiplier: u128,
/// divisor: u128, source_type: u8` (66 bytes).
pub fn parse_oracle(data: &[u8]) -> Result<Oracle> {
    let p = payload(data, "Oracle", 6)?;
    let count = u32::from_le_bytes(bytes(p, 2)) as usize;
    let rows = &p[6..];
    if rows.len() < count * ORACLE_SOURCE_LEN {
        bail!("Oracle account truncated: {count} sources need {} bytes", count * ORACLE_SOURCE_LEN);
    }
    Ok(Oracle {
        nonce: u16::from_le_bytes(bytes(p, 0)),
        sources: (0..count).map(|i| pubkey(rows, i * ORACLE_SOURCE_LEN)).collect(),
    })
}

/// SPL token account owner (bytes 32..64), for finding who holds a position NFT.
pub fn token_account_owner(data: &[u8]) -> Result<Pubkey> {
    ensure!(data.len() >= 64, "not a token account");
    Ok(pubkey(data, 32))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn account(name: &str, payload: &[u8]) -> Vec<u8> {
        [&discriminator("account", name)[..], payload].concat()
    }

    pub(crate) fn position_bytes(vault_id: u16, tick: i32, supply_raw: u64, dust: u64) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&vault_id.to_le_bytes());
        p.extend_from_slice(&7u32.to_le_bytes());
        p.extend_from_slice(&[9u8; 32]);
        p.push(0);
        p.extend_from_slice(&tick.to_le_bytes());
        p.extend_from_slice(&3u32.to_le_bytes());
        p.extend_from_slice(&supply_raw.to_le_bytes());
        p.extend_from_slice(&dust.to_le_bytes());
        account("Position", &p)
    }

    #[test]
    fn parses_position() {
        let pos = parse_position(&position_bytes(12, -400, 5_000_000_000, 42)).unwrap();
        assert_eq!((pos.vault_id, pos.nft_id, pos.tick, pos.tick_id), (12, 7, -400, 3));
        assert_eq!((pos.supply_raw, pos.dust_debt_raw), (5_000_000_000, 42));
        assert_eq!(pos.position_mint, Pubkey::new_from_array([9; 32]));
        assert!(!pos.supply_only);
        assert!(parse_position(&position_bytes(1, 0, 1, 0)[..70]).is_err());
    }

    #[test]
    fn parses_vault_config_offsets() {
        let mut p = vec![0u8; VAULT_CONFIG_PAYLOAD_LEN];
        p[0..2].copy_from_slice(&5u16.to_le_bytes());
        p[6..8].copy_from_slice(&800u16.to_le_bytes());
        p[8..10].copy_from_slice(&850u16.to_le_bytes());
        p[16..18].copy_from_slice(&25u16.to_le_bytes());
        p[18..50].copy_from_slice(&[1; 32]);
        p[114..146].copy_from_slice(&[4; 32]);
        p[146..178].copy_from_slice(&[2; 32]);
        p[178..210].copy_from_slice(&[3; 32]);
        let cfg = parse_vault_config(&account("VaultConfig", &p)).unwrap();
        assert_eq!((cfg.vault_id, cfg.collateral_factor, cfg.liquidation_threshold), (5, 800, 850));
        assert_eq!(cfg.oracle, Pubkey::new_from_array([1; 32]));
        assert_eq!(cfg.borrow_fee, 25);
        assert_eq!(cfg.oracle_program, Pubkey::new_from_array([4; 32]));
        assert_eq!(cfg.supply_token, Pubkey::new_from_array([2; 32]));
        assert_eq!(cfg.borrow_token, Pubkey::new_from_array([3; 32]));
    }

    #[test]
    fn parses_oracle_sources() {
        let mut p = 9u16.to_le_bytes().to_vec();
        p.extend_from_slice(&2u32.to_le_bytes());
        for b in [4u8, 5] {
            p.extend_from_slice(&[b; 32]);
            p.extend_from_slice(&[0; ORACLE_SOURCE_LEN - 32]);
        }
        p.push(255); // bump
        let oracle = parse_oracle(&account("Oracle", &p)).unwrap();
        assert_eq!(oracle.nonce, 9);
        assert_eq!(
            oracle.sources,
            vec![Pubkey::new_from_array([4; 32]), Pubkey::new_from_array([5; 32])]
        );
    }

    #[test]
    fn parses_vault_state() {
        let mut p = vec![0u8; VAULT_STATE_PAYLOAD_LEN];
        p[2] = 1;
        p[3..7].copy_from_slice(&(-120i32).to_le_bytes());
        p[7..11].copy_from_slice(&4u32.to_le_bytes());
        p[11..15].copy_from_slice(&9u32.to_le_bytes());
        let s = parse_vault_state(&account("VaultState", &p)).unwrap();
        assert!(s.branch_liquidated);
        assert_eq!((s.topmost_tick, s.current_branch_id, s.total_branch_id), (-120, 4, 9));
    }

    #[test]
    fn tick_has_debt_bitmap() {
        assert_eq!(tick_has_debt_index(crate::tick::MIN_TICK), 0);
        assert_eq!(tick_has_debt_index(0), 7);
        assert_eq!(tick_has_debt_index(crate::tick::MAX_TICK), 15);
        // Array 8 starts at tick 1; set ticks 1 and 300.
        let mut p = vec![0u8; TICK_HAS_DEBT_PAYLOAD_LEN];
        p[2] = 8;
        p[3] |= 1; // tick 1: bit 0
        p[3 + 299 / 8] |= 1 << (299 % 8); // tick 300
        let a = parse_tick_has_debt(&account("TickHasDebtArray", &p)).unwrap();
        assert_eq!(a.highest_below(5_000), Some(300));
        assert_eq!(a.highest_below(300), Some(1));
        assert_eq!(a.highest_below(1), None);
    }

    #[test]
    fn rejects_wrong_discriminator() {
        assert!(parse_tick(&account("Position", &[0; TICK_PAYLOAD_LEN])).is_err());
    }
}
