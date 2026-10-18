//! Fluid `TickMath` (`get_ratio_at_tick`, `get_tick_at_ratio`) and the
//! position debt formulas, ported from fluid-solana-programs@626b177
//! (`crates/library/src/math/tick.rs`, `programs/vaults/src/state/position.rs`,
//! `programs/vaults/src/utils/operate.rs`).
//!
//! A position's tick encodes its debt/collateral ratio: `ratio = 1.0015^tick`,
//! stored as `ratio × 2^48`.

use anyhow::{ensure, Context, Result};

pub const MIN_TICK: i32 = -16_383;
pub const MAX_TICK: i32 = 16_383;
const SHIFT: u32 = 48;
const ZERO_TICK_SCALED_RATIO: u128 = 1 << SHIFT;
const MIN_RATIO_X48: u128 = 6_093;
const MAX_RATIO_X48: u128 = 13_002_088_133_096_036_565_414_295;
const E13: u128 = 10_000_000_000_000;
/// `(2^k, 1.0015^(2^k) × 1e13)`, largest first: the binary search in
/// `get_tick_at_ratio`.
const TICK_STEPS: [(i32, u128); 14] = [
    (0x2000, 2_150_859_953_785_115_391),
    (0x1000, 4_637_736_467_054_931),
    (0x800, 215_354_044_936_586),
    (0x400, 46_406_254_420_777),
    (0x200, 21_542_110_950_596),
    (0x100, 14_677_230_989_051),
    (0x80, 12_114_962_232_319),
    (0x40, 11_006_798_913_544),
    (0x20, 10_491_329_235_871),
    (0x10, 10_242_718_992_470),
    (0x8, 10_120_631_893_548),
    (0x4, 10_060_135_135_051),
    (0x2, 10_030_022_500_000),
    (0x1, 10_015_000_000_000),
];

const FACTOR00: u128 = 0x10000000000000000; // 2^64
/// `FACTORS[k] = 2^64 / 1.0015^(2^k)`, applied for each set bit k of |tick|.
const FACTORS: [u128; 14] = [
    0xff9dd7de423466c2,
    0xff3bd55f4488ad27,
    0xfe78410fd6498b74,
    0xfcf2d9987c9be179,
    0xf9ef02c4529258b0,
    0xf402d288133a85a1,
    0xe895615b5beb6386,
    0xd34f17a00ffa00a8,
    0xae6b7961714e2055,
    0x76d6461f27082d75,
    0x372a3bfe0745d8b7,
    0xbe32cbee4897976,
    0x8d4f70c9ff4925,
    0x4e009ae55194,
];

/// `1.0015^tick × 2^48`, bit-exact with the on-chain library.
pub fn ratio_at_tick(tick: i32) -> Result<u128> {
    ensure!((MIN_TICK..=MAX_TICK).contains(&tick), "tick {tick} out of bounds");
    let abs_tick = tick.unsigned_abs();
    let mut factor = FACTOR00;
    for (bit, &f) in FACTORS.iter().enumerate() {
        if abs_tick & (1 << bit) != 0 {
            // Bit 0 replaces the identity; every factor is ≤ 2^64, so the
            // product fits in u128 (on-chain uses a 256-bit multiply).
            factor = if bit == 0 { f } else { factor.checked_mul(f).context("tick math")? >> 64 };
        }
    }
    let mut precision = 0;
    if tick >= 0 {
        factor = u128::MAX / factor;
        if factor % 0x10000 != 0 {
            precision = 1;
        }
    }
    Ok((factor >> 16) + precision)
}

/// `a × b / c`, floored. Every product here fits in u128 (on-chain uses a
/// 256-bit intermediate, with the same result).
fn mul_div(a: u128, b: u128, c: u128) -> Result<u128> {
    Ok(a.checked_mul(b).context("tick math overflow")? / c)
}

/// The tick whose ratio is the largest one ≤ `ratio_x48`, and that ratio, as
/// the on-chain library computes them.
pub fn tick_at_ratio(ratio_x48: u128) -> Result<(i32, u128)> {
    ensure!(
        (MIN_RATIO_X48..=MAX_RATIO_X48).contains(&ratio_x48),
        "ratio {ratio_x48} out of bounds"
    );
    let negative = ratio_x48 < ZERO_TICK_SCALED_RATIO;
    let mut factor = if negative {
        mul_div(ZERO_TICK_SCALED_RATIO, E13, ratio_x48)?
    } else {
        mul_div(ratio_x48, E13, ZERO_TICK_SCALED_RATIO)?
    };
    let mut tick = 0i32;
    for (bit, step) in TICK_STEPS {
        if factor >= step {
            tick |= bit;
            factor = mul_div(factor, E13, step)?;
        }
    }
    let perfect = if negative {
        tick = !tick;
        mul_div(ratio_x48, factor, 10_015_000_000_000)?
    } else {
        mul_div(ratio_x48, E13, factor)?
    };
    ensure!(perfect <= ratio_x48, "tick math: ratio above input");
    Ok((tick, perfect))
}

/// Debt as the vault stores it after an operate: scaled up by a billionth,
/// plus one (`OperateMemoryVars::get_scaled_debt`).
pub fn scaled_debt(net_debt_raw: u128) -> u128 {
    net_debt_raw * (1_000_000_000 + 1) / 1_000_000_000 + 1
}

/// The tick an operate leaves a position in, given its collateral and its
/// scaled net debt: one above the tick at its ratio (`add_debt_to_tick`).
pub fn position_tick(col_raw: u128, scaled_debt: u128) -> Result<i32> {
    ensure!(col_raw > 0, "position has no collateral");
    let ratio = mul_div(scaled_debt, ZERO_TICK_SCALED_RATIO, col_raw)?;
    Ok(tick_at_ratio(ratio)?.0 + 1)
}

/// Raw debt for `col_raw` collateral at `tick`, rounded up like the vault
/// program does. Subtract the position's dust debt to get its net debt.
pub fn debt_raw(col_raw: u64, tick: i32) -> Result<u128> {
    let debt = ratio_at_tick(tick)?
        .checked_mul(col_raw as u128 + 1)
        .context("position debt overflows u128")?;
    Ok((debt >> SHIFT) + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_zero_is_one() {
        assert_eq!(ratio_at_tick(0).unwrap(), 1 << 48);
    }

    #[test]
    fn matches_on_chain_integer_chain() {
        // Golden values from a big-int replay of the library algorithm. At
        // extreme ticks the factor chain drifts ~2e-9 from 1.0015^tick (the
        // library's MAX_RATIOX48 constant is the ideal value, not the chain's).
        for (tick, ratio) in [
            (MIN_TICK, 6_093),
            (-1, 281_053_396_615_732),
            (1, 281_897_189_175_722),
            (16_000, 7_323_148_922_621_280_266_932_496),
            (MAX_TICK, 13_002_088_162_051_014_298_412_982),
        ] {
            assert_eq!(ratio_at_tick(tick).unwrap(), ratio, "tick {tick}");
        }
    }

    #[test]
    fn matches_closed_form() {
        // Away from the extremes the integer ratio is large enough to compare tightly.
        for tick in [-5_000, -462, -1, 1, 77, 3_000] {
            let exact = ratio_at_tick(tick).unwrap() as f64 / (1u128 << 48) as f64;
            let expected = 1.0015f64.powi(tick);
            assert!((exact / expected - 1.0).abs() < 1e-9, "tick {tick}: {exact} vs {expected}");
        }
    }

    #[test]
    fn rejects_out_of_range() {
        assert!(ratio_at_tick(MAX_TICK + 1).is_err());
        assert!(ratio_at_tick(MIN_TICK - 1).is_err());
    }

    #[test]
    fn tick_at_ratio_finds_the_tick_below() {
        // The library's two directions round differently right at a tick's
        // ratio, so check ratios strictly inside a tick: they belong to it.
        for tick in [-9_000, -462, -1, 0, 1, 77, 3_000, 12_000] {
            let (lo, hi) = (ratio_at_tick(tick).unwrap(), ratio_at_tick(tick + 1).unwrap());
            let (found, perfect) = tick_at_ratio(lo + (hi - lo) / 2).unwrap();
            assert_eq!(found, tick, "inside tick {tick}");
            assert!(
                perfect.abs_diff(lo) <= lo / 1_000_000_000,
                "tick {tick} ratio {perfect} vs {lo}"
            );
        }
        assert!(tick_at_ratio(MIN_RATIO_X48 - 1).is_err());
        assert!(tick_at_ratio(MAX_RATIO_X48 + 1).is_err());
    }

    #[test]
    fn positions_land_one_tick_above_their_ratio() {
        // 70 debt on 100 collateral: ratio 0.7 sits in tick -238 (1.0015^-238 ≈ 0.700),
        // so the vault books it in tick -237.
        let col = 100_000_000_000u128;
        let tick = position_tick(col, scaled_debt(70_000_000_000)).unwrap();
        let ratio = |t: i32| ratio_at_tick(t).unwrap() as f64 / (1u128 << 48) as f64;
        assert!(ratio(tick - 1) <= 0.7 && 0.7 < ratio(tick), "tick {tick}");
        assert_eq!(scaled_debt(1_000_000_000), 1_000_000_002);
    }

    #[test]
    fn reproduces_real_positions_ticks() {
        // (col_raw, tick, dust_debt_raw) of mainnet positions in vaults 1 and 3,
        // 2026-10-19. Rebuild the scaled debt each one was booked with
        // (`add_debt_to_tick`: debt at the top of the tick, minus dust) and
        // check the vault's math puts it back in the same tick.
        for (col, tick, dust) in [
            (235_136_343_356_753u128, 2_435, 9_658_501_450_449u128), // Fjfma5X1
            (8_058_817_698, 2_254, 259_871_919),                     // 7Y4So3XA
            (20_296, 2_631, 144),                                    // CM9XnzoS
            (20_345_480_082_210, 2_786, 1_426_320_493_080),          // 6VdCBiBz
            (5_623_896_764, 2_785, 16_681_255),                      // Cpgdqbad
            (145_312, 2_707, 2_547),                                 // FCiZWwMY
        ] {
            let (lo, hi) = (ratio_at_tick(tick - 1).unwrap(), ratio_at_tick(tick).unwrap());
            let (_, perfect) = tick_at_ratio(lo + (hi - lo) / 2).unwrap();
            let booked = (((perfect * 10_015 / 10_000) * col) >> SHIFT) - dust;
            assert_eq!(position_tick(col, booked).unwrap(), tick, "col {col}");
        }
    }

    #[test]
    fn debt_rounds_up() {
        // Ratio 1.0 at tick 0: debt = (col + 1) + 1.
        assert_eq!(debt_raw(1_000, 0).unwrap(), 1_002);
    }
}
