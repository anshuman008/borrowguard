//! Turning controller decisions into signed transactions: keypair loading,
//! Jupiter swaps, and composing / simulating / sending.
//!
//! Venues implement [`Execute`]. Every trade is simulated first; nothing is
//! sent unless [`ExecContext::send`] is set.

pub mod jupiter;
pub mod tx;

use std::path::Path;

use anyhow::{ensure, Context, Result};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{commitment_config::CommitmentConfig, pubkey::Pubkey, signature::Keypair};

use crate::jupiter::Jupiter;

pub struct ExecContext {
    pub rpc: RpcClient,
    pub signer: Keypair,
    pub jupiter: Jupiter,
    /// Max swap slippage for routine trades, in basis points.
    pub slippage_bps: u16,
    /// Max swap slippage for ripcord trades, in basis points.
    pub ripcord_slippage_bps: u16,
    /// Priority fee, micro-lamports per compute unit.
    pub priority_fee: u64,
    /// Extra lookup tables (e.g. one holding the position's accounts).
    pub lookup_tables: Vec<Pubkey>,
    /// Off = simulate only.
    pub send: bool,
}

impl ExecContext {
    pub fn rpc(url: &str) -> RpcClient {
        RpcClient::new_with_commitment(url.to_string(), CommitmentConfig::confirmed())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Executed {
    /// Simulated successfully; nothing sent.
    Simulated {
        units: u64,
        summary: String,
    },
    Sent {
        signature: String,
        summary: String,
    },
}

/// A venue that can rebalance a position it watches.
pub trait Execute {
    /// Sell collateral worth `sell` (debt-token units) and repay debt with it,
    /// atomically, funded by a flash loan.
    fn delever(&self, ctx: &ExecContext, sell: f64, ripcord: bool) -> Result<Executed>;

    /// Borrow `borrow` (debt-token units) more and add the collateral it buys,
    /// atomically, funded by a flash loan.
    fn relever(&self, ctx: &ExecContext, borrow: f64) -> Result<Executed>;

    /// Accounts that every rebalance of this position touches; worth putting
    /// in an address lookup table so the swap route has room.
    fn static_accounts(&self, owner: &Pubkey) -> Result<Vec<Pubkey>>;
}

/// Load a Solana CLI keypair file (JSON array of 64 bytes). `~` expands to $HOME.
pub fn load_keypair(path: &Path) -> Result<Keypair> {
    let path = match path.strip_prefix("~") {
        Ok(rest) => Path::new(&std::env::var("HOME").context("HOME not set")?).join(rest),
        Err(_) => path.to_path_buf(),
    };
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("read keypair {}", path.display()))?;
    let bytes: Vec<u8> = serde_json::from_str(&text)
        .with_context(|| format!("{} is not a JSON keypair file", path.display()))?;
    ensure!(bytes.len() == 64, "{} holds {} bytes, expected 64", path.display(), bytes.len());
    Keypair::from_bytes(&bytes).map_err(|e| anyhow::anyhow!("invalid keypair: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::signer::Signer;

    #[test]
    fn loads_solana_cli_keypair() {
        let kp = Keypair::new();
        let path = std::env::temp_dir().join(format!("bg-key-{}.json", std::process::id()));
        std::fs::write(&path, serde_json::to_string(&kp.to_bytes().to_vec()).unwrap()).unwrap();
        assert_eq!(load_keypair(&path).unwrap().pubkey(), kp.pubkey());
        std::fs::write(&path, "[1,2,3]").unwrap();
        assert!(load_keypair(&path).is_err());
        std::fs::remove_file(path).unwrap();
    }
}
