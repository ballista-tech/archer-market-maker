use std::collections::HashMap;

use anyhow::{Context, Result};
use archer_sdk::accounts::{parse_maker_book, parse_market_state};
use archer_sdk::config::MarketConfig;
use archer_sdk::onchain::{
    MAKER_BOOK_DISCRIMINATOR, MARKET_STATE_DISCRIMINATOR, MakerBook, MakerRegistry, MarketStateHeader,
};
use archer_sdk::pda::derive_maker_registry;
use archer_sdk::ARCHER_V1_PROGRAM_ID;
use solana_account_decoder::UiAccountEncoding;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig};
use solana_client::rpc_filter::{Memcmp, RpcFilterType};
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature};
use solana_sdk::signer::Signer;
use solana_sdk::transaction::Transaction;

const METAPLEX_METADATA_PROGRAM: Pubkey =
    solana_sdk::pubkey!("metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s");

const MAX_MULTI_ACCOUNTS: usize = 100;

pub struct ArcherClient {
    sdk: archer_sdk::client::ArcherClient,
}

#[derive(Debug, Clone)]
pub struct SendOptions {
    pub priority_fee_micro_lamports: Option<u64>,
    pub compute_unit_limit: Option<u32>,
    pub max_retries: u32,
}

impl Default for SendOptions {
    fn default() -> Self {
        Self {
            priority_fee_micro_lamports: None,
            compute_unit_limit: None,
            max_retries: 3,
        }
    }
}

impl SendOptions {
    pub fn with_priority_fee(mut self, micro_lamports: u64) -> Self {
        self.priority_fee_micro_lamports = Some(micro_lamports);
        self
    }
}

impl ArcherClient {
    pub fn new(rpc_url: &str) -> Self {
        Self {
            sdk: archer_sdk::client::ArcherClient::new(rpc_url),
        }
    }

    fn rpc(&self) -> &RpcClient {
        self.sdk.rpc()
    }

    /// Market parameters plus the token programs and vaults, cached per market.
    pub async fn get_market_config(&self, market: &Pubkey) -> Result<MarketConfig> {
        self.sdk
            .get_market_config(market)
            .await
            .context("Failed to fetch market config")
    }

    pub async fn get_market_header(&self, market: &Pubkey) -> Result<MarketStateHeader> {
        let account = self
            .rpc()
            .get_account(market)
            .await
            .context("Failed to fetch market account")?;
        Ok(*parse_market_state(&account.data)?)
    }

    pub async fn get_maker_book(&self, market: &Pubkey, maker: &Pubkey) -> Result<MakerBook> {
        self.sdk
            .get_maker_book(market, maker)
            .await
            .context("Failed to fetch maker book account")
    }

    /// The market's registry, or `None` when the market has none.
    pub async fn get_maker_registry(&self, market: &Pubkey) -> Option<MakerRegistry> {
        let (pda, _) = derive_maker_registry(market);
        // A missing account is the common case for unregistered markets, so it
        // is not an error here; any other failure is also reported as "unknown".
        self.rpc().get_account(&pda).await.ok()?;
        self.sdk.get_maker_registry(market).await.ok()
    }

    pub async fn get_all_markets(&self) -> Result<Vec<(Pubkey, MarketStateHeader)>> {
        let config = RpcProgramAccountsConfig {
            filters: Some(vec![RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
                0,
                MARKET_STATE_DISCRIMINATOR.to_vec(),
            ))]),
            account_config: RpcAccountInfoConfig {
                encoding: Some(UiAccountEncoding::Base64),
                commitment: Some(CommitmentConfig::confirmed()),
                ..Default::default()
            },
            ..Default::default()
        };

        let accounts = self
            .rpc()
            .get_program_accounts_with_config(&ARCHER_V1_PROGRAM_ID, config)
            .await
            .context("Failed to fetch market accounts")?;

        let mut markets = Vec::with_capacity(accounts.len());
        for (pubkey, account) in accounts {
            if let Ok(header) = parse_market_state(&account.data) {
                markets.push((pubkey, *header));
            }
        }
        Ok(markets)
    }

    /// Every MakerBook on a market via `getProgramAccounts`.
    ///
    /// Done here rather than through the SDK client because public RPCs reject
    /// a `getProgramAccounts` without an explicit base64 encoding, which the SDK
    /// does not set.
    pub async fn get_maker_books_for_market(&self, market: &Pubkey) -> Result<Vec<MakerBook>> {
        let config = RpcProgramAccountsConfig {
            filters: Some(vec![
                RpcFilterType::Memcmp(Memcmp::new_raw_bytes(0, MAKER_BOOK_DISCRIMINATOR.to_vec())),
                RpcFilterType::Memcmp(Memcmp::new_raw_bytes(40, market.to_bytes().to_vec())),
            ]),
            account_config: RpcAccountInfoConfig {
                encoding: Some(UiAccountEncoding::Base64),
                commitment: Some(CommitmentConfig::confirmed()),
                ..Default::default()
            },
            ..Default::default()
        };

        let accounts = self
            .rpc()
            .get_program_accounts_with_config(&ARCHER_V1_PROGRAM_ID, config)
            .await
            .context("Failed to fetch maker book accounts")?;

        let mut books = Vec::with_capacity(accounts.len());
        for (_, account) in accounts {
            if let Ok(book) = parse_maker_book(&account.data) {
                books.push(*book);
            }
        }
        Ok(books)
    }

    pub async fn get_slot(&self) -> Result<u64> {
        self.sdk.get_slot().await.context("Failed to fetch slot")
    }

    pub async fn get_token_symbols(&self, mints: &[Pubkey]) -> HashMap<Pubkey, String> {
        let mut out = HashMap::new();
        if mints.is_empty() {
            return out;
        }

        let pdas: Vec<Pubkey> = mints.iter().map(metaplex_metadata_pda).collect();
        for (mint_chunk, pda_chunk) in mints
            .chunks(MAX_MULTI_ACCOUNTS)
            .zip(pdas.chunks(MAX_MULTI_ACCOUNTS))
        {
            let Ok(accounts) = self.rpc().get_multiple_accounts(pda_chunk).await else {
                continue;
            };
            for (mint, acc) in mint_chunk.iter().zip(accounts) {
                if let Some(sym) = acc.and_then(|a| parse_metaplex_symbol(&a.data)) {
                    out.insert(*mint, sym);
                }
            }
        }

        let missing: Vec<Pubkey> = mints.iter().filter(|m| !out.contains_key(m)).copied().collect();
        for chunk in missing.chunks(MAX_MULTI_ACCOUNTS) {
            let Ok(accounts) = self.rpc().get_multiple_accounts(chunk).await else {
                continue;
            };
            for (mint, acc) in chunk.iter().zip(accounts) {
                let sym = acc.and_then(|a| {
                    (a.owner == spl_token_2022::id())
                        .then(|| parse_token2022_symbol(&a.data))
                        .flatten()
                });
                if let Some(sym) = sym {
                    out.insert(*mint, sym);
                }
            }
        }

        out
    }

    pub async fn send_instructions(
        &self,
        instructions: &[Instruction],
        signers: &[&Keypair],
        options: SendOptions,
    ) -> Result<Signature> {
        let mut all_ixs = Vec::with_capacity(instructions.len() + 2);

        if let Some(limit) = options.compute_unit_limit {
            all_ixs.push(ComputeBudgetInstruction::set_compute_unit_limit(limit));
        }
        if let Some(fee) = options.priority_fee_micro_lamports {
            all_ixs.push(ComputeBudgetInstruction::set_compute_unit_price(fee));
        }

        all_ixs.extend_from_slice(instructions);

        let blockhash = self
            .rpc()
            .get_latest_blockhash()
            .await
            .context("Failed to get blockhash")?;

        let payer = signers[0].pubkey();
        let tx = Transaction::new_signed_with_payer(&all_ixs, Some(&payer), signers, blockhash);

        let mut last_err = None;
        for _ in 0..=options.max_retries {
            match self.rpc().send_and_confirm_transaction(&tx).await {
                Ok(sig) => return Ok(sig),
                Err(e) => last_err = Some(e),
            }
        }

        Err(last_err.unwrap().into())
    }
}

fn metaplex_metadata_pda(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"metadata",
            METAPLEX_METADATA_PROGRAM.as_ref(),
            mint.as_ref(),
        ],
        &METAPLEX_METADATA_PROGRAM,
    )
    .0
}

fn read_u32(data: &[u8], offset: usize) -> Option<u32> {
    let bytes = data.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

fn read_borsh_string(data: &[u8], offset: usize) -> Option<(String, usize)> {
    let len = read_u32(data, offset)? as usize;
    let start = offset + 4;
    let end = start.checked_add(len)?;
    let bytes = data.get(start..end)?;
    let s = String::from_utf8_lossy(bytes);
    let trimmed = s.trim_matches('\0').trim().to_string();
    Some((trimmed, end))
}

fn parse_metaplex_symbol(data: &[u8]) -> Option<String> {
    let (_name, after_name) = read_borsh_string(data, 65)?;
    let (symbol, _) = read_borsh_string(data, after_name)?;
    (!symbol.is_empty()).then_some(symbol)
}

fn parse_token2022_symbol(data: &[u8]) -> Option<String> {
    const TLV_START: usize = 166;
    const TOKEN_METADATA_TYPE: u16 = 19;

    let mut offset = TLV_START;
    while offset + 4 <= data.len() {
        let ext_type = u16::from_le_bytes(data[offset..offset + 2].try_into().ok()?);
        let ext_len = u16::from_le_bytes(data[offset + 2..offset + 4].try_into().ok()?) as usize;
        if ext_type == 0 {
            break;
        }
        let val_start = offset + 4;
        let val_end = val_start.checked_add(ext_len)?;
        if val_end > data.len() {
            break;
        }
        if ext_type == TOKEN_METADATA_TYPE {
            let val = &data[val_start..val_end];
            let (_name, after_name) = read_borsh_string(val, 64)?;
            let (symbol, _) = read_borsh_string(val, after_name)?;
            return (!symbol.is_empty()).then_some(symbol);
        }
        offset = val_end;
    }
    None
}
