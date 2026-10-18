//! Parses real mainnet accounts captured 2026-10-19 (slot 458541201):
//! marginfi account 6ojWXoNM7Y5shCavHPMFMDAMMT3eXi66Mf9vgChyEd19 as it looked
//! after a simulated `lending_account_pulse_health`, and its two banks
//! (RNDR collateral, USDC debt).

use borrowguard_marginfi::{
    layout::{parse_account, parse_bank},
    pulse_health, snapshot_from, Pubkey,
};

const ACCOUNT: &[u8] = include_bytes!("fixtures/account_6ojWXoNM_pulsed.bin");
const RNDR_BANK: &[u8] = include_bytes!("fixtures/bank_EbuSnXdF.bin");
const USDC_BANK: &[u8] = include_bytes!("fixtures/bank_2s37akK2.bin");

fn close(a: f64, b: f64, tol: f64) -> bool {
    (a - b).abs() < tol
}

#[test]
fn parses_account_balances_and_health_cache() {
    let a = parse_account(ACCOUNT).unwrap();
    assert_eq!(a.group.to_string(), "4qp6Fx6tnZkY5Wropq9wUYgtFxXKwE6viZxFHg3rdAG8");
    assert_eq!(a.balances.len(), 2);
    assert!(a.balances[0].bank.to_string().starts_with("EbuSnXdF"));
    assert!(a.balances[1].bank.to_string().starts_with("2s37akK2"));
    assert!(a.balances[0].asset_shares > 0.0 && a.balances[0].liability_shares == 0.0);
    assert!(a.balances[1].liability_shares > 0.0 && a.balances[1].asset_shares == 0.0);

    let h = &a.health;
    assert!(h.healthy() && h.engine_ok() && h.oracle_ok());
    assert!(close(h.asset_value_maint, 1_491.83, 0.01), "{}", h.asset_value_maint);
    assert!(close(h.liability_value_maint, 978.80, 0.01), "{}", h.liability_value_maint);
    assert!(close(h.prices[0], 1.7676, 1e-4) && close(h.prices[1], 1.0025, 1e-4));
}

#[test]
fn parses_banks() {
    let rndr = parse_bank(RNDR_BANK).unwrap();
    let usdc = parse_bank(USDC_BANK).unwrap();
    assert_eq!(rndr.mint.to_string(), "rndrizKT3MK1iimdxRdWabcF7Zg7AR5T4nud4EkHBof");
    assert_eq!(usdc.mint.to_string(), "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
    assert_eq!((rndr.decimals, usdc.decimals), (8, 6));
    assert_eq!((rndr.oracle_setup, usdc.oracle_setup), (3, 3));
    assert!(close(rndr.asset_weight_maint, 0.33, 1e-6));
    assert!(close(usdc.liability_weight_maint, 1.05, 1e-6));
    assert!(rndr.asset_share_value > 0.5 && usdc.liability_share_value > 1.0);
}

#[test]
fn snapshot_from_real_accounts() {
    let a = parse_account(ACCOUNT).unwrap();
    let banks = [parse_bank(RNDR_BANK).unwrap(), parse_bank(USDC_BANK).unwrap()];
    let snap = snapshot_from(&a, &banks, 0).unwrap();
    assert_eq!(snap.debt_symbol, "USDC");
    assert!(close(snap.collateral, 2_560.07, 0.01), "{}", snap.collateral);
    assert!(close(snap.debt, 930.13, 0.01), "{}", snap.debt);
    assert!(close(snap.liquidation_ltv, 0.3141, 1e-3), "{}", snap.liquidation_ltv);
    assert!(0.0 < snap.max_ltv && snap.max_ltv < snap.liquidation_ltv);
    let leverage = snap.position().leverage(snap.price()).unwrap();
    assert!(close(leverage, 1.26, 0.01), "{leverage}");
    assert!(leverage < snap.liquidation_leverage());
}

#[test]
fn pulse_health_uses_each_banks_oracle() {
    let a = parse_account(ACCOUNT).unwrap();
    let banks: Vec<(Pubkey, _)> = a
        .balances
        .iter()
        .map(|b| b.bank)
        .zip([parse_bank(RNDR_BANK).unwrap(), parse_bank(USDC_BANK).unwrap()])
        .collect();
    let key: Pubkey = "6ojWXoNM7Y5shCavHPMFMDAMMT3eXi66Mf9vgChyEd19".parse().unwrap();
    let ix = pulse_health(&key, &a, &banks).unwrap();
    let keys: Vec<String> = ix.accounts.iter().map(|m| m.pubkey.to_string()).collect();
    assert_eq!(
        keys[2..],
        [
            "EbuSnXdFz1R4VPdaJ96KQQQmeYgZTHSzpNW94Tw1PE3H",
            "CQunoG15nDNGdM4C4JnDmYMEmZFnTkD5eSJGf118MfWR",
            "2s37akK2eyBbp8DZgCm7RtsaEz8eJP3Nxd4urLHQv7yB",
            "6HAuqASbHEh4w4REJEUUUCginTLfj1kwCh215ZLtMkrT",
        ]
    );
}
