use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use anyhow::Result;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::RpcSendTransactionConfig;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::hash::Hash;
use solana_sdk::instruction::Instruction;
use solana_sdk::message::{VersionedMessage, v1};
use solana_sdk::signature::{Keypair, Signature};
use solana_sdk::signer::Signer;
use solana_sdk::transaction::VersionedTransaction;
use tokio::sync::RwLock;

use crate::state::SharedState;

#[derive(Debug, Copy, Clone)]
pub enum TxPriority {
    Normal,
    Emergency,
}

const BLOCKHASH_TTL: Duration = Duration::from_secs(2);

struct CachedBlockhash {
    hash: Hash,
    fetched_at: Instant,
}

pub struct TxSender {
    rpc: Arc<RpcClient>,
    signer: Arc<Keypair>,
    priority_fee: u64,
    data_size_limit: u32,
    shadow_mode: bool,
    state: Arc<SharedState>,
    blockhash_cache: Arc<RwLock<Option<CachedBlockhash>>>,
}

impl TxSender {
    pub fn new(
        rpc: Arc<RpcClient>,
        signer: Arc<Keypair>,
        priority_fee: u64,
        data_size_limit: u32,
        shadow_mode: bool,
        state: Arc<SharedState>,
    ) -> Self {
        Self {
            rpc,
            signer,
            priority_fee,
            data_size_limit,
            shadow_mode,
            state,
            blockhash_cache: Arc::new(RwLock::new(None)),
        }
    }

    pub async fn send_now(
        &self,
        instructions: Vec<Instruction>,
        priority: TxPriority,
        cu_limit: u32,
    ) -> Result<Signature> {
        if self.shadow_mode {
            tracing::debug!(ix_count = instructions.len(), cu = cu_limit, "SHADOW: would send TX");
            return Ok(Signature::default());
        }
        let fee = match priority {
            TxPriority::Normal => self.priority_fee,
            TxPriority::Emergency => self.priority_fee.saturating_mul(10),
        };
        let result = build_and_send(
            &self.rpc,
            &self.signer,
            instructions,
            fee,
            cu_limit,
            self.data_size_limit,
            &self.blockhash_cache,
        )
        .await;
        match &result {
            Ok(_) => self.state.consecutive_failures.store(0, Relaxed),
            Err(_) => {
                self.state.consecutive_failures.fetch_add(1, Relaxed);
            }
        }
        result
    }

    pub fn fire(&self, instructions: Vec<Instruction>, priority: TxPriority, cu_limit: u32) {
        if self.shadow_mode {
            tracing::debug!(ix_count = instructions.len(), cu = cu_limit, "SHADOW: would send TX");
            return;
        }

        let rpc = self.rpc.clone();
        let signer = self.signer.clone();
        let fee = match priority {
            TxPriority::Normal => self.priority_fee,
            TxPriority::Emergency => self.priority_fee.saturating_mul(10),
        };
        let data_size_limit = self.data_size_limit;
        let state = self.state.clone();
        let cache = self.blockhash_cache.clone();

        tokio::spawn(async move {
            match build_and_send(&rpc, &signer, instructions, fee, cu_limit, data_size_limit, &cache).await {
                Ok(sig) => {
                    tracing::debug!(%sig, "TX sent");
                    state.consecutive_failures.store(0, Relaxed);
                }
                Err(e) => {
                    tracing::warn!("TX send failed: {e:#}");
                    state.consecutive_failures.fetch_add(1, Relaxed);
                }
            }
        });
    }
}

async fn get_or_refresh_blockhash(
    rpc: &RpcClient,
    cache: &RwLock<Option<CachedBlockhash>>,
) -> Result<Hash> {
    {
        let guard = cache.read().await;
        if let Some(ref cached) = *guard {
            if cached.fetched_at.elapsed() < BLOCKHASH_TTL {
                return Ok(cached.hash);
            }
        }
    }
    let mut guard = cache.write().await;
    if let Some(ref cached) = *guard {
        if cached.fetched_at.elapsed() < BLOCKHASH_TTL {
            return Ok(cached.hash);
        }
    }
    let hash = rpc
        .get_latest_blockhash_with_commitment(CommitmentConfig::processed())
        .await?
        .0;
    *guard = Some(CachedBlockhash { hash, fetched_at: Instant::now() });
    Ok(hash)
}

/// A v1 header carries the priority fee as a lamport total, where the
/// ComputeBudget instruction took micro-lamports per compute unit. Convert at
/// the requested limit, rounding up so a non-zero rate never collapses to zero
/// on a small transaction.
fn v1_priority_fee_lamports(micro_lamports_per_cu: u64, cu_limit: u32) -> u64 {
    let micro_total = micro_lamports_per_cu as u128 * cu_limit as u128;
    u64::try_from(micro_total.div_ceil(1_000_000)).unwrap_or(u64::MAX)
}

/// Build and sign one quoting transaction in the v1 format (SIMD-0385).
///
/// The compute unit limit, priority fee and loaded-accounts size travel in the
/// message header, so there are no ComputeBudget instructions: `cu_limit` is
/// exactly the program instructions' own budget. A value left out of a v1
/// header means 0, not a default, so all three are always set.
fn build_transaction(
    signer: &Keypair,
    instructions: &[Instruction],
    priority_fee_micro_lamports: u64,
    cu_limit: u32,
    data_size_limit: u32,
    blockhash: Hash,
) -> Result<VersionedTransaction> {
    let config = v1::TransactionConfig::empty()
        .with_compute_unit_limit(cu_limit)
        .with_priority_fee(v1_priority_fee_lamports(priority_fee_micro_lamports, cu_limit))
        .with_loaded_accounts_data_size_limit(data_size_limit);
    let message = v1::Message::try_compile_with_config(&signer.pubkey(), instructions, blockhash, config)
        .map_err(|e| anyhow::anyhow!("v1 message compile failed: {e}"))?;
    VersionedTransaction::try_new(VersionedMessage::V1(message), &[signer])
        .map_err(|e| anyhow::anyhow!("v1 transaction signing failed: {e}"))
}

async fn build_and_send(
    rpc: &RpcClient,
    signer: &Keypair,
    instructions: Vec<Instruction>,
    priority_fee: u64,
    cu_limit: u32,
    data_size_limit: u32,
    cache: &RwLock<Option<CachedBlockhash>>,
) -> Result<Signature> {
    let blockhash = get_or_refresh_blockhash(rpc, cache).await?;
    let tx = build_transaction(signer, &instructions, priority_fee, cu_limit, data_size_limit, blockhash)?;

    // The RPC client encodes with the Agave wire codec, which is what the v1
    // layout needs (it is not the serde/bincode encoding).
    let sig = rpc
        .send_transaction_with_config(
            &tx,
            RpcSendTransactionConfig {
                skip_preflight: true,
                max_retries: Some(0),
                ..Default::default()
            },
        )
        .await?;

    Ok(sig)
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::instruction::AccountMeta;
    use solana_sdk::pubkey::Pubkey;

    #[test]
    fn v1_fee_is_rounded_up_to_whole_lamports() {
        assert_eq!(v1_priority_fee_lamports(0, 850), 0);
        // 100 µlamports/CU × 850 CU = 0.085 lamport → charged as 1.
        assert_eq!(v1_priority_fee_lamports(100, 850), 1);
        assert_eq!(v1_priority_fee_lamports(50_000, 5_600), 280);
        assert_eq!(v1_priority_fee_lamports(1_000_000, 5_600), 5_600);
        assert_eq!(v1_priority_fee_lamports(u64::MAX, u32::MAX), u64::MAX);
    }

    #[test]
    fn quoting_tx_is_v1_with_the_budget_in_the_header() {
        let signer = Keypair::new();
        let ix = Instruction::new_with_bytes(
            Pubkey::new_unique(),
            &[1, 2, 3],
            vec![AccountMeta::new(signer.pubkey(), true)],
        );
        let tx = build_transaction(&signer, &[ix], 100, 850, 400_000, Hash::new_unique()).unwrap();

        let VersionedMessage::V1(message) = &tx.message else {
            panic!("not a v1 message");
        };
        assert_eq!(message.config.compute_unit_limit, Some(850));
        assert_eq!(message.config.priority_fee, Some(1));
        assert_eq!(message.config.loaded_accounts_data_size_limit, Some(400_000));
        assert_eq!(message.instructions.len(), 1, "no ComputeBudget instructions in v1");

        tx.sanitize().unwrap();
        assert!(tx.verify_with_results().into_iter().all(|ok| ok));
    }
}
