use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::time::sleep;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;

use crate::config::FeedSettings;
use crate::state::{SharedState, now_us};
use crate::volatility::VolatilityTracker;

#[derive(Debug, Deserialize)]
struct BinanceBookTicker {
    s: String,
    b: String,
    a: String,
}

fn parse_binance_book_ticker(txt: &str) -> Option<(String, f64, f64)> {
    let bt: BinanceBookTicker = serde_json::from_str(txt).ok()?;
    let bid: f64 = bt.b.parse().ok()?;
    let ask: f64 = bt.a.parse().ok()?;
    if bid > 0.0 && ask > 0.0 && ask >= bid {
        Some((bt.s, bid, ask))
    } else {
        None
    }
}

fn handle_tick(
    state: &SharedState,
    vol_tracker: &mut VolatilityTracker,
    bid: f64,
    ask: f64,
) {
    let mid = (bid + ask) * 0.5;
    state.mid_price.store(mid, Relaxed);
    state.price_timestamp_us.store(now_us(), Relaxed);
    vol_tracker.push(mid);
    state.volatility_bps.store(vol_tracker.realized_vol_bps(), Relaxed);
}

pub async fn run_feed(
    state: Arc<SharedState>,
    config: FeedSettings,
    vol_window: usize,
    cancel: CancellationToken,
) {
    let primary = config.binance_symbol.to_uppercase();
    let cross = config.cross_symbol.to_uppercase();
    let use_cross = !cross.is_empty();

    let primary_stream = format!("{}@bookTicker", primary.to_lowercase());
    let mut streams: Vec<String> = vec![primary_stream];
    if use_cross {
        streams.push(format!("{}@bookTicker", cross.to_lowercase()));
    }

    let subscribe_msg = serde_json::json!({
        "method": "SUBSCRIBE",
        "params": streams,
        "id": 1
    })
    .to_string();

    let mut backoff_ms: u64 = 100;
    let mut vol_tracker = VolatilityTracker::new(vol_window);

    let mut primary_bid: f64 = 0.0;
    let mut primary_ask: f64 = 0.0;
    let mut cross_bid: f64 = 0.0;
    let mut cross_ask: f64 = 0.0;

    loop {
        if cancel.is_cancelled() {
            return;
        }

        let url = &config.binance_ws_url;
        tracing::info!(%url, ?streams, "Connecting to Binance");

        match connect_async(url).await {
            Ok((ws_stream, _)) => {
                backoff_ms = 100;
                state.feed_alive.store(true, Relaxed);

                let (mut write, mut read) = ws_stream.split();

                if let Err(e) = write.send(Message::Text(subscribe_msg.clone())).await {
                    tracing::warn!("Subscribe send failed: {e}");
                    state.feed_alive.store(false, Relaxed);
                    continue;
                }

                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        msg = read.next() => {
                            match msg {
                                Some(Ok(Message::Text(txt))) => {
                                    if let Some(bt) = parse_binance_book_ticker(&txt) {
                                        let sym = bt.0.to_uppercase();
                                        if sym == primary {
                                            primary_bid = bt.1;
                                            primary_ask = bt.2;
                                        } else if use_cross && sym == cross {
                                            cross_bid = bt.1;
                                            cross_ask = bt.2;
                                        } else {
                                            continue;
                                        }

                                        if primary_bid <= 0.0 || primary_ask <= 0.0 {
                                            continue;
                                        }
                                        if use_cross && (cross_bid <= 0.0 || cross_ask <= 0.0) {
                                            continue;
                                        }

                                        let (bid, ask) = if use_cross {
                                            let cross_mid = (cross_bid + cross_ask) * 0.5;
                                            (primary_bid / cross_mid, primary_ask / cross_mid)
                                        } else {
                                            (primary_bid, primary_ask)
                                        };

                                        handle_tick(&state, &mut vol_tracker, bid, ask);
                                        state.price_notify.notify_one();

                                        if !state.feed_alive.load(Relaxed) {
                                            state.feed_alive.store(true, Relaxed);
                                        }
                                    }
                                }
                                Some(Ok(Message::Ping(data))) => {
                                    let _ = write.send(Message::Pong(data)).await;
                                }
                                Some(Ok(Message::Close(_))) | None => {
                                    tracing::warn!("Binance WS closed");
                                    break;
                                }
                                Some(Err(e)) => {
                                    tracing::warn!("Binance WS error: {e}");
                                    break;
                                }
                                _ => {}
                            }
                        }
                    }
                }

                state.feed_alive.store(false, Relaxed);
            }
            Err(e) => {
                tracing::warn!("Binance connect failed: {e}");
                state.feed_alive.store(false, Relaxed);
            }
        }

        tracing::info!(backoff_ms, "Reconnecting in {backoff_ms}ms");
        sleep(Duration::from_millis(backoff_ms)).await;
        backoff_ms = (backoff_ms * 2).min(5000);
    }
}
