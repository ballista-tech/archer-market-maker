use crate::archer::{
    config::MarketConfig,
    math::{
        BookUpdate, Quote, TwoSidedQuote,
        base_lots_to_amount, quote_lots_to_amount,
        price_to_ticks, build_book_update,
    },
};

use crate::config::StrategySettings;

pub enum QuoteDecision {
    /// Nothing to send this cycle — the on-chain book already matches the target.
    Noop,
    ClearBook,
    UpdateMidOnly { new_mid_ticks: u64 },
    UpdateFull {
        book_update: BookUpdate,
        structure_hash: u64,
    },
}

pub struct Strategy {
    config: StrategySettings,
    /// True when quoting a limit-order (LO) book. LO books can't cheaply shift
    /// their mid, so every price move requires a full re-quote at absolute
    /// price ticks rather than a `UpdateMidOnly`.
    is_lo: bool,
}

impl Strategy {
    pub fn new(config: &StrategySettings, is_lo: bool) -> Self {
        Self { config: config.clone(), is_lo }
    }

    fn vol_multiplier(&self, volatility_bps: f64) -> f64 {
        let raw = (volatility_bps / self.config.vol_baseline_bps).max(1.0);
        raw.min(self.config.vol_max_multiplier)
    }

    pub fn compute(
        &self,
        mid_price: f64,
        cached_mid_ticks: u64,
        last_structure_hash: u64,
        sdk_config: &MarketConfig,
        base_total_lots: u64,
        quote_total_lots: u64,
        volatility_bps: f64,
    ) -> (QuoteDecision, f64) {
        if !mid_price.is_finite() || mid_price <= 0.0 {
            return (QuoteDecision::ClearBook, 0.0);
        }

        let vol_mult = self.vol_multiplier(volatility_bps);
        let num_levels = self.config.spread_levels_bps.len();
        let pct_per_level = (self.config.inventory_pct / 100.0) / (num_levels as f64);

        let available_base = base_lots_to_amount(base_total_lots, sdk_config);
        let available_quote = quote_lots_to_amount(quote_total_lots, sdk_config);
        let quote_as_base = if mid_price > 0.0 { available_quote / mid_price } else { 0.0 };

        let tightest_spread = self.config.spread_levels_bps[0] * vol_mult;

        let mut bids: Vec<Quote> = Vec::with_capacity(num_levels);
        let mut asks: Vec<Quote> = Vec::with_capacity(num_levels);
        let mut bid_sizes_q: Vec<u64> = Vec::with_capacity(num_levels);
        let mut ask_sizes_q: Vec<u64> = Vec::with_capacity(num_levels);

        for &spread_bps in &self.config.spread_levels_bps {
            let effective_spread = spread_bps * vol_mult;

            let ask_size = available_base * pct_per_level;
            let bid_size = quote_as_base * pct_per_level;

            let ask_q = quantize(ask_size);
            let bid_q = quantize(bid_size);

            if ask_q > 0.0 {
                asks.push(Quote {
                    price: mid_price * (1.0 + effective_spread / 10_000.0),
                    size: ask_size,
                });
            }
            ask_sizes_q.push((ask_q * 100.0) as u64);

            if bid_q > 0.0 {
                bids.push(Quote {
                    price: mid_price * (1.0 - effective_spread / 10_000.0),
                    size: bid_size,
                });
            }
            bid_sizes_q.push((bid_q * 100.0) as u64);
        }

        if bids.is_empty() && asks.is_empty() {
            return (QuoteDecision::ClearBook, tightest_spread);
        }

        // For LO books the absolute price level is part of the book structure
        // (each level IS an absolute price), so fold the mid tick into the hash:
        // any tick move must trigger a full re-quote. MM books deliberately
        // exclude it, since a price move is handled by the cheap mid shift.
        let price_anchor = if self.is_lo {
            price_to_ticks(mid_price, sdk_config).unwrap_or(0)
        } else {
            0
        };
        let new_hash = structure_hash(num_levels, &bid_sizes_q, &ask_sizes_q, tightest_spread, price_anchor);

        if new_hash == last_structure_hash && last_structure_hash != 0 {
            // Nothing changed. MM books can cheaply re-pin their mid to the
            // latest price; LO books have an immutable mid, so there is nothing
            // to send.
            let decision = if self.is_lo {
                QuoteDecision::Noop
            } else {
                match price_to_ticks(mid_price, sdk_config) {
                    Ok(new_mid_ticks) => QuoteDecision::UpdateMidOnly { new_mid_ticks },
                    Err(_) => QuoteDecision::ClearBook,
                }
            };
            (decision, tightest_spread)
        } else {
            let reference_mid_ticks = if cached_mid_ticks > 0 {
                cached_mid_ticks
            } else {
                match price_to_ticks(mid_price, sdk_config) {
                    Ok(t) if t > 0 => t,
                    _ => return (QuoteDecision::ClearBook, tightest_spread),
                }
            };

            let mut quotes = TwoSidedQuote::new();
            for b in &bids {
                quotes = quotes.with_bid(b.price, b.size);
            }
            for a in &asks {
                quotes = quotes.with_ask(a.price, a.size);
            }

            let decision = match build_book_update(&quotes, reference_mid_ticks, sdk_config, self.is_lo) {
                Ok(book_update) => QuoteDecision::UpdateFull {
                    book_update,
                    structure_hash: new_hash,
                },
                Err(e) => {
                    tracing::warn!("build_book_update failed: {e}, clearing book");
                    QuoteDecision::ClearBook
                }
            };
            (decision, tightest_spread)
        }
    }
}

fn quantize(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn structure_hash(
    num_levels: usize,
    bid_sizes_q: &[u64],
    ask_sizes_q: &[u64],
    spread_bps: f64,
    price_anchor: u64,
) -> u64 {
    let mut h: u64 = num_levels as u64;
    for &s in bid_sizes_q {
        h = h.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(s);
    }
    for &s in ask_sizes_q {
        h = h.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(s.wrapping_add(1));
    }
    h = h.wrapping_mul(6_364_136_223_846_793_005).wrapping_add((spread_bps * 10.0).round() as u64);
    // Zero for MM books (price excluded), the mid tick for LO books.
    h = h.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(price_anchor);
    h
}
