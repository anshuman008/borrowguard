//! Compose, simulate and send v0 transactions.

use std::{
    thread,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use solana_client::{rpc_client::RpcClient, rpc_config::RpcSendTransactionConfig};
use solana_sdk::{
    address_lookup_table::{state::AddressLookupTable, AddressLookupTableAccount},
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    instruction::Instruction,
    message::{v0, VersionedMessage},
    pubkey::Pubkey,
    signature::{Keypair, Signature},
    signer::Signer,
    transaction::VersionedTransaction,
};

/// [`compose`] prepends this many compute-budget instructions. Instruction
/// indexes that point inside a transaction (e.g. a flash-loan repay naming its
/// borrow) must add it.
pub const COMPUTE_BUDGET_IXS: usize = 2;

/// Solana's hard cap on a serialized transaction.
const MAX_TX_BYTES: usize = 1232;

pub struct Simulated {
    pub units: u64,
    pub logs: Vec<String>,
}

pub fn load_lookup_tables(
    rpc: &RpcClient,
    keys: &[Pubkey],
) -> Result<Vec<AddressLookupTableAccount>> {
    let mut unique = keys.to_vec();
    unique.sort();
    unique.dedup();
    let accounts = rpc.get_multiple_accounts(&unique)?;
    unique
        .iter()
        .zip(accounts)
        .map(|(key, account)| {
            let account = account.with_context(|| format!("lookup table {key} not found"))?;
            let table = AddressLookupTable::deserialize(&account.data)
                .map_err(|e| anyhow::anyhow!("lookup table {key}: {e}"))?;
            Ok(AddressLookupTableAccount { key: *key, addresses: table.addresses.to_vec() })
        })
        .collect()
}

/// Sign `ixs` (preceded by compute-budget instructions) as one v0 transaction.
pub fn compose(
    rpc: &RpcClient,
    signer: &Keypair,
    ixs: &[Instruction],
    lookup_tables: &[AddressLookupTableAccount],
    compute_units: u32,
    micro_lamports_per_cu: u64,
) -> Result<VersionedTransaction> {
    let mut all = Vec::with_capacity(ixs.len() + COMPUTE_BUDGET_IXS);
    all.push(ComputeBudgetInstruction::set_compute_unit_limit(compute_units));
    all.push(ComputeBudgetInstruction::set_compute_unit_price(micro_lamports_per_cu));
    all.extend_from_slice(ixs);
    let blockhash = rpc.get_latest_blockhash()?;
    let message = v0::Message::try_compile(&signer.pubkey(), &all, lookup_tables, blockhash)?;
    let tx = VersionedTransaction::try_new(VersionedMessage::V0(message), &[signer])?;
    let size = bincode_size(&tx)?;
    if size > MAX_TX_BYTES {
        bail!(
            "transaction is {size} bytes, over Solana's {MAX_TX_BYTES}-byte limit; \
             add an address lookup table for the position's accounts"
        );
    }
    Ok(tx)
}

fn bincode_size(tx: &VersionedTransaction) -> Result<usize> {
    Ok(bincode::serialize(tx).context("serialize transaction")?.len())
}

/// Create an address lookup table owned by `ctx.signer` holding `addresses`,
/// and wait until it can be used. Returns its address.
pub fn create_lookup_table(
    rpc: &RpcClient,
    signer: &Keypair,
    addresses: &[Pubkey],
    micro_lamports_per_cu: u64,
) -> Result<Pubkey> {
    use solana_sdk::address_lookup_table::instruction::{create_lookup_table, extend_lookup_table};
    let me = signer.pubkey();
    let slot = rpc.get_slot()?;
    let (create, table) = create_lookup_table(me, me, slot);
    // ~20 addresses per extend keeps each transaction well under the size cap.
    let mut batches = addresses.chunks(20);
    let first = batches.next().unwrap_or(&[]).to_vec();
    let mut ixs = vec![create];
    ixs.push(extend_lookup_table(table, me, Some(me), first));
    for ixs in std::iter::once(ixs)
        .chain(batches.map(|chunk| vec![extend_lookup_table(table, me, Some(me), chunk.to_vec())]))
    {
        let tx = compose(rpc, signer, &ixs, &[], 200_000, micro_lamports_per_cu)?;
        simulate(rpc, &tx)?;
        send_and_confirm(rpc, &tx)?;
    }
    // Addresses become usable one slot after the extend lands.
    let ready = rpc.get_slot()? + 2;
    while rpc.get_slot()? < ready {
        thread::sleep(Duration::from_millis(400));
    }
    Ok(table)
}

/// Run the transaction against current state with real signature checks.
pub fn simulate(rpc: &RpcClient, tx: &VersionedTransaction) -> Result<Simulated> {
    let sim = rpc.simulate_transaction(tx)?.value;
    let logs = sim.logs.unwrap_or_default();
    if let Some(err) = sim.err {
        let tail: Vec<_> = logs.iter().rev().take(25).rev().cloned().collect();
        bail!("simulation failed: {err}\n  {}", tail.join("\n  "));
    }
    Ok(Simulated { units: sim.units_consumed.unwrap_or(0), logs })
}

/// Send, rebroadcasting until confirmed or the blockhash expires (~60s).
pub fn send_and_confirm(rpc: &RpcClient, tx: &VersionedTransaction) -> Result<Signature> {
    let config = RpcSendTransactionConfig {
        skip_preflight: true, // already simulated
        max_retries: Some(0),
        ..Default::default()
    };
    let signature = tx.signatures[0];
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(60) {
        rpc.send_transaction_with_config(tx, config)?;
        thread::sleep(Duration::from_secs(2));
        let status = rpc.get_signature_statuses(&[signature])?.value[0].clone();
        if let Some(status) = status {
            if let Some(err) = status.err {
                bail!("transaction {signature} failed on-chain: {err}");
            }
            if status.satisfies_commitment(CommitmentConfig::confirmed()) {
                return Ok(signature);
            }
        }
    }
    bail!("transaction {signature} not confirmed within 60s (it may still land; check before retrying)")
}
