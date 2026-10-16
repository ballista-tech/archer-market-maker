use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use anyhow::Result;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::RpcSendTransactionConfig;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::hash::Hash;
use solana_sdk::instruction::Instruction;
use solana_sdk::signature::{Keypair, Signature};
use solana_sdk::signer::Signer;
use solana_sdk::transaction::Transaction;
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
    shadow_mode: bool,
    state: Arc<SharedState>,
    blockhash_cache: Arc<RwLock<Option<CachedBlockhash>>>,
}

impl TxSender {
    pub fn new(
        rpc: Arc<RpcClient>,
        signer: Arc<Keypair>,
        priority_fee: u64,
        shadow_mode: bool,
        state: Arc<SharedState>,
    ) -> Self {
        Self {
            rpc,
            signer,
            priority_fee,
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
        let state = self.state.clone();
        let cache = self.blockhash_cache.clone();

        tokio::spawn(async move {
            match build_and_send(&rpc, &signer, instructions, fee, cu_limit, &cache).await {
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

async fn build_and_send(
    rpc: &RpcClient,
    signer: &Keypair,
    mut instructions: Vec<Instruction>,
    priority_fee: u64,
    cu_limit: u32,
    cache: &RwLock<Option<CachedBlockhash>>,
) -> Result<Signature> {
    let mut all_ixs = Vec::with_capacity(instructions.len() + 2);
    all_ixs.push(ComputeBudgetInstruction::set_compute_unit_limit(cu_limit));
    all_ixs.push(ComputeBudgetInstruction::set_compute_unit_price(priority_fee));
    all_ixs.append(&mut instructions);

    let blockhash = get_or_refresh_blockhash(rpc, cache).await?;
    let tx = Transaction::new_signed_with_payer(&all_ixs, Some(&signer.pubkey()), &[signer], blockhash);

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
