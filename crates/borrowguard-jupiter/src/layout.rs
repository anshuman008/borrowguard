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
const TICK_PAYLOAD_LEN: usize = 32;
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
    pub oracle: Pubkey,
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
        oracle: pubkey(p, 18),
        supply_token: pubkey(p, 146),
        borrow_token: pubkey(p, 178),
    })
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
        p[18..50].copy_from_slice(&[1; 32]);
        p[146..178].copy_from_slice(&[2; 32]);
        p[178..210].copy_from_slice(&[3; 32]);
        let cfg = parse_vault_config(&account("VaultConfig", &p)).unwrap();
        assert_eq!((cfg.vault_id, cfg.collateral_factor, cfg.liquidation_threshold), (5, 800, 850));
        assert_eq!(cfg.oracle, Pubkey::new_from_array([1; 32]));
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
    fn rejects_wrong_discriminator() {
        assert!(parse_tick(&account("Position", &[0; TICK_PAYLOAD_LEN])).is_err());
    }
}
