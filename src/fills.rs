use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures_util::StreamExt;
use solana_account_decoder::{UiAccountData, UiAccountEncoding};
use solana_client::nonblocking::pubsub_client::PubsubClient;
use solana_client::rpc_config::{
    RpcAccountInfoConfig, RpcTransactionLogsConfig, RpcTransactionLogsFilter,
};
use solana_client::rpc_response::RpcLogsResponse;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use tokio_util::sync::CancellationToken;

use crate::archer::config::MarketConfig;
use crate::archer::math::{base_lots_to_amount, quote_lots_to_amount};
use crate::archer::types::MakerBook;
use crate::state::SharedState;

/// sha256("event:MakerFillEvent")[..8] — the first 8 bytes of the matching
/// `Program data:` log line. Must stay in sync with `MAKER_FILL_DISC` in the
/// on-chain program (`program/src/events.rs`).
const MAKER_FILL_DISC: [u8; 8] = [60, 14, 66, 1, 204, 202, 42, 161];

/// Serialized body length: u8 + u8 + u64 + i64 + u64 + u64 + i64 + u64 = 50.
const MAKER_FILL_BODY_LEN: usize = 50;

/// A decoded `MakerFillEvent` from the program's log stream.
#[derive(Debug, Clone, Copy)]
pub struct MakerFillEvent {
    /// Index of the filled book within the swap's maker-book account list.
    /// Cannot be mapped back to a pubkey from logs alone — see `handle_logs`.
    pub maker_index: u8,
    /// 0 = bid fill (maker buys base), 1 = ask fill (maker sells base).
    pub side: u8,
    pub absolute_price_ticks: u64,
    pub price_offset_ticks: i64,
    pub base_lots_filled: u64,
    pub quote_lots_filled: u64,
    pub maker_fee: i64,
    pub sequence_number: u64,
}

impl MakerFillEvent {
    /// Decode a `disc || borsh(body)` blob. Returns `None` unless the
    /// discriminator matches and the body is fully present.
    fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < 8 + MAKER_FILL_BODY_LEN || buf[..8] != MAKER_FILL_DISC {
            return None;
        }
        let b = &buf[8..];
        Some(Self {
            maker_index: b[0],
            side: b[1],
            absolute_price_ticks: u64::from_le_bytes(b[2..10].try_into().ok()?),
            price_offset_ticks: i64::from_le_bytes(b[10..18].try_into().ok()?),
            base_lots_filled: u64::from_le_bytes(b[18..26].try_into().ok()?),
            quote_lots_filled: u64::from_le_bytes(b[26..34].try_into().ok()?),
            maker_fee: i64::from_le_bytes(b[34..42].try_into().ok()?),
            sequence_number: u64::from_le_bytes(b[42..50].try_into().ok()?),
        })
    }
}

/// Derive the websocket endpoint from an HTTP(S) RPC URL.
pub fn ws_url_from_rpc(rpc_url: &str) -> String {
    if let Some(rest) = rpc_url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = rpc_url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        rpc_url.to_string()
    }
}

/// Run the fills + book subscriptions, reconnecting with backoff until cancelled.
pub async fn run_fills(
    state: Arc<SharedState>,
    sdk_config: Arc<MarketConfig>,
    ws_url: String,
    maker_book_pda: Pubkey,
    cancel: CancellationToken,
) {
    let mut backoff_ms = 200u64;
    loop {
        if cancel.is_cancelled() {
            return;
        }
        match run_once(&state, &sdk_config, &ws_url, &maker_book_pda, &cancel).await {
            Ok(()) => return, // cancelled cleanly
            Err(e) => {
                tracing::warn!("fills subscription dropped: {e:#}; reconnecting in {backoff_ms}ms");
            }
        }
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(Duration::from_millis(backoff_ms)) => {}
        }
        backoff_ms = (backoff_ms * 2).min(5_000);
    }
}

async fn run_once(
    state: &SharedState,
    sdk_config: &MarketConfig,
    ws_url: &str,
    maker_book_pda: &Pubkey,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    let client = PubsubClient::new(ws_url).await?;

    let (mut logs_stream, _logs_unsub) = client
        .logs_subscribe(
            RpcTransactionLogsFilter::Mentions(vec![maker_book_pda.to_string()]),
            RpcTransactionLogsConfig {
                commitment: Some(CommitmentConfig::confirmed()),
            },
        )
        .await?;

    let (mut acct_stream, _acct_unsub) = client
        .account_subscribe(
            maker_book_pda,
            Some(RpcAccountInfoConfig {
                encoding: Some(UiAccountEncoding::Base64),
                commitment: Some(CommitmentConfig::confirmed()),
                data_slice: None,
                min_context_slot: None,
            }),
        )
        .await?;

    tracing::info!(%maker_book_pda, "Fill + book subscriptions active");

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            msg = logs_stream.next() => match msg {
                Some(resp) => handle_logs(state, sdk_config, &resp.value),
                None => anyhow::bail!("logs stream closed"),
            },
            msg = acct_stream.next() => match msg {
                Some(resp) => handle_account(state, &resp.value.data),
                None => anyhow::bail!("account stream closed"),
            },
        }
    }
}

fn handle_logs(state: &SharedState, sdk_config: &MarketConfig, resp: &RpcLogsResponse) {
    // Failed transactions never settle fills.
    if resp.err.is_some() {
        return;
    }
    for line in &resp.logs {
        let Some(b64) = line.strip_prefix("Program data: ") else {
            continue;
        };
        let Ok(bytes) = BASE64.decode(b64.trim()) else {
            continue;
        };
        if let Some(ev) = MakerFillEvent::decode(&bytes) {
            record_fill(state, sdk_config, &ev, &resp.signature);
        }
    }
}

/// Note: the `mentions` filter guarantees the transaction touched our book, but
/// a single swap can fill several makers' books. `maker_index` can't be mapped
/// to a pubkey from logs alone, so in a shared swap these counters may include
/// sibling makers' fills. Inventory itself is always exact — it comes from the
/// `account_subscribe` stream below, not from these events.
fn record_fill(
    state: &SharedState,
    sdk_config: &MarketConfig,
    ev: &MakerFillEvent,
    signature: &str,
) {
    state.fills_count.fetch_add(1, Relaxed);
    state.fill_base_lots.fetch_add(ev.base_lots_filled, Relaxed);
    state.fill_quote_lots.fetch_add(ev.quote_lots_filled, Relaxed);

    let side = if ev.side == 0 { "BID/buy" } else { "ASK/sell" };
    let base = base_lots_to_amount(ev.base_lots_filled, sdk_config);
    let quote = quote_lots_to_amount(ev.quote_lots_filled, sdk_config);
    let price = ev.absolute_price_ticks as f64 * sdk_config.ticks_to_price_factor();
    tracing::info!(
        side,
        base,
        quote,
        price,
        offset_ticks = ev.price_offset_ticks,
        maker_index = ev.maker_index,
        maker_fee = ev.maker_fee,
        seq = ev.sequence_number,
        sig = signature,
        "Fill"
    );
}

fn handle_account(state: &SharedState, data: &UiAccountData) {
    let bytes = match data {
        UiAccountData::Binary(b64, _) => BASE64.decode(b64).ok(),
        UiAccountData::LegacyBinary(b64) => BASE64.decode(b64).ok(),
        UiAccountData::Json(_) => None,
    };
    let Some(bytes) = bytes else {
        return;
    };
    match MakerBook::load(&bytes) {
        Ok(book) => {
            state
                .base_total_lots
                .store(book.base_free + book.base_locked, Relaxed);
            state
                .quote_total_lots
                .store(book.quote_free + book.quote_locked, Relaxed);
            state.cached_mid_ticks.store(book.mid_price_ticks, Relaxed);
            // Only ever move the sequence forward; out-of-order ws frames are possible.
            state
                .onchain_sequence_number
                .fetch_max(book.last_updated_sequence_number, Relaxed);
            state.book_resyncs.fetch_add(1, Relaxed);
        }
        Err(e) => tracing::warn!("book account decode failed: {e}"),
    }
}
