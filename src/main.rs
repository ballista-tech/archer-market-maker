mod archer;
mod config;
mod engine;
mod feed;
mod fills;
mod state;
mod strategy;
mod tx;
mod volatility;

use std::sync::Arc;

use anyhow::{Context, Result};
use crate::archer::accounts::{maker_balances, parse_market_state, active_bid_levels, active_ask_levels};
use crate::archer::ix_builder::{
    build_clear_book_ix, build_deposit_ix, build_initialize_maker_book_ix,
    build_set_book_delegate_ix, build_update_expiry_in_slots_ix, build_withdraw_ix,
};
use crate::archer::client::{ArcherClient, SendOptions};
use crate::archer::types::{MakerBook, MakerRegistry, MAKER_KIND_LO, MAKER_KIND_MM};
use clap::Parser;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, read_keypair_file};
use solana_sdk::signer::Signer;
use tokio_util::sync::CancellationToken;

use crate::config::{Cli, load_config, resolve_path};
use crate::state::SharedState;
use crate::tx::TxSender;

async fn detect_token_program(rpc: &RpcClient, mint: &Pubkey) -> Result<Pubkey> {
    let account = rpc
        .get_account(mint)
        .await
        .with_context(|| format!("Failed to fetch mint account {mint}"))?;
    if account.owner == spl_token::id() {
        Ok(spl_token::id())
    } else if account.owner == spl_token_2022::id() {
        Ok(spl_token_2022::id())
    } else {
        anyhow::bail!("Mint {mint} owned by unknown program {}", account.owner)
    }
}

struct TokenPrograms { base: Pubkey, quote: Pubkey }

async fn resolve_token_programs(rpc: &RpcClient, base_mint: &Pubkey, quote_mint: &Pubkey) -> Result<TokenPrograms> {
    Ok(TokenPrograms {
        base: detect_token_program(rpc, base_mint).await?,
        quote: detect_token_program(rpc, quote_mint).await?,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli {
        Cli::Run { config, shadow } => cmd_run(&config, shadow).await,
        Cli::Init { config, kind } => cmd_init(&config, &kind).await,
        Cli::Deposit { config, base, quote } => cmd_deposit(&config, base, quote).await,
        Cli::Withdraw { config } => cmd_withdraw(&config).await,
        Cli::Kill { config } => cmd_kill(&config).await,
        Cli::Status { config } => cmd_status(&config).await,
        Cli::SetExpiry { config, slots } => cmd_set_expiry(&config, slots).await,
        Cli::SetDelegate { config, delegate } => cmd_set_delegate(&config, delegate).await,
    }
}

fn parse_book_kind(kind: &str) -> Result<u8> {
    match kind.to_lowercase().as_str() {
        "mm" | "maker" => Ok(MAKER_KIND_MM),
        "lo" | "limit" | "limit-order" => Ok(MAKER_KIND_LO),
        other => anyhow::bail!("invalid book kind '{other}' (expected 'mm' or 'lo')"),
    }
}

/// Fetch the market's maker registry and report whether `maker_book` is listed.
/// Returns `None` when no registry account exists for the market.
async fn check_registry(rpc: &RpcClient, market: &Pubkey, maker_book: &Pubkey) -> Option<bool> {
    let (registry_pda, _) = MakerRegistry::get_address(market);
    let account = rpc.get_account(&registry_pda).await.ok()?;
    let registry = MakerRegistry::load(&account.data).ok()?;
    Some(registry.contains(maker_book))
}

async fn cmd_run(config_path: &std::path::Path, shadow: bool) -> Result<()> {
    let mut mm_config = load_config(config_path)?;
    if shadow { mm_config.execution.shadow_mode = true; }
    let mm_config = Arc::new(mm_config);

    init_tracing(&mm_config.monitoring.log_level);
    tracing::info!("Archer Market Maker starting");
    if mm_config.execution.shadow_mode {
        tracing::warn!("SHADOW MODE — no transactions will be sent");
    }

    let (signer_keypair, maker_pubkey) = resolve_run_identity(&mm_config.market)?;
    let signer_pubkey = signer_keypair.pubkey();
    let signer = Arc::new(signer_keypair);
    let market_pubkey: Pubkey = mm_config.market.market_pubkey.parse().context("Invalid market_pubkey")?;

    if maker_pubkey != signer_pubkey {
        tracing::info!(%market_pubkey, %maker_pubkey, delegate = %signer_pubkey, "Running as delegate");
    } else {
        tracing::info!(%market_pubkey, %maker_pubkey);
    }

    let rpc = Arc::new(solana_client::nonblocking::rpc_client::RpcClient::new_with_commitment(
        mm_config.connection.rpc_url.clone(),
        solana_sdk::commitment_config::CommitmentConfig::processed(),
    ));

    let archer_client = ArcherClient::new(&mm_config.connection.rpc_url);
    let sdk_config = Arc::new(
        archer_client.get_market_config(&market_pubkey).await.context("Failed to fetch MarketConfig")?,
    );
    tracing::info!(base_mint = %sdk_config.base_mint, quote_mint = %sdk_config.quote_mint, "MarketConfig loaded");

    let initial_book = archer_client
        .get_maker_book(&market_pubkey, &maker_pubkey)
        .await
        .context("Failed to fetch maker book — run `init` first.")?;

    let bal = maker_balances(&initial_book, &sdk_config);
    tracing::info!(base_free = bal.base_free, quote_free = bal.quote_free, "Initial balances");

    let is_lo = initial_book.is_lo();
    tracing::info!(book_kind = initial_book.kind_str(), "Maker book loaded");

    // Registry awareness: if the market has a registry and our book isn't in it,
    // the aggregator may never route flow to us.
    let (maker_book_pda, _) = MakerBook::get_address(&market_pubkey, &maker_pubkey);
    match check_registry(&rpc, &market_pubkey, &maker_book_pda).await {
        Some(true) => tracing::info!("Maker book is registered in the market registry"),
        Some(false) => tracing::warn!(
            %maker_book_pda,
            "Maker book is NOT registered — the admin must run RegisterMaker or the aggregator may skip your quotes"
        ),
        None => tracing::debug!("No maker registry for this market (registration not required)"),
    }

    let state = Arc::new(SharedState::new());
    state.cached_mid_ticks.store(initial_book.mid_price_ticks, std::sync::atomic::Ordering::Relaxed);
    state.onchain_sequence_number.store(initial_book.last_updated_sequence_number, std::sync::atomic::Ordering::Relaxed);
    state.base_total_lots.store(initial_book.base_free + initial_book.base_locked, std::sync::atomic::Ordering::Relaxed);
    state.quote_total_lots.store(initial_book.quote_free + initial_book.quote_locked, std::sync::atomic::Ordering::Relaxed);

    let tx_sender = Arc::new(TxSender::new(
        rpc.clone(), signer.clone(),
        mm_config.execution.priority_fee_microlamports,
        mm_config.execution.shadow_mode, state.clone(),
    ));

    let cancel = CancellationToken::new();

    tokio::spawn(feed::run_feed(
        state.clone(), mm_config.feed.clone(), mm_config.strategy.vol_window, cancel.clone(),
    ));

    // Live fill + inventory subscriptions over the RPC websocket.
    let ws_url = if mm_config.connection.ws_url.is_empty() {
        fills::ws_url_from_rpc(&mm_config.connection.rpc_url)
    } else {
        mm_config.connection.ws_url.clone()
    };
    tokio::spawn(fills::run_fills(
        state.clone(), sdk_config.clone(), ws_url, maker_book_pda, cancel.clone(),
    ));

    tracing::info!("Waiting for price feed...");
    let mut waited = 0u64;
    while state.mid_price.load(std::sync::atomic::Ordering::Relaxed) <= 0.0 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        waited += 100;
        if waited > 15_000 { anyhow::bail!("Price feed did not connect within 15 seconds"); }
    }
    tracing::info!(price = state.mid_price.load(std::sync::atomic::Ordering::Relaxed), "Price feed connected");

    let engine_handle = tokio::spawn(engine::run_engine(
        state.clone(), sdk_config.clone(), mm_config.clone(), signer.clone(),
        maker_pubkey, market_pubkey, tx_sender.clone(),
        initial_book.last_updated_sequence_number, is_lo, cancel.clone(),
    ));

    tracing::info!("Engine running. Press Ctrl+C to stop.");
    tokio::signal::ctrl_c().await?;
    tracing::info!("Shutting down");
    cancel.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), engine_handle).await;
    tracing::info!("Stopped");
    Ok(())
}

async fn cmd_init(config_path: &std::path::Path, kind: &str) -> Result<()> {
    let mm_config = load_config(config_path)?;
    init_tracing(&mm_config.monitoring.log_level);
    let kind_byte = parse_book_kind(kind)?;
    let keypair = load_keypair(&mm_config.market.maker_keypair_path)?;
    let market: Pubkey = mm_config.market.market_pubkey.parse()?;
    let client = ArcherClient::new(&mm_config.connection.rpc_url);
    let ix = build_initialize_maker_book_ix(&keypair.pubkey(), &market, kind_byte);
    let sig = client.send_instructions(&[ix], &[&keypair], SendOptions::default()).await?;
    let kind_label = if kind_byte == MAKER_KIND_LO { "LO (limit-order)" } else { "MM (market-maker)" };
    println!("Maker book initialized [{kind_label}]: {sig}");
    Ok(())
}

async fn cmd_set_delegate(config_path: &std::path::Path, delegate: Option<String>) -> Result<()> {
    let mm_config = load_config(config_path)?;
    init_tracing(&mm_config.monitoring.log_level);
    let keypair = load_keypair(&mm_config.market.maker_keypair_path)?;
    let market: Pubkey = mm_config.market.market_pubkey.parse()?;
    let client = ArcherClient::new(&mm_config.connection.rpc_url);

    // Pubkey::default() (all zeros) clears the delegate on-chain.
    let clear = matches!(
        delegate.as_deref().map(str::to_lowercase).as_deref(),
        None | Some("clear") | Some("none") | Some("")
    );
    let delegate_pubkey = if clear {
        Pubkey::default()
    } else {
        delegate.as_deref().unwrap().parse().context("Invalid delegate pubkey")?
    };

    let ix = build_set_book_delegate_ix(&keypair.pubkey(), &market, &delegate_pubkey);
    let sig = client.send_instructions(&[ix], &[&keypair], SendOptions::default()).await?;
    if clear {
        println!("Delegate cleared: {sig}");
    } else {
        println!("Delegate set to {delegate_pubkey}: {sig}");
    }
    Ok(())
}

async fn cmd_set_expiry(config_path: &std::path::Path, slots: u64) -> Result<()> {
    let mm_config = load_config(config_path)?;
    init_tracing(&mm_config.monitoring.log_level);
    let keypair = load_keypair(&mm_config.market.maker_keypair_path)?;
    let market: Pubkey = mm_config.market.market_pubkey.parse()?;
    let client = ArcherClient::new(&mm_config.connection.rpc_url);
    let ix = build_update_expiry_in_slots_ix(&keypair.pubkey(), &market, slots);
    let sig = client.send_instructions(&[ix], &[&keypair], SendOptions::default()).await?;
    if slots == 0 {
        println!("expiry_in_slots set to 0 (disabled): {sig}");
    } else {
        println!("expiry_in_slots set to {slots}: {sig}");
    }
    Ok(())
}

async fn cmd_deposit(config_path: &std::path::Path, base: f64, quote: f64) -> Result<()> {
    let mm_config = load_config(config_path)?;
    init_tracing(&mm_config.monitoring.log_level);
    let keypair = load_keypair(&mm_config.market.maker_keypair_path)?;
    let market: Pubkey = mm_config.market.market_pubkey.parse()?;
    let client = ArcherClient::new(&mm_config.connection.rpc_url);
    let sdk_config = client.get_market_config(&market).await?;
    let rpc = RpcClient::new(mm_config.connection.rpc_url.clone());
    let programs = resolve_token_programs(&rpc, &sdk_config.base_mint, &sdk_config.quote_mint).await?;
    let maker_base_ata = spl_associated_token_account::get_associated_token_address_with_program_id(
        &keypair.pubkey(), &sdk_config.base_mint, &programs.base,
    );
    let maker_quote_ata = spl_associated_token_account::get_associated_token_address_with_program_id(
        &keypair.pubkey(), &sdk_config.quote_mint, &programs.quote,
    );
    let ix = build_deposit_ix(
        &keypair.pubkey(), &market, base, quote,
        &maker_base_ata, &maker_quote_ata, &programs.base, &programs.quote, &sdk_config,
    )?;
    let sig = client.send_instructions(&[ix], &[&keypair], SendOptions::default()).await?;
    println!("Deposited {base} base + {quote} quote: {sig}");
    Ok(())
}

async fn cmd_withdraw(config_path: &std::path::Path) -> Result<()> {
    let mm_config = load_config(config_path)?;
    init_tracing(&mm_config.monitoring.log_level);
    let keypair = load_keypair(&mm_config.market.maker_keypair_path)?;
    let market: Pubkey = mm_config.market.market_pubkey.parse()?;
    let client = ArcherClient::new(&mm_config.connection.rpc_url);
    let sdk_config = client.get_market_config(&market).await?;
    let rpc = RpcClient::new(mm_config.connection.rpc_url.clone());
    let programs = resolve_token_programs(&rpc, &sdk_config.base_mint, &sdk_config.quote_mint).await?;
    let (maker_book_pda, _) = MakerBook::get_address(&market, &keypair.pubkey());
    let account = rpc.get_account(&maker_book_pda).await.context("MakerBook not found")?;
    let book = MakerBook::load(&account.data)?;

    let total_base = book.base_free + book.base_locked;
    let total_quote = book.quote_free + book.quote_locked;
    if total_base == 0 && total_quote == 0 {
        println!("Nothing to withdraw.");
        return Ok(());
    }
    println!("  Base:  {} free, {} locked", book.base_free, book.base_locked);
    println!("  Quote: {} free, {} locked", book.quote_free, book.quote_locked);

    let mut ixs = Vec::new();
    if book.base_locked > 0 || book.quote_locked > 0 {
        println!("  Locked funds detected — prepending ClearBook");
        ixs.push(build_clear_book_ix(&keypair.pubkey(), &market, &keypair.pubkey(), book.last_updated_sequence_number + 1));
    }

    let maker_base_ata = spl_associated_token_account::get_associated_token_address_with_program_id(
        &keypair.pubkey(), &sdk_config.base_mint, &programs.base,
    );
    let maker_quote_ata = spl_associated_token_account::get_associated_token_address_with_program_id(
        &keypair.pubkey(), &sdk_config.quote_mint, &programs.quote,
    );

    let wb = if book.base_locked > 0 { total_base } else { book.base_free };
    let wq = if book.quote_locked > 0 { total_quote } else { book.quote_free };
    let wb_ui = (wb as f64) * (sdk_config.base_atoms_per_base_lot as f64) / 10f64.powi(sdk_config.base_decimals as i32);
    let wq_ui = (wq as f64) * (sdk_config.quote_atoms_per_quote_lot as f64) / 10f64.powi(sdk_config.quote_decimals as i32);

    if wb > 0 || wq > 0 {
        ixs.push(build_withdraw_ix(
            &keypair.pubkey(), &market, wb_ui, wq_ui,
            &maker_base_ata, &maker_quote_ata, &programs.base, &programs.quote, &sdk_config,
        )?);
    }
    let sig = client.send_instructions(&ixs, &[&keypair], SendOptions::default()).await?;
    println!("Withdrawn: {sig}");
    Ok(())
}

async fn cmd_kill(config_path: &std::path::Path) -> Result<()> {
    let mm_config = load_config(config_path)?;
    init_tracing(&mm_config.monitoring.log_level);
    let keypair = load_keypair(&mm_config.market.maker_keypair_path)?;
    let market: Pubkey = mm_config.market.market_pubkey.parse()?;
    let client = ArcherClient::new(&mm_config.connection.rpc_url);
    let book = client.get_maker_book(&market, &keypair.pubkey()).await?;
    let ix = build_clear_book_ix(&keypair.pubkey(), &market, &keypair.pubkey(), book.last_updated_sequence_number + 1);
    let sig = client.send_instructions(&[ix], &[&keypair], SendOptions::default().with_priority_fee(500_000)).await?;
    println!("Book cleared: {sig}");
    Ok(())
}

async fn cmd_status(config_path: &std::path::Path) -> Result<()> {
    let mm_config = load_config(config_path)?;
    let market: Pubkey = mm_config.market.market_pubkey.parse()?;
    let keypair = load_keypair(&mm_config.market.maker_keypair_path)?;
    let client = ArcherClient::new(&mm_config.connection.rpc_url);
    let sdk_config = client.get_market_config(&market).await?;
    let book = client.get_maker_book(&market, &keypair.pubkey()).await?;
    let bal = maker_balances(&book, &sdk_config);
    let rpc = RpcClient::new(mm_config.connection.rpc_url.clone());
    let market_account = rpc.get_account(&market).await?;
    let header = parse_market_state(&market_account.data)?;
    let mode = match header.mode { 0 => "Continuous", 1 => "Asynchronous", 2 => "Hybrid", _ => "Unknown" };

    let (maker_book_pda, _) = MakerBook::get_address(&market, &keypair.pubkey());
    let registered = match check_registry(&rpc, &market, &maker_book_pda).await {
        Some(true) => "yes",
        Some(false) => "NO (book not in registry)",
        None => "n/a (no registry)",
    };
    let delegate = if book.delegate == Pubkey::default() {
        "none".to_string()
    } else {
        book.delegate.to_string()
    };
    let status = match book.status { 1 => "Active", 2 => "Suspended", _ => "Unknown" };

    println!("=== Archer Market Maker Status ===");
    println!("Market:       {market}");
    println!("Maker:        {}", keypair.pubkey());
    println!("Book PDA:     {maker_book_pda}");
    println!("Mode:         {mode}");
    println!("Book kind:    {}", book.kind_str());
    println!("Book status:  {status}");
    println!("Registered:   {registered}");
    println!("Delegate:     {delegate}");
    println!("Sync spread:  {} ticks", book.sync_spread_ticks);
    println!("Expiry slots: {}", book.expiry_in_slots);
    println!("Mid ticks:    {}", book.mid_price_ticks);
    println!("Bid levels:   {}", active_bid_levels(&book));
    println!("Ask levels:   {}", active_ask_levels(&book));
    println!("Base free:    {:.6}", bal.base_free);
    println!("Base locked:  {:.6}", bal.base_locked);
    println!("Quote free:   {:.4}", bal.quote_free);
    println!("Quote locked: {:.4}", bal.quote_locked);
    Ok(())
}

fn resolve_run_identity(m: &crate::config::MarketSettings) -> Result<(Keypair, Pubkey)> {
    let owner_keypair = if m.maker_keypair_path.is_empty() {
        None
    } else {
        Some(load_keypair(&m.maker_keypair_path)?)
    };

    let owner_pubkey = match (&owner_keypair, m.maker_owner_pubkey.is_empty()) {
        (Some(kp), _) => kp.pubkey(),
        (None, false) => m.maker_owner_pubkey.parse().context("Invalid maker_owner_pubkey")?,
        (None, true) => anyhow::bail!("set maker_keypair_path or maker_owner_pubkey"),
    };

    let signer = if m.delegate_keypair_path.is_empty() {
        owner_keypair
            .ok_or_else(|| anyhow::anyhow!("no signer: set maker_keypair_path or delegate_keypair_path"))?
    } else {
        load_keypair(&m.delegate_keypair_path)?
    };

    Ok((signer, owner_pubkey))
}

fn load_keypair(path: &str) -> Result<Keypair> {
    let resolved = resolve_path(path);
    read_keypair_file(&resolved)
        .map_err(|e| anyhow::anyhow!("Failed to load keypair from {}: {e}", resolved.display()))
}

fn init_tracing(level: &str) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("archer_market_maker={level},warn")));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_timer(tracing_subscriber::fmt::time::uptime())
        .compact()
        .init();
}
