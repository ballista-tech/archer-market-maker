use anyhow::{Result, ensure, bail};

use super::config::MarketConfig;
use super::types::{MakerLevel, MAX_LEVELS};

pub fn price_to_ticks(price: f64, config: &MarketConfig) -> Result<u64> {
    ensure!(price.is_finite() && price > 0.0, "invalid price: {price}");
    let ticks_f64 = price * config.price_to_ticks_factor();
    ensure!(ticks_f64 >= 0.5, "price {price} below tick resolution");
    ensure!(ticks_f64 <= u64::MAX as f64, "price overflow");
    let ticks = ticks_f64.round() as u64;
    ensure!(ticks > 0, "price {price} rounds to zero ticks");
    Ok(ticks)
}

#[inline]
pub fn base_lots_to_amount(lots: u64, config: &MarketConfig) -> f64 {
    lots as f64 * config.lots_to_base_factor()
}

#[inline]
pub fn quote_lots_to_amount(lots: u64, config: &MarketConfig) -> f64 {
    lots as f64 * config.lots_to_quote_factor()
}

pub fn base_amount_to_lots(amount: f64, config: &MarketConfig) -> Result<u64> {
    ensure!(amount.is_finite() && amount >= 0.0, "invalid base amount: {amount}");
    if amount == 0.0 {
        return Ok(0);
    }
    let lots = amount * config.base_to_lots_factor();
    ensure!(lots >= 1.0, "base amount {amount} below lot resolution");
    Ok(lots.floor() as u64)
}

pub fn quote_amount_to_lots(amount: f64, config: &MarketConfig) -> Result<u64> {
    ensure!(amount.is_finite() && amount >= 0.0, "invalid quote amount: {amount}");
    if amount == 0.0 {
        return Ok(0);
    }
    let lots = amount * config.quote_to_lots_factor();
    ensure!(lots >= 1.0, "quote amount {amount} below lot resolution");
    Ok(lots.floor() as u64)
}

#[derive(Debug, Clone, Copy)]
pub struct Quote {
    pub price: f64,
    pub size: f64,
}

#[derive(Debug, Clone, Default)]
pub struct TwoSidedQuote {
    pub bids: Vec<Quote>,
    pub asks: Vec<Quote>,
}

impl TwoSidedQuote {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_bid(mut self, price: f64, size: f64) -> Self {
        self.bids.push(Quote { price, size });
        self
    }

    pub fn with_ask(mut self, price: f64, size: f64) -> Self {
        self.asks.push(Quote { price, size });
        self
    }
}

#[derive(Debug, Clone)]
pub struct BookUpdate {
    pub new_mid_price_ticks: u64,
    pub bid_levels: Vec<MakerLevel>,
    pub ask_levels: Vec<MakerLevel>,
    pub mid_price_changed: bool,
}

pub fn build_book_update(
    quotes: &TwoSidedQuote,
    current_mid_price_ticks: u64,
    config: &MarketConfig,
    is_lo: bool,
) -> Result<BookUpdate> {
    ensure!(quotes.bids.len() <= MAX_LEVELS, "too many bid levels: {}", quotes.bids.len());
    ensure!(quotes.asks.len() <= MAX_LEVELS, "too many ask levels: {}", quotes.asks.len());

    for i in 1..quotes.bids.len() {
        ensure!(
            quotes.bids[i].price < quotes.bids[i - 1].price,
            "bids not strictly descending at index {i}"
        );
    }
    for i in 1..quotes.asks.len() {
        ensure!(
            quotes.asks[i].price > quotes.asks[i - 1].price,
            "asks not strictly ascending at index {i}"
        );
    }
    if let (Some(bb), Some(ba)) = (quotes.bids.first(), quotes.asks.first()) {
        ensure!(bb.price < ba.price, "crossed book: bid {} >= ask {}", bb.price, ba.price);
    }

    // LO books pin `mid_price_ticks` to 0 and each level's `price_offset_ticks`
    // is its absolute price tick. MM books anchor levels to a moving mid and
    // store signed offsets from it.
    let new_mid_price_ticks = if is_lo {
        0
    } else {
        match (quotes.bids.first(), quotes.asks.first()) {
            (Some(b), Some(a)) => price_to_ticks((b.price + a.price) / 2.0, config)?,
            (Some(b), None) => price_to_ticks(b.price, config)?,
            (None, Some(a)) => price_to_ticks(a.price, config)?,
            (None, None) => bail!("empty quote"),
        }
    };

    // For LO the on-chain mid never moves, so there is never a standalone
    // mid-price update to emit.
    let mid_price_changed = !is_lo && new_mid_price_ticks != current_mid_price_ticks;
    let anchor_ticks = new_mid_price_ticks as i64;

    let mut bid_levels: Vec<MakerLevel> = Vec::with_capacity(quotes.bids.len());
    for (i, q) in quotes.bids.iter().enumerate() {
        let price_ticks = price_to_ticks(q.price, config)?;
        let offset = price_ticks as i64 - anchor_ticks;
        let size_lots = base_amount_to_lots(q.size, config)?;

        if i > 0 {
            ensure!(
                offset < bid_levels[i - 1].price_offset_ticks,
                "duplicate bid tick offset at level {i}"
            );
        }

        bid_levels.push(MakerLevel {
            size_in_base_lots: size_lots,
            price_offset_ticks: offset,
        });
    }

    let mut ask_levels: Vec<MakerLevel> = Vec::with_capacity(quotes.asks.len());
    for (i, q) in quotes.asks.iter().enumerate() {
        let price_ticks = price_to_ticks(q.price, config)?;
        let offset = price_ticks as i64 - anchor_ticks;
        let size_lots = base_amount_to_lots(q.size, config)?;

        if i > 0 {
            ensure!(
                offset > ask_levels[i - 1].price_offset_ticks,
                "duplicate ask tick offset at level {i}"
            );
        }

        ask_levels.push(MakerLevel {
            size_in_base_lots: size_lots,
            price_offset_ticks: offset,
        });
    }

    Ok(BookUpdate {
        new_mid_price_ticks,
        bid_levels,
        ask_levels,
        mid_price_changed,
    })
}
