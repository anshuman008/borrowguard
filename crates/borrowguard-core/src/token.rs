//! Display symbols for well-known Solana mints.

const KNOWN: [(&str, &str); 14] = [
    ("So11111111111111111111111111111111111111112", "SOL"),
    ("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", "USDC"),
    ("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB", "USDT"),
    ("J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn", "JITOSOL"),
    ("jupSoLaHXQiZZTSfEWMTRRgpnyFm8f6sZdosWBjx93v", "JUPSOL"),
    ("mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So", "MSOL"),
    ("LSTxxxnJzKDFSLr4dUkPcmCf5VyryEqzPLz5j4bpxFp", "LST"),
    ("JUPyiwrYJFskUPiHa7hkeR8VUtAeFoSYbKedZNsDvCN", "JUP"),
    ("cbbtcf3aa214zXHbiAZQwf4122FBYbraNdFqgw4iMij", "CBBTC"),
    ("3NZ9JMVBmGAqocybic2c7LQCJScmgsAZ6vQqTDzcqmJh", "WBTC"),
    ("27G8MtK7VtTcCHkpASjSDdkWWYfoqT6ggEuKidVJidD4", "JLP"),
    ("2u1tszSeqZ3qBWF3uNGPFc8TzMk2tdiwknnRMWGWjGWH", "USDG"),
    ("2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo", "PYUSD"),
    ("HzwqbKZw8HxMN6bF2yFZNrht3c2iXXzpKcFu7uBEDKtr", "EURC"),
];

/// Symbol for a common mint (base58); otherwise the mint's first characters.
pub fn symbol(mint: &str) -> String {
    KNOWN
        .iter()
        .find(|(m, _)| *m == mint)
        .map_or_else(|| format!("{}…", &mint[..4.min(mint.len())]), |(_, s)| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_and_unknown_mints() {
        assert_eq!(symbol("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"), "USDC");
        assert_eq!(symbol("11111111111111111111111111111111"), "1111…");
    }
}
