//! Fluid `TickMath::get_ratio_at_tick` and the position debt formula,
//! ported from fluid-solana-programs@626b177 (`crates/library/src/math/tick.rs`,
//! `programs/vaults/src/state/position.rs`).
//!
//! A position's tick encodes its debt/collateral ratio: `ratio = 1.0015^tick`,
//! stored as `ratio × 2^48`.

use anyhow::{ensure, Context, Result};

pub const MIN_TICK: i32 = -16_383;
pub const MAX_TICK: i32 = 16_383;
const SHIFT: u32 = 48;

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
    fn debt_rounds_up() {
        // Ratio 1.0 at tick 0: debt = (col + 1) + 1.
        assert_eq!(debt_raw(1_000, 0).unwrap(), 1_002);
    }
}
