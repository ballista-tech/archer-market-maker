// Behavioral tests for the math port. math.rs has no Rust unit tests, so these
// pin the conversion + book-update logic against a realistic market config.

import { expect, test } from "bun:test";
import type { Address } from "@solana/kit";

import type { MarketStateHeader } from "../generated/accounts/marketStateHeader";
import { MarketConfig } from "./marketConfig";
import {
  baseAmountToLots,
  baseLotsToAmount,
  buildBookUpdate,
  priceToTicks,
  quoteAmountToLots,
  TwoSidedQuote,
} from "./math";

const ADDR = (s: string) => s as Address;

// SOL(9 decimals) / USDC(6 decimals)-style market.
function testConfig(): MarketConfig {
  const header = {
    baseMint: ADDR("So11111111111111111111111111111111111111112"),
    quoteMint: ADDR("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"),
    baseAtomsPerBaseLot: 1_000_000n, // 0.001 SOL per lot
    quoteAtomsPerQuoteLot: 1_000n, // 0.001 USDC per lot
    tickSizeInQuoteAtomsPerBaseUnit: 1_000n,
    rawBaseUnitsPerBaseUnit: 1n,
    makerFeePpm: 0,
    takerFeePpm: 0,
  } as unknown as MarketStateHeader;

  return new MarketConfig({
    marketPubkey: ADDR("11111111111111111111111111111111"),
    header,
    baseDecimals: 9,
    quoteDecimals: 6,
    baseTokenProgram: ADDR("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"),
    quoteTokenProgram: ADDR("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"),
    baseVault: ADDR("11111111111111111111111111111111"),
    quoteVault: ADDR("11111111111111111111111111111111"),
  });
}

test("price/ticks factor: 1 tick = 0.001 quote per base unit", () => {
  const c = testConfig();
  // tick_size 1000 quote atoms / (1 raw unit * 10^6 quote atoms) = 0.001
  expect(c.ticksToPriceFactor).toBeCloseTo(0.001, 12);
  expect(priceToTicks(100, c)).toBe(100_000n);
});

test("priceToTicks rounds and rejects sub-resolution prices", () => {
  const c = testConfig();
  expect(priceToTicks(100.0004, c)).toBe(100_000n); // rounds down
  expect(priceToTicks(100.0006, c)).toBe(100_001n); // rounds up
  expect(() => priceToTicks(0, c)).toThrow();
  expect(() => priceToTicks(-1, c)).toThrow();
});

test("base amount <-> lots round trips through floor", () => {
  const c = testConfig();
  // 1 base lot = 0.001 SOL -> baseToLotsFactor = 1000
  expect(baseAmountToLots(1, c)).toBe(1_000n);
  expect(baseLotsToAmount(1_000n, c)).toBeCloseTo(1, 12);
  expect(baseAmountToLots(0, c)).toBe(0n);
  expect(() => baseAmountToLots(0.0005, c)).toThrow(); // below lot resolution
});

test("quote amount <-> lots", () => {
  const c = testConfig();
  expect(quoteAmountToLots(1, c)).toBe(1_000n);
  expect(quoteAmountToLots(0, c)).toBe(0n);
});

test("buildBookUpdate (MM): offsets are signed distance from mid", () => {
  const c = testConfig();
  const q = new TwoSidedQuote().withBid(99.9, 1).withAsk(100.1, 1);
  const u = buildBookUpdate(q, 0n, c, false);
  // mid = 100 -> 100_000 ticks
  expect(u.newMidPriceTicks).toBe(100_000n);
  expect(u.midPriceChanged).toBe(true);
  expect(u.bidLevels[0]!.priceOffsetTicks).toBe(99_900n - 100_000n); // -100
  expect(u.askLevels[0]!.priceOffsetTicks).toBe(100_100n - 100_000n); // +100
});

test("buildBookUpdate (LO): mid pinned to 0, offsets are absolute ticks", () => {
  const c = testConfig();
  const q = new TwoSidedQuote().withBid(99.9, 1).withAsk(100.1, 1);
  const u = buildBookUpdate(q, 0n, c, true);
  expect(u.newMidPriceTicks).toBe(0n);
  expect(u.midPriceChanged).toBe(false);
  expect(u.bidLevels[0]!.priceOffsetTicks).toBe(99_900n);
  expect(u.askLevels[0]!.priceOffsetTicks).toBe(100_100n);
});

test("buildBookUpdate rejects a crossed book", () => {
  const c = testConfig();
  const q = new TwoSidedQuote().withBid(100.1, 1).withAsk(99.9, 1);
  expect(() => buildBookUpdate(q, 0n, c, false)).toThrow(/crossed book/);
});

test("buildBookUpdate rejects non-monotonic ladders", () => {
  const c = testConfig();
  const q = new TwoSidedQuote().withBid(99.9, 1).withBid(100.0, 1); // ascending bids
  expect(() => buildBookUpdate(q, 0n, c, false)).toThrow(
    /bids not strictly descending/,
  );
});
