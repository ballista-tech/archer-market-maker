use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use archer_sdk::config::MarketConfig;
use solana_sdk::{pubkey::Pubkey, signature::Keypair, signer::Signer};
use tokio_util::sync::CancellationToken;

use crate::archer::quote::{BookAuthority, clear_book_ix, update_instructions, update_mid_price_ix};
use crate::{
    config::MMConfig,
    state::SharedState,
    strategy::{QuoteDecision, Strategy},
    tx::{TxPriority, TxSender},
};

const CU_CLEAR_BOOK: u32 = 650;
const CU_MID_ONLY: u32 = 850;
const CU_FULL_UPDATE: u32 = 5600;

/// How far `local_seq` may run ahead of the book before it is treated as
/// drift from sends that never landed and reset to the on-chain value. Well
/// inside the program's `MAX_SEQUENCE_JUMP` (65_535), past which every write
/// is rejected.
const MAX_LOCAL_SEQ_LEAD: u64 = 4_096;

/// How long a mid update that has not yet shown up on chain is left alone
/// before the same ticks are sent again. Covers confirmation plus the account
/// subscription's delivery; past it the send is assumed dropped.
const MID_RESEND_AFTER: Duration = Duration::from_secs(2);

/// How long shutdown waits for the final ClearBook to be accepted by the RPC.
const SHUTDOWN_CLEAR_TIMEOUT: Duration = Duration::from_secs(8);

pub async fn run_engine(
    state: Arc<SharedState>,
    sdk_config: Arc<MarketConfig>,
    mm_config: Arc<MMConfig>,
    signer: Arc<Keypair>,
    maker_pubkey: Pubkey,
    market_pubkey: Pubkey,
    tx_sender: Arc<TxSender>,
    initial_sequence_number: u64,
    is_lo: bool,
    cancel: CancellationToken,
) {
    let strategy = Strategy::new(&mm_config.strategy, is_lo);
    let heartbeat = Duration::from_millis(mm_config.execution.heartbeat_interval_ms);
    let authority = BookAuthority {
        maker: maker_pubkey,
        signer: signer.pubkey(),
    };
    let staleness_us = mm_config.feed.staleness_timeout_ms * 1000;

    let mut last_structure_hash: u64 = 0;
    let mut last_sent_mid_ticks: u64 = 0;
    let mut last_sent_at_us: u64 = 0;
    let mut needs_initial_book: bool = true;
    let mut local_seq: u64 = initial_sequence_number;

    tracing::info!(
        %market_pubkey, %maker_pubkey,
        heartbeat_ms = mm_config.execution.heartbeat_interval_ms,
        num_levels = mm_config.strategy.spread_levels_bps.len(),
        book_kind = if is_lo { "LO" } else { "MM" },
        "Engine starting (event-driven + heartbeat)"
    );

    state.engine_alive.store(true, Relaxed);

    loop {
        // Wait for either a price update or the heartbeat timeout.
        let is_heartbeat = tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!("Engine shutting down, clearing book");
                state.engine_alive.store(false, Relaxed);
                local_seq += 1;
                let ix = clear_book_ix(&authority, &market_pubkey, local_seq);
                // Awaited, not fired: the process exits as soon as this task
                // returns, which would drop a spawned send mid-flight and
                // leave the quotes resting.
                match tokio::time::timeout(
                    SHUTDOWN_CLEAR_TIMEOUT,
                    tx_sender.send_now(vec![ix], TxPriority::Emergency, CU_CLEAR_BOOK),
                )
                .await
                {
                    Ok(Ok(sig)) => tracing::info!(%sig, "ClearBook sent"),
                    Ok(Err(e)) => tracing::error!("ClearBook send failed: {e:#}"),
                    Err(_) => tracing::error!("ClearBook send timed out; quotes may still rest"),
                }
                return;
            }
            _ = state.price_notify.notified() => false,
            _ = tokio::time::sleep(heartbeat) => true,
        };

        if cancel.is_cancelled() {
            continue; // will hit the cancel branch above
        }

        if state.consecutive_failures.load(Relaxed) >= 10 {
            local_seq += 1;
            let ix = clear_book_ix(&authority, &market_pubkey, local_seq);
            tx_sender.fire(vec![ix], TxPriority::Emergency, CU_CLEAR_BOOK);
            state.clear_book_sends.fetch_add(1, Relaxed);
            needs_initial_book = true;
            last_structure_hash = 0;
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
            continue;
        }

        let price_age_us = crate::state::now_us().saturating_sub(state.price_timestamp_us.load(Relaxed));
        if price_age_us > staleness_us && state.price_timestamp_us.load(Relaxed) > 0 {
            tracing::warn!(age_ms = price_age_us / 1000, "Price feed stale, clearing book");
            local_seq += 1;
            let ix = clear_book_ix(&authority, &market_pubkey, local_seq);
            tx_sender.fire(vec![ix], TxPriority::Emergency, CU_CLEAR_BOOK);
            state.clear_book_sends.fetch_add(1, Relaxed);
            needs_initial_book = true;
            last_structure_hash = 0;
            continue;
        }

        let mid_price = state.mid_price.load(Relaxed);
        if mid_price <= 0.0 || !mid_price.is_finite() {
            state.cycles_total.fetch_add(1, Relaxed);
            continue;
        }

        let onchain_seq = state.onchain_sequence_number.load(Relaxed);
        if onchain_seq > local_seq {
            local_seq = onchain_seq;
        } else if local_seq.saturating_sub(onchain_seq) > MAX_LOCAL_SEQ_LEAD {
            // `local_seq` only ever climbs on sends, and a send that never
            // lands leaves it ahead of the book. Past this lead it is drift;
            // fall back to the on-chain value rather than march toward the
            // program's jump ceiling, after which every write is rejected.
            tracing::warn!(local_seq, onchain_seq, "Local sequence drifted ahead of the book, resyncing");
            local_seq = onchain_seq;
        }

        let cached_mid = state.cached_mid_ticks.load(Relaxed);
        let reference_mid = if cached_mid > 0 { cached_mid } else { last_sent_mid_ticks };
        let effective_hash = if needs_initial_book { 0 } else { last_structure_hash };

        let volatility_bps = state.volatility_bps.load(Relaxed);
        let (decision, _spread_bps) = strategy.compute(
            mid_price,
            reference_mid,
            effective_hash,
            &sdk_config,
            state.base_total_lots.load(Relaxed),
            state.quote_total_lots.load(Relaxed),
            volatility_bps,
        );

        match decision {
            QuoteDecision::Noop => {
                state.cycles_total.fetch_add(1, Relaxed);
                continue;
            }
            QuoteDecision::ClearBook => {
                local_seq += 1;
                let ix = clear_book_ix(&authority, &market_pubkey, local_seq);
                tx_sender.fire(vec![ix], TxPriority::Normal, CU_CLEAR_BOOK);
                state.clear_book_sends.fetch_add(1, Relaxed);
                state.updates_sent.fetch_add(1, Relaxed);
                last_structure_hash = 0;
                needs_initial_book = true;
            }
            QuoteDecision::UpdateMidOnly { new_mid_ticks } => {
                // The on-chain book is the reference. Same ticks: nothing to
                // do, heartbeat or not. Different ticks: the book is wrong and
                // must be repriced, unless exactly these ticks were sent
                // moments ago and have simply not shown up on chain yet.
                if new_mid_ticks == cached_mid {
                    state.cycles_total.fetch_add(1, Relaxed);
                    continue;
                }
                let now_us = crate::state::now_us();
                if new_mid_ticks == last_sent_mid_ticks
                    && now_us.saturating_sub(last_sent_at_us) < MID_RESEND_AFTER.as_micros() as u64
                {
                    state.cycles_total.fetch_add(1, Relaxed);
                    continue;
                }
                local_seq += 1;
                let ix = update_mid_price_ix(&authority, &market_pubkey, new_mid_ticks, local_seq);
                tx_sender.fire(vec![ix], TxPriority::Normal, CU_MID_ONLY);
                state.mid_only_updates.fetch_add(1, Relaxed);
                state.updates_sent.fetch_add(1, Relaxed);
                if is_heartbeat {
                    state.heartbeat_sends.fetch_add(1, Relaxed);
                }
                last_sent_mid_ticks = new_mid_ticks;
                last_sent_at_us = now_us;
            }
            QuoteDecision::UpdateFull { ref book_update, structure_hash } => {
                let (ixs, last_seq) = update_instructions(&authority, &market_pubkey, book_update, local_seq + 1);
                local_seq = last_seq;
                tx_sender.fire(ixs, TxPriority::Normal, CU_FULL_UPDATE);
                state.book_updates.fetch_add(1, Relaxed);
                state.updates_sent.fetch_add(1, Relaxed);
                last_sent_mid_ticks = book_update.new_mid_price_ticks;
                last_sent_at_us = crate::state::now_us();
                last_structure_hash = structure_hash;
                needs_initial_book = false;
            }
        }

        state.cycles_total.fetch_add(1, Relaxed);
    }
}
