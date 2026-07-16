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
use crate::archer::types::{MakerBook, MakerRegistry, MarketStateHeader, MAKER_KIND_LO, MAKER_KIND_MM};
use clap::Parser;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, read_keypair_file};
use solana_sdk::signer::Signer;
use tokio_util::sync::CancellationToken;

use crate::config::{Cli, MarketsCommand, load_config, load_markets_context, resolve_path};
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
        Cli::Markets { cmd } => cmd_markets(cmd).await,
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

async fn cmd_markets(cmd: MarketsCommand) -> Result<()> {
    match cmd {
        MarketsCommand::List { config, all } => cmd_markets_list(&config, all).await,
        MarketsCommand::View { config, market } => cmd_markets_view(&config, market).await,
    }
}

fn market_status_str(status: u8) -> &'static str {
    match status {
        0 => "Active",
        1 => "Paused",
        2 => "Closed",
        _ => "Unknown",
    }
}

#[derive(Clone, Copy)]
enum Align {
    Left,
    Right,
}

fn print_table(headers: &[&str], aligns: &[Align], rows: &[Vec<String>]) {
    let n = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }

    let rule = |left: &str, mid: &str, right: &str| {
        let mut s = String::from(left);
        for (i, w) in widths.iter().enumerate() {
            s.push_str(&"─".repeat(w + 2));
            s.push_str(if i + 1 == n { right } else { mid });
        }
        s
    };

    let print_row = |cells: &[String]| {
        let mut s = String::from("│");
        for (i, cell) in cells.iter().enumerate() {
            let pad = widths[i] - cell.chars().count();
            match aligns[i] {
                Align::Left => s.push_str(&format!(" {}{} ", cell, " ".repeat(pad))),
                Align::Right => s.push_str(&format!(" {}{} ", " ".repeat(pad), cell)),
            }
            s.push('│');
        }
        println!("{s}");
    };

    let print_spacer = || {
        let mut s = String::from("│");
        for w in &widths {
            s.push_str(&" ".repeat(w + 2));
            s.push('│');
        }
        println!("{s}");
    };

    println!("{}", rule("┌", "┬", "┐"));
    print_row(&headers.iter().map(|h| h.to_string()).collect::<Vec<_>>());
    println!("{}", rule("├", "┼", "┤"));
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            print_spacer();
        }
        print_row(row);
    }
    println!("{}", rule("└", "┴", "┘"));
}

async fn cmd_markets_list(config_path: &std::path::Path, all: bool) -> Result<()> {
    let ctx = load_markets_context(config_path)?;
    let client = ArcherClient::new(&ctx.rpc_url);
    let mut markets = client.get_all_markets().await?;

    if !all {
        markets.retain(|(_, h)| h.status == 0);
    }

    if markets.is_empty() {
        if all {
            println!("No markets found on the Archer program.");
        } else {
            println!("No active markets found (use `--all` to include paused/closed).");
        }
        return Ok(());
    }

    markets.sort_by(|(a_pk, a), (b_pk, b)| {
        a.status
            .cmp(&b.status)
            .then_with(|| a_pk.to_string().cmp(&b_pk.to_string()))
    });

    let mut mints: Vec<Pubkey> = markets
        .iter()
        .flat_map(|(_, h)| [h.base_mint, h.quote_mint])
        .collect();
    mints.sort();
    mints.dedup();
    let symbols = client.get_token_symbols(&mints).await;
    let sym = |mint: &Pubkey| symbols.get(mint).map(String::as_str).unwrap_or("?");

    let rows: Vec<Vec<String>> = markets
        .iter()
        .enumerate()
        .map(|(i, (pubkey, h))| {
            vec![
                (i + 1).to_string(),
                market_status_str(h.status).to_string(),
                sym(&h.base_mint).to_string(),
                sym(&h.quote_mint).to_string(),
                format!("{:.2}", h.maker_fee_ppm as f64 / 100.0),
                format!("{:.2}", h.taker_fee_ppm as f64 / 100.0),
                pubkey.to_string(),
                h.base_mint.to_string(),
                h.quote_mint.to_string(),
            ]
        })
        .collect();

    println!("Found {} market(s):\n", markets.len());
    print_table(
        &[
            "#", "Status", "Base", "Quote", "MkrBps", "TkrBps", "Market", "Base Token",
            "Quote Token",
        ],
        &[
            Align::Right, // #
            Align::Left,  // Status
            Align::Left,  // Base
            Align::Left,  // Quote
            Align::Right, // MkrBps
            Align::Right, // TkrBps
            Align::Left,  // Market
            Align::Left,  // Base Token
            Align::Left,  // Quote Token
        ],
        &rows,
    );
    println!("\nRun `markets view --market <pubkey>` for full details.");
    Ok(())
}

async fn cmd_markets_view(config_path: &std::path::Path, market: Option<String>) -> Result<()> {
    let ctx = load_markets_context(config_path)?;
    let market_str = market
        .or(ctx.default_market)
        .context("no market given: pass --market <pubkey> or set market_pubkey in config")?;
    let market: Pubkey = market_str.parse().context("Invalid market pubkey")?;

    let client = ArcherClient::new(&ctx.rpc_url);
    let cfg = client
        .get_market_config(&market)
        .await
        .context("Failed to fetch market — is the pubkey a valid Archer market?")?;

    let rpc = RpcClient::new(ctx.rpc_url.clone());
    let account = rpc.get_account(&market).await.context("Failed to fetch market account")?;
    let h: MarketStateHeader = *MarketStateHeader::load(&account.data)?;

    let symbols = client.get_token_symbols(&[cfg.base_mint, cfg.quote_mint]).await;
    let sym = |mint: &Pubkey| symbols.get(mint).map(String::as_str).unwrap_or("?");

    println!("=== Market {market} ===");
    println!("Status:       {}", market_status_str(h.status));
    println!("Pair:         {} / {}", sym(&cfg.base_mint), sym(&cfg.quote_mint));
    println!("Base mint:    {} ({}, {} decimals)", cfg.base_mint, sym(&cfg.base_mint), cfg.base_decimals);
    println!("Quote mint:   {} ({}, {} decimals)", cfg.quote_mint, sym(&cfg.quote_mint), cfg.quote_decimals);
    println!("Base vault:   {}", cfg.base_vault);
    println!("Quote vault:  {}", cfg.quote_vault);
    println!("Tick size:    {} quote atoms/base unit", cfg.tick_size_in_quote_atoms_per_base_unit);
    println!("Base lot:     {} atoms", cfg.base_atoms_per_base_lot);
    println!("Quote lot:    {} atoms", cfg.quote_atoms_per_quote_lot);
    println!("Maker fee:    {} ppm ({:.2} bps)", h.maker_fee_ppm, h.maker_fee_ppm as f64 / 100.0);
    println!("Taker fee:    {} ppm ({:.2} bps)", h.taker_fee_ppm, h.taker_fee_ppm as f64 / 100.0);

    let books = client.get_maker_books_for_market(&market).await?;
    let active = books.iter().filter(|b| b.status == 1).count();

    let mut best_bid: Option<f64> = None;
    let mut best_ask: Option<f64> = None;
    let factor = cfg.ticks_to_price_factor();
    for book in books.iter().filter(|b| b.status == 1) {
        let mid = book.mid_price_ticks as i64;
        for lvl in book.bid_levels.iter().filter(|l| l.size_in_base_lots > 0) {
            let ticks = mid + lvl.price_offset_ticks;
            if ticks > 0 {
                let price = ticks as f64 * factor;
                best_bid = Some(best_bid.map_or(price, |b| b.max(price)));
            }
        }
        for lvl in book.ask_levels.iter().filter(|l| l.size_in_base_lots > 0) {
            let ticks = mid + lvl.price_offset_ticks;
            if ticks > 0 {
                let price = ticks as f64 * factor;
                best_ask = Some(best_ask.map_or(price, |a| a.min(price)));
            }
        }
    }

    println!("\n--- Liquidity ---");
    println!("Maker books:  {} ({} active)", books.len(), active);
    match (best_bid, best_ask) {
        (Some(bid), Some(ask)) => {
            let spread_bps = if ask > 0.0 { (ask - bid) / ask * 10_000.0 } else { 0.0 };
            println!("Best bid:     {bid:.6}");
            println!("Best ask:     {ask:.6}");
            println!("Spread:       {spread_bps:.2} bps");
        }
        (Some(bid), None) => println!("Best bid:     {bid:.6}  (no asks)"),
        (None, Some(ask)) => println!("Best ask:     {ask:.6}  (no bids)"),
        (None, None) => println!("No live quotes on this market."),
    }
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
