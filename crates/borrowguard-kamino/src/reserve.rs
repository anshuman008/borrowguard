//! The handful of klend `Reserve` fields BorrowGuard needs, read at fixed
//! offsets (klend release/v1.18.0). The full struct is ~400 fields; a narrow
//! offset map is easier to re-audit when klend changes.

use solana_program::pubkey::Pubkey;

use crate::layout::{check_account, sf_to_f64, LayoutError};

pub const RESERVE_ACCOUNT_LEN: usize = 8624;

// Offsets within the payload (after the 8-byte discriminator).
const LENDING_MARKET: usize = 24;
const LIQUIDITY: usize = 120;
const LIQUIDITY_MINT: usize = LIQUIDITY;
const LIQUIDITY_MARKET_PRICE_SF: usize = LIQUIDITY + 120;
const LIQUIDITY_MINT_DECIMALS: usize = LIQUIDITY + 144;
const CONFIG: usize = 4848;
const CONFIG_LOAN_TO_VALUE_PCT: usize = CONFIG + 16;
const CONFIG_LIQUIDATION_THRESHOLD_PCT: usize = CONFIG + 17;
const CONFIG_BORROW_FACTOR_PCT: usize = CONFIG + 152;
const TOKEN_INFO: usize = CONFIG + 176;
const TOKEN_NAME: usize = TOKEN_INFO;
const SCOPE_PRICES: usize = TOKEN_INFO + 80;
const SWITCHBOARD_PRICE: usize = TOKEN_INFO + 128;
const SWITCHBOARD_TWAP: usize = TOKEN_INFO + 160;
const PYTH_PRICE: usize = TOKEN_INFO + 192;

#[derive(Debug, Clone, PartialEq)]
pub struct Reserve {
    pub lending_market: Pubkey,
    pub mint: Pubkey,
    pub decimals: u32,
    /// Token symbol from the reserve's config, e.g. "SOL".
    pub name: String,
    /// USD price per whole token as of the reserve's last refresh.
    pub market_price: f64,
    pub loan_to_value_pct: u8,
    pub liquidation_threshold_pct: u8,
    pub borrow_factor_pct: u64,
    pub scope_prices: Option<Pubkey>,
    pub switchboard_price: Option<Pubkey>,
    pub switchboard_twap: Option<Pubkey>,
    pub pyth_price: Option<Pubkey>,
}

pub fn parse_reserve(data: &[u8]) -> Result<Reserve, LayoutError> {
    let p = check_account(data, "Reserve", RESERVE_ACCOUNT_LEN)?;
    let name = &p[TOKEN_NAME..TOKEN_NAME + 32];
    Ok(Reserve {
        lending_market: pubkey(p, LENDING_MARKET),
        mint: pubkey(p, LIQUIDITY_MINT),
        decimals: u64_at(p, LIQUIDITY_MINT_DECIMALS) as u32,
        name: String::from_utf8_lossy(name).trim_end_matches('\0').to_string(),
        market_price: sf_to_f64(u128::from_le_bytes(
            p[LIQUIDITY_MARKET_PRICE_SF..LIQUIDITY_MARKET_PRICE_SF + 16].try_into().unwrap(),
        )),
        loan_to_value_pct: p[CONFIG_LOAN_TO_VALUE_PCT],
        liquidation_threshold_pct: p[CONFIG_LIQUIDATION_THRESHOLD_PCT],
        borrow_factor_pct: u64_at(p, CONFIG_BORROW_FACTOR_PCT),
        scope_prices: oracle(p, SCOPE_PRICES),
        switchboard_price: oracle(p, SWITCHBOARD_PRICE),
        switchboard_twap: oracle(p, SWITCHBOARD_TWAP),
        pyth_price: oracle(p, PYTH_PRICE),
    })
}

fn pubkey(p: &[u8], at: usize) -> Pubkey {
    Pubkey::new_from_array(p[at..at + 32].try_into().unwrap())
}

fn u64_at(p: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(p[at..at + 8].try_into().unwrap())
}

/// Unused oracle slots hold the default pubkey.
fn oracle(p: &[u8], at: usize) -> Option<Pubkey> {
    Some(pubkey(p, at)).filter(|pk| *pk != Pubkey::default())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::layout::{discriminator, DISCRIMINATOR_LEN};

    /// Minimal reserve account bytes for tests.
    pub(crate) fn reserve_bytes(name: &str, price: f64, decimals: u64) -> Vec<u8> {
        let mut data = vec![0u8; RESERVE_ACCOUNT_LEN];
        data[..8].copy_from_slice(&discriminator("account", "Reserve"));
        let p = &mut data[DISCRIMINATOR_LEN..];
        p[TOKEN_NAME..TOKEN_NAME + name.len()].copy_from_slice(name.as_bytes());
        let price_sf = (price * (1u128 << 60) as f64) as u128;
        p[LIQUIDITY_MARKET_PRICE_SF..LIQUIDITY_MARKET_PRICE_SF + 16]
            .copy_from_slice(&price_sf.to_le_bytes());
        p[LIQUIDITY_MINT_DECIMALS..LIQUIDITY_MINT_DECIMALS + 8]
            .copy_from_slice(&decimals.to_le_bytes());
        p[SCOPE_PRICES] = 7;
        data
    }

    #[test]
    fn parses_fields() {
        let r = parse_reserve(&reserve_bytes("SOL", 146.25, 9)).unwrap();
        assert_eq!(r.name, "SOL");
        assert_eq!(r.decimals, 9);
        assert!((r.market_price - 146.25).abs() < 1e-9);
        assert!(r.scope_prices.is_some());
        assert_eq!(r.pyth_price, None);
    }
}
