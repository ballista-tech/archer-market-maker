// Port of src/archer/math.rs — price/amount conversions and the book-update
// builder. Lots and ticks are bigint (u64/i64 on-chain); prices and sizes are
// number (f64), matching where Rust uses f64.

import type { MakerLevel } from "../generated/types/makerLevel";
import type { MarketConfig } from "./marketConfig";

export const MAX_LEVELS = 16;

const U64_MAX = 18_446_744_073_709_551_615n;

export function priceToTicks(price: number, config: MarketConfig): bigint {
  if (!Number.isFinite(price) || price <= 0) {
    throw new Error(`invalid price: ${price}`);
  }
  const ticksF64 = price * config.priceToTicksFactor;
  if (ticksF64 < 0.5) {
    throw new Error(`price ${price} below tick resolution`);
  }
  if (ticksF64 > Number(U64_MAX)) {
    throw new Error("price overflow");
  }
  const ticks = BigInt(Math.round(ticksF64));
  if (ticks <= 0n) {
    throw new Error(`price ${price} rounds to zero ticks`);
  }
  return ticks;
}

export function baseLotsToAmount(lots: bigint, config: MarketConfig): number {
  return Number(lots) * config.lotsToBaseFactor;
}

export function quoteLotsToAmount(lots: bigint, config: MarketConfig): number {
  return Number(lots) * config.lotsToQuoteFactor;
}

export function baseAmountToLots(amount: number, config: MarketConfig): bigint {
  if (!Number.isFinite(amount) || amount < 0) {
    throw new Error(`invalid base amount: ${amount}`);
  }
  if (amount === 0) {
    return 0n;
  }
  const lots = amount * config.baseToLotsFactor;
  if (lots < 1) {
    throw new Error(`base amount ${amount} below lot resolution`);
  }
  return BigInt(Math.floor(lots));
}

export function quoteAmountToLots(amount: number, config: MarketConfig): bigint {
  if (!Number.isFinite(amount) || amount < 0) {
    throw new Error(`invalid quote amount: ${amount}`);
  }
  if (amount === 0) {
    return 0n;
  }
  const lots = amount * config.quoteToLotsFactor;
  if (lots < 1) {
    throw new Error(`quote amount ${amount} below lot resolution`);
  }
  return BigInt(Math.floor(lots));
}

export interface Quote {
  price: number;
  size: number;
}

export class TwoSidedQuote {
  bids: Quote[] = [];
  asks: Quote[] = [];

  withBid(price: number, size: number): this {
    this.bids.push({ price, size });
    return this;
  }

  withAsk(price: number, size: number): this {
    this.asks.push({ price, size });
    return this;
  }
}

export interface BookUpdate {
  newMidPriceTicks: bigint;
  bidLevels: MakerLevel[];
  askLevels: MakerLevel[];
  midPriceChanged: boolean;
}

export function buildBookUpdate(
  quotes: TwoSidedQuote,
  currentMidPriceTicks: bigint,
  config: MarketConfig,
  isLo: boolean,
): BookUpdate {
  if (quotes.bids.length > MAX_LEVELS) {
    throw new Error(`too many bid levels: ${quotes.bids.length}`);
  }
  if (quotes.asks.length > MAX_LEVELS) {
    throw new Error(`too many ask levels: ${quotes.asks.length}`);
  }

  for (let i = 1; i < quotes.bids.length; i++) {
    if (!(quotes.bids[i]!.price < quotes.bids[i - 1]!.price)) {
      throw new Error(`bids not strictly descending at index ${i}`);
    }
  }
  for (let i = 1; i < quotes.asks.length; i++) {
    if (!(quotes.asks[i]!.price > quotes.asks[i - 1]!.price)) {
      throw new Error(`asks not strictly ascending at index ${i}`);
    }
  }
  const bb = quotes.bids[0];
  const ba = quotes.asks[0];
  if (bb && ba && !(bb.price < ba.price)) {
    throw new Error(`crossed book: bid ${bb.price} >= ask ${ba.price}`);
  }

  // LO books pin mid_price_ticks to 0 and each level's price_offset_ticks is its
  // absolute price tick. MM books anchor levels to a moving mid and store signed
  // offsets from it.
  let newMidPriceTicks: bigint;
  if (isLo) {
    newMidPriceTicks = 0n;
  } else if (bb && ba) {
    newMidPriceTicks = priceToTicks((bb.price + ba.price) / 2, config);
  } else if (bb) {
    newMidPriceTicks = priceToTicks(bb.price, config);
  } else if (ba) {
    newMidPriceTicks = priceToTicks(ba.price, config);
  } else {
    throw new Error("empty quote");
  }

  const midPriceChanged = !isLo && newMidPriceTicks !== currentMidPriceTicks;
  const anchorTicks = newMidPriceTicks;

  const bidLevels: MakerLevel[] = [];
  for (let i = 0; i < quotes.bids.length; i++) {
    const q = quotes.bids[i]!;
    const priceTicks = priceToTicks(q.price, config);
    const offset = priceTicks - anchorTicks;
    const sizeLots = baseAmountToLots(q.size, config);

    if (i > 0 && !(offset < bidLevels[i - 1]!.priceOffsetTicks)) {
      throw new Error(`duplicate bid tick offset at level ${i}`);
    }
    bidLevels.push({ sizeInBaseLots: sizeLots, priceOffsetTicks: offset });
  }

  const askLevels: MakerLevel[] = [];
  for (let i = 0; i < quotes.asks.length; i++) {
    const q = quotes.asks[i]!;
    const priceTicks = priceToTicks(q.price, config);
    const offset = priceTicks - anchorTicks;
    const sizeLots = baseAmountToLots(q.size, config);

    if (i > 0 && !(offset > askLevels[i - 1]!.priceOffsetTicks)) {
      throw new Error(`duplicate ask tick offset at level ${i}`);
    }
    askLevels.push({ sizeInBaseLots: sizeLots, priceOffsetTicks: offset });
  }

  return { newMidPriceTicks, bidLevels, askLevels, midPriceChanged };
}
