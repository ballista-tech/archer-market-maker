//! Book construction and quoting instructions.
//!
//! The SDK's builders take an `Identity`, which for a wallet-owned book means
//! "this key is both the maker and the signer". The bot also runs as a
//! *delegate*: the book belongs to the owner's pubkey but the transaction is
//! signed by the delegate key. The helpers here take the two keys separately and
//! call the SDK's lower-level encoders, which accept any signer alongside an
//! explicit book address.

use anyhow::{Result, ensure};
use archer_sdk::config::MarketConfig;
use archer_sdk::math::{BookUpdate, TwoSidedQuote, levels};
use archer_sdk::onchain::builders::{
    MakerIdentity, UpdateBookParams, UpdateMidPriceParams, create_clear_book_instruction,
    create_update_book_instruction, create_update_mid_price_instruction,
};
use archer_sdk::onchain::{ArcherUnit, Ticks};
use archer_sdk::pda::derive_maker_book;
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;

/// Who is writing to a wallet-owned book.
#[derive(Debug, Clone, Copy)]
pub struct BookAuthority {
    /// The book owner. Third seed of the MakerBook PDA.
    pub maker: Pubkey,
    /// The key signing the transaction: the owner, or its on-chain delegate.
    pub signer: Pubkey,
}

impl BookAuthority {
    pub fn owner(maker: Pubkey) -> Self {
        Self {
            maker,
            signer: maker,
        }
    }

    pub fn book_pda(&self, market: &Pubkey) -> Pubkey {
        derive_maker_book(market, &self.maker).0
    }
}

/// Convert a two-sided quote into a [`BookUpdate`] for the given book kind.
///
/// MM books anchor every level to a moving `mid_price_ticks` and store signed
/// offsets. LO books pin the mid to `0`, so each level's offset *is* its
/// absolute price tick. The SDK's builder computes the MM form; for LO the
/// offsets are re-based onto absolute ticks and the mid is fixed at zero.
pub fn build_book_update(
    quotes: &TwoSidedQuote,
    current_mid_price_ticks: u64,
    config: &MarketConfig,
    is_lo: bool,
) -> Result<BookUpdate> {
    let mut update = levels::build_book_update(quotes, current_mid_price_ticks, config)?;
    if is_lo {
        let mid = i64::try_from(update.new_mid_price_ticks)?;
        for level in update.bid_levels.iter_mut().chain(update.ask_levels.iter_mut()) {
            let abs = level.price_offset_ticks.checked_add(mid);
            ensure!(matches!(abs, Some(t) if t > 0), "LO level price rounds to a non-positive tick");
            level.price_offset_ticks = abs.unwrap();
        }
        update.new_mid_price_ticks = 0;
        // The on-chain mid never moves on an LO book, so there is never a
        // standalone mid-price update to emit.
        update.mid_price_changed = false;
    }
    Ok(update)
}

pub fn update_mid_price_ix(
    authority: &BookAuthority,
    market: &Pubkey,
    new_mid_price_ticks: u64,
    sequence_number: u64,
) -> Instruction {
    create_update_mid_price_instruction(
        MakerIdentity::Wallet(authority.signer),
        authority.book_pda(market),
        UpdateMidPriceParams {
            new_mid_price_ticks: Ticks::new(new_mid_price_ticks),
            sequence_number,
        },
    )
}

pub fn clear_book_ix(authority: &BookAuthority, market: &Pubkey, sequence_number: u64) -> Instruction {
    create_clear_book_instruction(
        MakerIdentity::Wallet(authority.signer),
        authority.book_pda(market),
        sequence_number,
    )
}

pub fn update_book_ix(
    authority: &BookAuthority,
    market: &Pubkey,
    book_update: &BookUpdate,
    sequence_number: u64,
) -> Instruction {
    create_update_book_instruction(
        MakerIdentity::Wallet(authority.signer),
        *market,
        authority.book_pda(market),
        UpdateBookParams {
            mid_price_ticks: book_update.new_mid_price_ticks,
            bid_levels: book_update.bid_levels.clone(),
            ask_levels: book_update.ask_levels.clone(),
            sequence_number,
        },
    )
}

/// The instruction(s) for a full book write, starting at `sequence_number`.
///
/// When the mid moved this is `[UpdateMidPrice, UpdateBook]` with consecutive
/// sequence numbers; otherwise just `[UpdateBook]`. Returns the last sequence
/// number used so the caller's counter stays exactly in step with the chain —
/// the program requires every write to be `last + 1 ..= last + 65535`, and a
/// counter that falls behind by one is rejected as stale.
pub fn update_instructions(
    authority: &BookAuthority,
    market: &Pubkey,
    book_update: &BookUpdate,
    sequence_number: u64,
) -> (Vec<Instruction>, u64) {
    let mut seq = sequence_number;
    let mut ixs = Vec::with_capacity(2);
    if book_update.mid_price_changed {
        ixs.push(update_mid_price_ix(authority, market, book_update.new_mid_price_ticks, seq));
        seq += 1;
    }
    ixs.push(update_book_ix(authority, market, book_update, seq));
    (ixs, seq)
}

#[cfg(test)]
mod tests {
    use super::*;
    use archer_sdk::math::ticks::price_to_ticks;
    use archer_sdk::onchain::bytemuck::Zeroable;
    use archer_sdk::onchain::{BaseAtomsPerLot, MarketStateHeader, QuoteAtomsPerBaseUnitPerTick, QuoteAtomsPerLot};

    // A SOL/USDC-shaped market: 9/6 decimals, 0.001 SOL lots, 0.001 USDC
    // quote lots, 0.001 USDC ticks.
    fn config() -> MarketConfig {
        let mut header = MarketStateHeader::zeroed();
        header.base_atoms_per_base_lot = BaseAtomsPerLot::new(1_000_000);
        header.quote_atoms_per_quote_lot = QuoteAtomsPerLot::new(1_000);
        header.tick_size_in_quote_atoms_per_base_unit = QuoteAtomsPerBaseUnitPerTick::new(1_000);
        header.raw_base_units_per_base_unit = 1;
        MarketConfig::from_header(Pubkey::new_unique(), &header, 9, 6, spl_token::id(), spl_token::id())
    }

    fn quotes() -> TwoSidedQuote {
        TwoSidedQuote::new()
            .with_bid(149.90, 1.0)
            .with_bid(149.80, 2.0)
            .with_ask(150.10, 1.0)
            .with_ask(150.20, 2.0)
    }

    #[test]
    fn mm_book_anchors_levels_to_the_mid() {
        let cfg = config();
        let update = build_book_update(&quotes(), 0, &cfg, false).unwrap();
        let mid = price_to_ticks(150.0, &cfg).unwrap();
        assert_eq!(update.new_mid_price_ticks, mid);
        assert!(update.mid_price_changed);
        assert_eq!(update.bid_levels[0].price_offset_ticks, -100);
        assert_eq!(update.ask_levels[1].price_offset_ticks, 200);
    }

    #[test]
    fn lo_book_pins_mid_to_zero_and_uses_absolute_ticks() {
        let cfg = config();
        let update = build_book_update(&quotes(), 0, &cfg, true).unwrap();
        assert_eq!(update.new_mid_price_ticks, 0);
        assert!(!update.mid_price_changed);
        assert_eq!(update.bid_levels[0].price_offset_ticks, price_to_ticks(149.90, &cfg).unwrap() as i64);
        assert_eq!(update.bid_levels[1].price_offset_ticks, price_to_ticks(149.80, &cfg).unwrap() as i64);
        assert_eq!(update.ask_levels[0].price_offset_ticks, price_to_ticks(150.10, &cfg).unwrap() as i64);
        assert_eq!(update.ask_levels[1].price_offset_ticks, price_to_ticks(150.20, &cfg).unwrap() as i64);
        // Sizes are untouched by the re-basing.
        assert_eq!(update.bid_levels[1].size_in_base_lots.as_u64(), 2_000);
    }

    #[test]
    fn full_update_uses_consecutive_sequence_numbers() {
        let cfg = config();
        let authority = BookAuthority::owner(Pubkey::new_unique());
        let market = Pubkey::new_unique();

        // Mid moved: UpdateMidPrice at seq, UpdateBook at seq + 1.
        let moved = build_book_update(&quotes(), 1, &cfg, false).unwrap();
        let (ixs, last) = update_instructions(&authority, &market, &moved, 10);
        assert_eq!(ixs.len(), 2);
        assert_eq!(last, 11);
        assert_eq!(u64::from_le_bytes(ixs[0].data[1..9].try_into().unwrap()), 10);
        assert_eq!(u64::from_le_bytes(ixs[1].data[1..9].try_into().unwrap()), 11);

        // Mid unchanged: a single UpdateBook at seq.
        let mid = price_to_ticks(150.0, &cfg).unwrap();
        let same = build_book_update(&quotes(), mid, &cfg, false).unwrap();
        let (ixs, last) = update_instructions(&authority, &market, &same, 12);
        assert_eq!(ixs.len(), 1);
        assert_eq!(last, 12);
    }

    #[test]
    fn delegate_signs_on_the_owners_book() {
        let owner = Pubkey::new_unique();
        let delegate = Pubkey::new_unique();
        let market = Pubkey::new_unique();
        let authority = BookAuthority { maker: owner, signer: delegate };
        let ix = update_mid_price_ix(&authority, &market, 1, 1);
        assert_eq!(ix.accounts[0].pubkey, delegate);
        assert!(ix.accounts[0].is_signer);
        assert_eq!(ix.accounts[1].pubkey, derive_maker_book(&market, &owner).0);
    }

    #[test]
    fn update_mid_price_uses_the_fast_path_layout() {
        let authority = BookAuthority::owner(Pubkey::new_unique());
        let ix = update_mid_price_ix(&authority, &Pubkey::new_unique(), 7, 3);
        assert_eq!(ix.accounts.len(), 3, "[signer, book, clock]: the program's fast path");
        assert_eq!(ix.accounts[2].pubkey, solana_sdk::sysvar::clock::ID);
        assert_eq!(ix.data.len(), 17);
        assert_eq!(u64::from_le_bytes(ix.data[1..9].try_into().unwrap()), 3);
        assert_eq!(u64::from_le_bytes(ix.data[9..17].try_into().unwrap()), 7);
    }
}
