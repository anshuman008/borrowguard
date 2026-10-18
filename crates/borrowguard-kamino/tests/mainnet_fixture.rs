//! Parses real mainnet accounts captured 2026-04-15:
//! obligation 3E9N8DZw9v1XqSer76XDnHUC4yRsnh9AveZJx8UXrSxp and its two reserves.

use borrowguard_kamino::{layout::parse_obligation, reserve::parse_reserve, snapshot_from};

const OBLIGATION: &[u8] = include_bytes!("fixtures/obligation_3E9N8DZw.bin");
const COLLATERAL: &[u8] = include_bytes!("fixtures/reserve_2EXNMnkH.bin");
const DEBT: &[u8] = include_bytes!("fixtures/reserve_2gc9Dm1e.bin");

#[test]
fn parses_single_pair_obligation() {
    let obligation = parse_obligation(OBLIGATION).unwrap();
    assert_eq!(obligation.active_deposits().count(), 1);
    assert_eq!(obligation.active_borrows().count(), 1);
    let deposit = obligation.active_deposits().next().unwrap();
    let borrow = obligation.active_borrows().next().unwrap();
    assert!(deposit.deposit_reserve.to_string().starts_with("2EXNMnkH"));
    assert!(borrow.borrow_reserve.to_string().starts_with("2gc9Dm1e"));
    // Cached health from the capture: LTV ≈ 0.539, liquidation at 0.80.
    let deposited = obligation.deposited_value_sf.to_f64();
    let ltv = obligation.borrow_factor_adjusted_debt_value_sf.to_f64() / deposited;
    let liq = obligation.unhealthy_borrow_value_sf.to_f64() / deposited;
    assert!((ltv - 0.539).abs() < 0.001, "ltv {ltv}");
    assert!((liq - 0.800).abs() < 0.001, "liq {liq}");
}

#[test]
fn snapshot_from_real_accounts() {
    let obligation = parse_obligation(OBLIGATION).unwrap();
    let collateral = parse_reserve(COLLATERAL).unwrap();
    let debt = parse_reserve(DEBT).unwrap();
    assert!(collateral.pyth_price.is_some() || collateral.scope_prices.is_some());

    let snap = snapshot_from(&obligation, &collateral, &debt, 0).unwrap();
    assert!(!snap.collateral_symbol.is_empty() && !snap.debt_symbol.is_empty());
    assert!(snap.collateral > 0.0 && snap.debt > 0.0);
    assert!(
        0.0 < snap.max_ltv && snap.max_ltv < snap.liquidation_ltv && snap.liquidation_ltv < 1.0
    );
    let leverage = snap.position().leverage(snap.price()).unwrap();
    assert!(leverage > 1.0 && leverage < snap.liquidation_leverage(), "leverage {leverage}");
}

#[test]
fn reserve_execution_fields_are_sane() {
    for data in [COLLATERAL, DEBT] {
        let r = parse_reserve(data).unwrap();
        // SPL Token or Token-2022; the executor must use whichever the reserve says.
        let program = r.token_program.to_string();
        assert!(
            [
                "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
                "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"
            ]
            .contains(&program.as_str()),
            "{} token program {program}",
            r.name
        );
        for key in [r.supply_vault, r.fee_vault, r.collateral_mint, r.collateral_supply_vault] {
            assert_ne!(key, Default::default());
        }
        // Each cToken is worth slightly more than one unit of liquidity as
        // interest accrues; it never drops below 1.
        let rate = r.total_liquidity as f64 / r.collateral_mint_supply as f64;
        assert!((1.0..2.0).contains(&rate), "{} rate {rate}", r.name);
        assert!((0.0..0.01).contains(&r.flash_loan_fee), "{} fee {}", r.name, r.flash_loan_fee);
    }
}
