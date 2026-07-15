use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use serde::Deserialize;

#[derive(Parser, Debug)]
#[command(
    name = "archer-market-maker",
    about = "A simple market maker for Archer Exchange on Solana"
)]
pub enum Cli {
    /// Start the market maker
    Run {
        #[arg(short, long, default_value = "config/default.toml")]
        config: PathBuf,
        #[arg(long, default_value_t = false)]
        shadow: bool,
    },
    /// Initialize your maker book on-chain (one-time)
    Init {
        #[arg(short, long, default_value = "config/default.toml")]
        config: PathBuf,
        /// Book kind: "mm" (market-maker, default) or "lo" (limit-order).
        /// Init-only and immutable thereafter.
        #[arg(long, default_value = "mm")]
        kind: String,
    },
    /// Deposit tokens into your maker book
    Deposit {
        #[arg(short, long, default_value = "config/default.toml")]
        config: PathBuf,
        #[arg(long)]
        base: f64,
        #[arg(long)]
        quote: f64,
    },
    /// Withdraw all funds from your maker book
    Withdraw {
        #[arg(short, long, default_value = "config/default.toml")]
        config: PathBuf,
    },
    /// Emergency: clear all orders immediately
    Kill {
        #[arg(short, long, default_value = "config/default.toml")]
        config: PathBuf,
    },
    /// Print current on-chain maker book status
    Status {
        #[arg(short, long, default_value = "config/default.toml")]
        config: PathBuf,
    },
    /// Set the maker book's expiry_in_slots (0 disables the aggregator's expiry-skip check)
    SetExpiry {
        #[arg(short, long, default_value = "config/default.toml")]
        config: PathBuf,
        #[arg(long)]
        slots: u64,
    },
    /// Set (or clear) the delegate allowed to manage orders on your behalf
    SetDelegate {
        #[arg(short, long, default_value = "config/default.toml")]
        config: PathBuf,
        /// Delegate pubkey. Omit, or pass "clear"/"none", to remove the delegate.
        #[arg(long)]
        delegate: Option<String>,
    },
}

#[derive(Debug, Deserialize, Clone)]
pub struct MMConfig {
    pub market: MarketSettings,
    pub connection: ConnectionSettings,
    pub feed: FeedSettings,
    pub strategy: StrategySettings,
    pub execution: ExecutionSettings,
    pub monitoring: MonitoringSettings,
}

#[derive(Debug, Deserialize, Clone)]
pub struct MarketSettings {
    pub market_pubkey: String,
    /// Maker (owner) private key. Signs owner-only ops (init/deposit/withdraw/
    /// set-delegate). Optional for `run` if you instead supply
    /// `delegate_keypair_path` + `maker_owner_pubkey`, keeping the owner offline.
    #[serde(default)]
    pub maker_keypair_path: String,
    /// Optional delegate private key. When set, `run` signs with this key
    /// instead of the owner key (see `set-delegate`).
    #[serde(default)]
    pub delegate_keypair_path: String,
    /// Book-owner pubkey, used to derive the MakerBook PDA when
    /// `maker_keypair_path` is empty. Ignored when the owner key is present.
    #[serde(default)]
    pub maker_owner_pubkey: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ConnectionSettings {
    pub rpc_url: String,
    /// Optional websocket endpoint for fill/account subscriptions. When empty,
    /// it is derived from `rpc_url` (https→wss, http→ws).
    #[serde(default)]
    pub ws_url: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct FeedSettings {
    pub binance_symbol: String,
    #[serde(default)]
    pub cross_symbol: String,
    #[serde(default = "default_binance_ws")]
    pub binance_ws_url: String,
    #[serde(default = "default_staleness_ms")]
    pub staleness_timeout_ms: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct StrategySettings {
    pub spread_levels_bps: Vec<f64>,
    #[serde(default = "default_inventory_pct")]
    pub inventory_pct: f64,
    #[serde(default = "default_vol_window")]
    pub vol_window: usize,
    #[serde(default = "default_vol_baseline_bps")]
    pub vol_baseline_bps: f64,
    #[serde(default = "default_vol_max_multiplier")]
    pub vol_max_multiplier: f64,
    #[serde(default = "default_max_price_deviation_pct")]
    pub max_price_deviation_pct: f64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ExecutionSettings {
    #[serde(default = "default_heartbeat_ms")]
    pub heartbeat_interval_ms: u64,
    #[serde(default = "default_priority_fee")]
    pub priority_fee_microlamports: u64,
    #[serde(default)]
    pub shadow_mode: bool,
}

#[derive(Debug, Deserialize, Clone)]
pub struct MonitoringSettings {
    #[serde(default = "default_log_level")]
    pub log_level: String,
}

pub fn load_config(path: &Path) -> Result<MMConfig> {
    let contents =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let config: MMConfig =
        toml::from_str(&contents).with_context(|| format!("parsing {}", path.display()))?;
    validate_config(&config)?;
    Ok(config)
}

fn validate_config(c: &MMConfig) -> Result<()> {
    anyhow::ensure!(!c.market.market_pubkey.is_empty(), "market_pubkey required");
    anyhow::ensure!(
        !c.market.maker_keypair_path.is_empty() || !c.market.maker_owner_pubkey.is_empty(),
        "set maker_keypair_path, or maker_owner_pubkey (+ delegate_keypair_path) for a delegate-only run"
    );
    anyhow::ensure!(!c.connection.rpc_url.is_empty(), "rpc_url required");
    anyhow::ensure!(!c.feed.binance_symbol.is_empty(), "binance_symbol required");
    anyhow::ensure!(!c.strategy.spread_levels_bps.is_empty(), "need at least 1 spread level");
    anyhow::ensure!(c.strategy.spread_levels_bps.len() <= 16, "max 16 levels per side");
    anyhow::ensure!(
        c.strategy.spread_levels_bps.iter().all(|&s| s > 0.0),
        "all spread levels must be positive"
    );
    anyhow::ensure!(
        c.strategy.inventory_pct > 0.0 && c.strategy.inventory_pct <= 100.0,
        "inventory_pct must be between 0 and 100"
    );
    anyhow::ensure!(c.strategy.vol_window >= 2, "vol_window must be >= 2");
    anyhow::ensure!(c.strategy.vol_baseline_bps > 0.0, "vol_baseline_bps must be positive");
    anyhow::ensure!(c.strategy.vol_max_multiplier >= 1.0, "vol_max_multiplier must be >= 1.0");
    anyhow::ensure!(
        c.strategy.max_price_deviation_pct >= 0.0,
        "max_price_deviation_pct must be >= 0 (0 disables the check)"
    );
    Ok(())
}

fn default_binance_ws() -> String { "wss://stream.binance.com:9443/ws".into() }
fn default_staleness_ms() -> u64 { 5000 }
fn default_inventory_pct() -> f64 { 80.0 }
fn default_vol_window() -> usize { 300 }
fn default_vol_baseline_bps() -> f64 { 5.0 }
fn default_vol_max_multiplier() -> f64 { 5.0 }
fn default_max_price_deviation_pct() -> f64 { 5.0 }
fn default_heartbeat_ms() -> u64 { 100 }
fn default_priority_fee() -> u64 { 100 }
fn default_log_level() -> String { "info".into() }

pub fn resolve_path(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(format!("{}/{}", home, rest));
        }
    }
    PathBuf::from(s)
}
