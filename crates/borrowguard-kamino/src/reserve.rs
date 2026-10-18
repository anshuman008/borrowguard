//! The handful of klend `Reserve` fields BorrowGuard needs, read at fixed
//! offsets (klend release/v1.18.0). The full struct is ~400 fields; a narrow
//! offset map is easier to re-audit when klend changes.

use solana_program::pubkey::Pubkey;

use crate::layout::{check_account, sf_to_f64, LayoutError};

pub const RESERVE_ACCOUNT_LEN: usize = 8624;

// Offsets within the payload (after the 8-byte discriminator).
const LENDING_MARKET: usize = 24;
const FARM_COLLATERAL: usize = 56;
const FARM_DEBT: usize = 88;
const LIQUIDITY: usize = 120;
const LIQUIDITY_MINT: usize = LIQUIDITY;
const LIQUIDITY_SUPPLY_VAULT: usize = LIQUIDITY + 32;
const LIQUIDITY_FEE_VAULT: usize = LIQUIDITY + 64;
const LIQUIDITY_AVAILABLE: usize = LIQUIDITY + 96;
const LIQUIDITY_BORROWED_SF: usize = LIQUIDITY + 104;
const LIQUIDITY_MARKET_PRICE_SF: usize = LIQUIDITY + 120;
const LIQUIDITY_MINT_DECIMALS: usize = LIQUIDITY + 144;
const LIQUIDITY_PROTOCOL_FEES_SF: usize = LIQUIDITY + 216;
const LIQUIDITY_REFERRER_FEES_SF: usize = LIQUIDITY + 232;
const LIQUIDITY_PENDING_REFERRER_FEES_SF: usize = LIQUIDITY + 248;
const LIQUIDITY_TOKEN_PROGRAM: usize = LIQUIDITY + 280;
const COLLATERAL: usize = 2552;
const COLLATERAL_MINT: usize = COLLATERAL;
const COLLATERAL_MINT_SUPPLY: usize = COLLATERAL + 32;
const COLLATERAL_SUPPLY_VAULT: usize = COLLATERAL + 40;
const CONFIG: usize = 4848;
const CONFIG_FLASH_LOAN_FEE_SF: usize = CONFIG + 48;
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
    /// Vault holding the reserve's liquidity (borrows are paid from here).
    pub supply_vault: Pubkey,
    pub fee_vault: Pubkey,
    pub token_program: Pubkey,
    /// Reserve collateral ("cToken") mint and the vault holding deposits.
    pub collateral_mint: Pubkey,
    pub collateral_supply_vault: Pubkey,
    pub collateral_mint_supply: u64,
    /// Liquidity owned by depositors, base units (available + borrowed - fees).
    pub total_liquidity: u128,
    /// Kamino farms attached to deposits / borrows of this reserve, if any.
    pub farm_collateral: Option<Pubkey>,
    pub farm_debt: Option<Pubkey>,
    /// Flash loan fee as a fraction of the amount (0.0 on most reserves).
    pub flash_loan_fee: f64,
}

impl Reserve {
    /// cTokens to withdraw to receive at least `liquidity` base units. klend
    /// redeems `ctokens * total_liquidity / ctoken_supply` rounded down, so
    /// round up here and add one unit of margin.
    pub fn collateral_for_liquidity(&self, liquidity: u64) -> Option<u64> {
        let supply = self.collateral_mint_supply as u128;
        (self.total_liquidity > 0 && supply > 0).then_some(())?;
        let ctokens = (liquidity as u128 * supply).div_ceil(self.total_liquidity) + 1;
        u64::try_from(ctokens).ok()
    }
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
        supply_vault: pubkey(p, LIQUIDITY_SUPPLY_VAULT),
        fee_vault: pubkey(p, LIQUIDITY_FEE_VAULT),
        token_program: pubkey(p, LIQUIDITY_TOKEN_PROGRAM),
        collateral_mint: pubkey(p, COLLATERAL_MINT),
        collateral_supply_vault: pubkey(p, COLLATERAL_SUPPLY_VAULT),
        collateral_mint_supply: u64_at(p, COLLATERAL_MINT_SUPPLY),
        total_liquidity: total_liquidity(p),
        farm_collateral: oracle(p, FARM_COLLATERAL),
        farm_debt: oracle(p, FARM_DEBT),
        flash_loan_fee: sf_to_f64(u64_at(p, CONFIG_FLASH_LOAN_FEE_SF) as u128),
    })
}

/// klend `ReserveLiquidity::total_supply`: available + borrowed - all fees.
fn total_liquidity(p: &[u8]) -> u128 {
    let sf = |at: usize| u128::from_le_bytes(p[at..at + 16].try_into().unwrap());
    let total_sf = ((u64_at(p, LIQUIDITY_AVAILABLE) as u128) << 60) + sf(LIQUIDITY_BORROWED_SF)
        - sf(LIQUIDITY_PROTOCOL_FEES_SF)
        - sf(LIQUIDITY_REFERRER_FEES_SF)
        - sf(LIQUIDITY_PENDING_REFERRER_FEES_SF);
    total_sf >> 60
}

fn pubkey(p: &[u8], at: usize) -> Pubkey {
    Pubkey::new_from_array(p[at..at + 32].try_into().unwrap())
}

fn u64_at(p: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(p[at..at + 8].try_into().unwrap())
}

/// Unused optional pubkeys (oracles, farms) hold the default pubkey.
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
    fn collateral_for_liquidity_rounds_up() {
        let mut r = parse_reserve(&reserve_bytes("SOL", 100.0, 9)).unwrap();
        // 1.1 liquidity per cToken: 1,000 cTokens back 1,100 liquidity.
        r.collateral_mint_supply = 1_000;
        r.total_liquidity = 1_100;
        let ctokens = r.collateral_for_liquidity(110).unwrap();
        assert!(ctokens as u128 * r.total_liquidity / r.collateral_mint_supply as u128 >= 110);
        assert_eq!(ctokens, 101);
        r.total_liquidity = 0;
        assert_eq!(r.collateral_for_liquidity(1), None);
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
