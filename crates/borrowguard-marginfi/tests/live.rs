//! Live mainnet checks, skipped by default. Run with
//! `RPC_URL=<mainnet rpc> cargo test -p borrowguard-marginfi -- --ignored`.
//! The accounts were picked 2026-10-19 for their banks' oracle setups; swap
//! in others if they close.

use borrowguard_marginfi::{MarginfiVenue, Pubkey};

const PYTH_PUSH: &str = "6ojWXoNM7Y5shCavHPMFMDAMMT3eXi66Mf9vgChyEd19";

#[test]
#[ignore = "needs RPC_URL pointing at mainnet"]
fn pulse_prices_every_common_oracle_setup() {
    let rpc = std::env::var("RPC_URL").expect("RPC_URL");
    // Some owners hold no SOL, so borrow a funded fee payer for the simulations.
    let payer = MarginfiVenue::new(&rpc, PYTH_PUSH.parse().unwrap(), None).owner().unwrap();
    // Delisted (reduce-only, fixed-price) banks count for nothing and stay unpriced.
    for (key, banks, all_priced) in [
        (PYTH_PUSH, "Pyth push", true),
        ("4WzDoZwRxFVWpfWE2zcuTUYosnR1a8nwc6Lymu1qTnE3", "Pyth + SPL stake pool", true),
        ("9ZuoqZHSmuGAADccT2cBjfvE9ubcGm7noDFNoW23eW2U", "Pyth + Marinade state", true),
        ("AgJkm2PDV2bY1PgipgK12LRkboMjh5ddYenXd3nZxkip", "9 balances, 3 delisted", false),
    ] {
        let key: Pubkey = key.parse().unwrap();
        let venue = MarginfiVenue::new(&rpc, key, Some(payer));
        let account = venue.account().unwrap();
        let (fresh, _, _) = venue.pulse(&account).unwrap_or_else(|e| panic!("{banks}: {e:#}"));
        let h = &fresh.health;
        assert!(h.engine_ok() && h.oracle_ok(), "{banks}: {h:?}");
        if !all_priced {
            continue;
        }
        for b in fresh.balances.iter().filter(|b| b.asset_shares > 1.0 || b.liability_shares > 1.0)
        {
            assert!(h.prices[b.index] > 0.0, "{banks}: slot {} unpriced", b.index);
        }
    }
}
