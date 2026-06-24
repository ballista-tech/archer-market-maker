use std::sync::atomic::{AtomicBool, AtomicU64};
use portable_atomic::AtomicF64;
use tokio::sync::Notify;

pub struct SharedState {
    pub mid_price: AtomicF64,
    pub price_timestamp_us: AtomicU64,
    pub feed_alive: AtomicBool,

    /// Feed signals the engine whenever a new price arrives.
    pub price_notify: Notify,

    pub cached_mid_ticks: AtomicU64,
    pub base_total_lots: AtomicU64,
    pub quote_total_lots: AtomicU64,
    pub onchain_sequence_number: AtomicU64,

    pub volatility_bps: AtomicF64,

    pub consecutive_failures: AtomicU64,

    pub cycles_total: AtomicU64,
    pub updates_sent: AtomicU64,
    pub mid_only_updates: AtomicU64,
    pub book_updates: AtomicU64,
    pub clear_book_sends: AtomicU64,
    pub heartbeat_sends: AtomicU64,

    /// MakerFillEvent stream metrics (populated by the fills subscriber).
    pub fills_count: AtomicU64,
    pub fill_base_lots: AtomicU64,
    pub fill_quote_lots: AtomicU64,
    /// Number of times inventory was refreshed from an on-chain book account update.
    pub book_resyncs: AtomicU64,

    pub engine_alive: AtomicBool,
}

impl SharedState {
    pub fn new() -> Self {
        Self {
            mid_price: AtomicF64::new(0.0),
            price_timestamp_us: AtomicU64::new(0),
            feed_alive: AtomicBool::new(false),
            price_notify: Notify::new(),
            cached_mid_ticks: AtomicU64::new(0),
            base_total_lots: AtomicU64::new(0),
            quote_total_lots: AtomicU64::new(0),
            onchain_sequence_number: AtomicU64::new(0),
            volatility_bps: AtomicF64::new(0.0),
            consecutive_failures: AtomicU64::new(0),
            cycles_total: AtomicU64::new(0),
            updates_sent: AtomicU64::new(0),
            mid_only_updates: AtomicU64::new(0),
            book_updates: AtomicU64::new(0),
            clear_book_sends: AtomicU64::new(0),
            heartbeat_sends: AtomicU64::new(0),
            fills_count: AtomicU64::new(0),
            fill_base_lots: AtomicU64::new(0),
            fill_quote_lots: AtomicU64::new(0),
            book_resyncs: AtomicU64::new(0),
            engine_alive: AtomicBool::new(false),
        }
    }
}

pub fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}
