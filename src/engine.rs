use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use crate::archer::{
    config::MarketConfig,
    ix_builder::{build_clear_book_ix, build_update_instructions, build_update_mid_price_ix},
};
use solana_sdk::{pubkey::Pubkey, signature::Keypair, signer::Signer};
use tokio_util::sync::CancellationToken;

use crate::{
    config::MMConfig,
    state::SharedState,
    strategy::{QuoteDecision, Strategy},
    tx::{TxPriority, TxSender},
};

const CU_CLEAR_BOOK: u32 = 650;
const CU_MID_ONLY: u32 = 850;
const CU_FULL_UPDATE: u32 = 5600;

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
    let signer_pubkey = signer.pubkey();
    let staleness_us = mm_config.feed.staleness_timeout_ms * 1000;

    let mut last_structure_hash: u64 = 0;
    let mut last_sent_mid_ticks: u64 = 0;
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
                let ix = build_clear_book_ix(&signer_pubkey, &market_pubkey, &maker_pubkey, local_seq);
                tx_sender.fire(vec![ix], TxPriority::Emergency, CU_CLEAR_BOOK);
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
            let ix = build_clear_book_ix(&signer_pubkey, &market_pubkey, &maker_pubkey, local_seq);
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
            let ix = build_clear_book_ix(&signer_pubkey, &market_pubkey, &maker_pubkey, local_seq);
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

        let max_dev_pct = mm_config.strategy.max_price_deviation_pct;
        if max_dev_pct > 0.0 && cached_mid > 0 {
            let candidate_mid_ticks = match &decision {
                QuoteDecision::UpdateMidOnly { new_mid_ticks } => Some(*new_mid_ticks),
                QuoteDecision::UpdateFull { book_update, .. } => Some(book_update.new_mid_price_ticks),
                _ => None,
            };
            if let Some(new_ticks) = candidate_mid_ticks {
                let dev_pct = (new_ticks as f64 - cached_mid as f64).abs() / cached_mid as f64 * 100.0;
                if dev_pct > max_dev_pct {
                    tracing::warn!(
                        new_ticks, onchain_ticks = cached_mid, dev_pct, max_dev_pct,
                        "Mid deviates beyond band — withholding update"
                    );
                    state.cycles_total.fetch_add(1, Relaxed);
                    continue;
                }
            }
        }

        match decision {
            QuoteDecision::Noop => {
                state.cycles_total.fetch_add(1, Relaxed);
                continue;
            }
            QuoteDecision::ClearBook => {
                local_seq += 1;
                let ix = build_clear_book_ix(&signer_pubkey, &market_pubkey, &maker_pubkey, local_seq);
                tx_sender.fire(vec![ix], TxPriority::Normal, CU_CLEAR_BOOK);
                state.clear_book_sends.fetch_add(1, Relaxed);
                state.updates_sent.fetch_add(1, Relaxed);
                last_structure_hash = 0;
                needs_initial_book = true;
            }
            QuoteDecision::UpdateMidOnly { new_mid_ticks } => {
                // Skip if ticks unchanged (price moved but not enough to change tick).
                if new_mid_ticks == last_sent_mid_ticks && !is_heartbeat {
                    state.cycles_total.fetch_add(1, Relaxed);
                    continue;
                }
                local_seq += 1;
                let ix = build_update_mid_price_ix(
                    &signer_pubkey, &market_pubkey, &maker_pubkey, new_mid_ticks, local_seq,
                );
                tx_sender.fire(vec![ix], TxPriority::Normal, CU_MID_ONLY);
                state.mid_only_updates.fetch_add(1, Relaxed);
                state.updates_sent.fetch_add(1, Relaxed);
                if is_heartbeat {
                    state.heartbeat_sends.fetch_add(1, Relaxed);
                }
                last_sent_mid_ticks = new_mid_ticks;
            }
            QuoteDecision::UpdateFull { ref book_update, structure_hash } => {
                local_seq += 1;
                match build_update_instructions(
                    book_update, &market_pubkey, &maker_pubkey, &signer_pubkey, local_seq,
                ) {
                    Ok(ixs) if !ixs.is_empty() => {
                        tx_sender.fire(ixs, TxPriority::Normal, CU_FULL_UPDATE);
                        state.book_updates.fetch_add(1, Relaxed);
                        state.updates_sent.fetch_add(1, Relaxed);
                        last_sent_mid_ticks = book_update.new_mid_price_ticks;
                        last_structure_hash = structure_hash;
                        needs_initial_book = false;
                    }
                    Ok(_) => { local_seq -= 1; }
                    Err(e) => {
                        tracing::warn!("build_update_instructions error: {e}");
                        local_seq -= 1;
                    }
                }
            }
        }

        state.cycles_total.fetch_add(1, Relaxed);
    }
}
