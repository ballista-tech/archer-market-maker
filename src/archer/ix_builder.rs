use anyhow::Result;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::system_program;

use super::config::MarketConfig;
use super::math::{base_amount_to_lots, quote_amount_to_lots, BookUpdate};
use super::types::*;

pub fn build_update_mid_price_ix(
    signer: &Pubkey,
    market: &Pubkey,
    maker: &Pubkey,
    new_mid_price_ticks: u64,
    sequence_number: u64,
) -> Instruction {
    let (maker_book_pda, _) = MakerBook::get_address(market, maker);
    let mut data = vec![IX_UPDATE_MID_PRICE];
    data.extend_from_slice(&sequence_number.to_le_bytes());
    data.extend_from_slice(&new_mid_price_ticks.to_le_bytes());
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(*signer, true),
            AccountMeta::new(maker_book_pda, false),
        ],
        data,
    }
}

pub fn build_clear_book_ix(
    signer: &Pubkey,
    market: &Pubkey,
    maker: &Pubkey,
    sequence_number: u64,
) -> Instruction {
    let (maker_book_pda, _) = MakerBook::get_address(market, maker);
    let mut data = vec![IX_CLEAR_BOOK];
    data.extend_from_slice(&sequence_number.to_le_bytes());
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(*signer, true),
            AccountMeta::new(maker_book_pda, false),
        ],
        data,
    }
}

pub fn build_close_maker_book_ix(maker: &Pubkey, market: &Pubkey) -> Instruction {
    let (maker_book_pda, _) = MakerBook::get_address(market, maker);
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*maker, true),
            AccountMeta::new_readonly(*market, false),
            AccountMeta::new(maker_book_pda, false),
        ],
        data: vec![IX_CLOSE_MAKER_BOOK],
    }
}

pub fn build_set_book_delegate_ix(
    maker: &Pubkey,
    market: &Pubkey,
    delegate: &Pubkey,
) -> Instruction {
    let (maker_book_pda, _) = MakerBook::get_address(market, maker);
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(*maker, true),
            AccountMeta::new(maker_book_pda, false),
            AccountMeta::new_readonly(*delegate, false),
        ],
        data: vec![IX_SET_BOOK_DELEGATE],
    }
}

pub fn build_update_sync_spread_ix(
    signer: &Pubkey,
    market: &Pubkey,
    maker: &Pubkey,
    sync_spread_ticks: u16,
) -> Instruction {
    let (maker_book_pda, _) = MakerBook::get_address(market, maker);
    let mut data = vec![IX_UPDATE_SYNC_SPREAD];
    data.extend_from_slice(&sync_spread_ticks.to_le_bytes());
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(*signer, true),
            AccountMeta::new(maker_book_pda, false),
            AccountMeta::new_readonly(*market, false),
        ],
        data,
    }
}

pub fn build_update_expiry_in_slots_ix(
    maker: &Pubkey,
    market: &Pubkey,
    expiry_in_slots: u64,
) -> Instruction {
    let (maker_book_pda, _) = MakerBook::get_address(market, maker);
    let mut data = vec![IX_UPDATE_EXPIRY_IN_SLOTS];
    data.extend_from_slice(&expiry_in_slots.to_le_bytes());
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(*maker, true),
            AccountMeta::new(maker_book_pda, false),
        ],
        data,
    }
}

pub fn build_initialize_maker_book_ix(maker: &Pubkey, market: &Pubkey, kind: u8) -> Instruction {
    let (maker_book_pda, _) = MakerBook::get_address(market, maker);
    // Instruction data: [disc, kind]. The program reads the optional kind byte
    // (0 = MM, 1 = LO); passing it explicitly keeps the book type unambiguous.
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*maker, true),
            AccountMeta::new(maker_book_pda, false),
            AccountMeta::new_readonly(*market, false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data: vec![IX_INITIALIZE_MAKER_BOOK, kind],
    }
}

pub fn build_update_instructions(
    book_update: &BookUpdate,
    market: &Pubkey,
    maker: &Pubkey,
    signer: &Pubkey,
    sequence_number: u64,
) -> Result<Vec<Instruction>> {
    let (maker_book_pda, _) = MakerBook::get_address(market, maker);
    let mut instructions = Vec::with_capacity(2);

    let mut seq = sequence_number;

    if book_update.mid_price_changed {
        let mut mid_data = vec![IX_UPDATE_MID_PRICE];
        mid_data.extend_from_slice(&seq.to_le_bytes());
        mid_data.extend_from_slice(&book_update.new_mid_price_ticks.to_le_bytes());
        instructions.push(Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new_readonly(*signer, true),
                AccountMeta::new(maker_book_pda, false),
            ],
            data: mid_data,
        });
        seq += 1;
    }

    // Layout: [disc(1), seq(8), mid_ticks(8), num_bids(1), num_asks(1), padding(5), bids(256), asks(256)]
    let update_data_len = 8 + 8 + 8 + (MAX_LEVELS * 2 * MAKER_LEVEL_SIZE); // 536
    let mut data = vec![0u8; update_data_len];
    data[0] = IX_UPDATE_BOOK;
    data[1..9].copy_from_slice(&seq.to_le_bytes());
    data[9..17].copy_from_slice(&book_update.new_mid_price_ticks.to_le_bytes());
    data[17] = book_update.bid_levels.len().min(MAX_LEVELS) as u8;
    data[18] = book_update.ask_levels.len().min(MAX_LEVELS) as u8;
    // bytes 19-23: padding (already zero)

    let bids_offset = 24;
    for (i, level) in book_update.bid_levels.iter().take(MAX_LEVELS).enumerate() {
        let off = bids_offset + i * MAKER_LEVEL_SIZE;
        data[off..off + 8].copy_from_slice(&level.size_in_base_lots.to_le_bytes());
        data[off + 8..off + 16].copy_from_slice(&level.price_offset_ticks.to_le_bytes());
    }

    let asks_offset = bids_offset + MAX_LEVELS * MAKER_LEVEL_SIZE;
    for (i, level) in book_update.ask_levels.iter().take(MAX_LEVELS).enumerate() {
        let off = asks_offset + i * MAKER_LEVEL_SIZE;
        data[off..off + 8].copy_from_slice(&level.size_in_base_lots.to_le_bytes());
        data[off + 8..off + 16].copy_from_slice(&level.price_offset_ticks.to_le_bytes());
    }

    instructions.push(Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(*signer, true),
            AccountMeta::new(maker_book_pda, false),
            AccountMeta::new_readonly(*market, false),
        ],
        data,
    });

    Ok(instructions)
}

pub fn build_deposit_ix(
    maker: &Pubkey,
    market: &Pubkey,
    base_amount: f64,
    quote_amount: f64,
    maker_base_ata: &Pubkey,
    maker_quote_ata: &Pubkey,
    base_token_program: &Pubkey,
    quote_token_program: &Pubkey,
    config: &MarketConfig,
) -> Result<Instruction> {
    let (maker_book_pda, _) = MakerBook::get_address(market, maker);

    let base_lots = if base_amount > 0.0 {
        base_amount_to_lots(base_amount, config)?
    } else {
        0
    };
    let quote_lots = if quote_amount > 0.0 {
        quote_amount_to_lots(quote_amount, config)?
    } else {
        0
    };

    let mut data = vec![IX_MAKER_DEPOSIT];
    data.extend_from_slice(&base_lots.to_le_bytes());
    data.extend_from_slice(&quote_lots.to_le_bytes());

    Ok(Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(*market, false),
            AccountMeta::new(maker_book_pda, false),
            AccountMeta::new_readonly(*maker, true),
            AccountMeta::new_readonly(config.base_mint, false),
            AccountMeta::new_readonly(config.quote_mint, false),
            AccountMeta::new(config.base_vault, false),
            AccountMeta::new(config.quote_vault, false),
            AccountMeta::new(*maker_base_ata, false),
            AccountMeta::new(*maker_quote_ata, false),
            AccountMeta::new_readonly(*base_token_program, false),
            AccountMeta::new_readonly(*quote_token_program, false),
        ],
        data,
    })
}

pub fn build_withdraw_ix(
    maker: &Pubkey,
    market: &Pubkey,
    base_amount: f64,
    quote_amount: f64,
    maker_base_ata: &Pubkey,
    maker_quote_ata: &Pubkey,
    base_token_program: &Pubkey,
    quote_token_program: &Pubkey,
    config: &MarketConfig,
) -> Result<Instruction> {
    let (maker_book_pda, _) = MakerBook::get_address(market, maker);

    let base_lots = if base_amount > 0.0 {
        base_amount_to_lots(base_amount, config)?
    } else {
        0
    };
    let quote_lots = if quote_amount > 0.0 {
        quote_amount_to_lots(quote_amount, config)?
    } else {
        0
    };

    let mut data = vec![IX_MAKER_WITHDRAW];
    data.extend_from_slice(&base_lots.to_le_bytes());
    data.extend_from_slice(&quote_lots.to_le_bytes());

    Ok(Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(*market, false),
            AccountMeta::new(maker_book_pda, false),
            AccountMeta::new_readonly(*maker, true),
            AccountMeta::new_readonly(config.base_mint, false),
            AccountMeta::new_readonly(config.quote_mint, false),
            AccountMeta::new(config.base_vault, false),
            AccountMeta::new(config.quote_vault, false),
            AccountMeta::new(*maker_base_ata, false),
            AccountMeta::new(*maker_quote_ata, false),
            AccountMeta::new_readonly(*base_token_program, false),
            AccountMeta::new_readonly(*quote_token_program, false),
        ],
        data,
    })
}
