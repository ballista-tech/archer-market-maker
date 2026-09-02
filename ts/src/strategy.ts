// Port of src/strategy.rs — quote computation, structure hash, decision enum.
// f64 math is number; ticks/lots/hash are bigint. The structure hash uses
// wrapping u64 arithmetic, reproduced with BigInt.asUintN(64, …).

import type { MarketConfig } from "./archer/marketConfig";
import {
  baseLotsToAmount,
  buildBookUpdate,
  priceToTicks,
  quoteLotsToAmount,
  TwoSidedQuote,
  type BookUpdate,
  type Quote,
} from "./archer/math";
import type { MMConfig } from "./config";

export type QuoteDecision =
  | { kind: "noop" }
  | { kind: "clearBook" }
  | { kind: "updateMidOnly"; newMidTicks: bigint }
  | { kind: "updateFull"; bookUpdate: BookUpdate; structureHash: bigint };

const HASH_MUL = 6_364_136_223_846_793_005n;

function wrapMul(h: bigint, x: bigint): bigint {
  return BigInt.asUintN(64, h * x);
}
function wrapAdd(h: bigint, x: bigint): bigint {
  return BigInt.asUintN(64, h + x);
}

type StrategySettings = MMConfig["strategy"];

export class Strategy {
  private readonly config: StrategySettings;
  private readonly isLo: boolean;

  constructor(config: StrategySettings, isLo: boolean) {
    this.config = config;
    this.isLo = isLo;
  }

  private volMultiplier(volatilityBps: number): number {
    const raw = Math.max(volatilityBps / this.config.vol_baseline_bps, 1.0);
    return Math.min(raw, this.config.vol_max_multiplier);
  }

  compute(
    midPrice: number,
    cachedMidTicks: bigint,
    lastStructureHash: bigint,
    sdkConfig: MarketConfig,
    baseTotalLots: bigint,
    quoteTotalLots: bigint,
    volatilityBps: number,
  ): { decision: QuoteDecision; spreadBps: number } {
    if (!Number.isFinite(midPrice) || midPrice <= 0) {
      return { decision: { kind: "clearBook" }, spreadBps: 0 };
    }

    const volMult = this.volMultiplier(volatilityBps);
    const numLevels = this.config.spread_levels_bps.length;
    const pctPerLevel = this.config.inventory_pct / 100 / numLevels;

    const availableBase = baseLotsToAmount(baseTotalLots, sdkConfig);
    const availableQuote = quoteLotsToAmount(quoteTotalLots, sdkConfig);
    const quoteAsBase = midPrice > 0 ? availableQuote / midPrice : 0;

    const tightestSpread = this.config.spread_levels_bps[0]! * volMult;

    const bids: Quote[] = [];
    const asks: Quote[] = [];
    const bidSizesQ: bigint[] = [];
    const askSizesQ: bigint[] = [];

    for (const spreadBps of this.config.spread_levels_bps) {
      const effectiveSpread = spreadBps * volMult;

      const askSize = availableBase * pctPerLevel;
      const bidSize = quoteAsBase * pctPerLevel;

      const askQ = quantize(askSize);
      const bidQ = quantize(bidSize);

      if (askQ > 0) {
        asks.push({
          price: midPrice * (1 + effectiveSpread / 10_000),
          size: askSize,
        });
      }
      askSizesQ.push(BigInt(Math.trunc(askQ * 100)));

      if (bidQ > 0) {
        bids.push({
          price: midPrice * (1 - effectiveSpread / 10_000),
          size: bidSize,
        });
      }
      bidSizesQ.push(BigInt(Math.trunc(bidQ * 100)));
    }

    if (bids.length === 0 && asks.length === 0) {
      return { decision: { kind: "clearBook" }, spreadBps: tightestSpread };
    }

    // For LO books the absolute price level is part of the structure, so fold
    // the mid tick into the hash; MM books exclude it (price handled by a cheap
    // mid shift).
    let priceAnchor = 0n;
    if (this.isLo) {
      try {
        priceAnchor = priceToTicks(midPrice, sdkConfig);
      } catch {
        priceAnchor = 0n;
      }
    }
    const newHash = structureHash(
      numLevels,
      bidSizesQ,
      askSizesQ,
      tightestSpread,
      priceAnchor,
    );

    if (newHash === lastStructureHash && lastStructureHash !== 0n) {
      // Nothing changed. MM re-pins its mid cheaply; LO has an immutable mid so
      // there is nothing to send.
      if (this.isLo) {
        return { decision: { kind: "noop" }, spreadBps: tightestSpread };
      }
      try {
        const newMidTicks = priceToTicks(midPrice, sdkConfig);
        return {
          decision: { kind: "updateMidOnly", newMidTicks },
          spreadBps: tightestSpread,
        };
      } catch {
        return { decision: { kind: "clearBook" }, spreadBps: tightestSpread };
      }
    }

    let referenceMidTicks: bigint;
    if (cachedMidTicks > 0n) {
      referenceMidTicks = cachedMidTicks;
    } else {
      try {
        const t = priceToTicks(midPrice, sdkConfig);
        if (t <= 0n) {
          return { decision: { kind: "clearBook" }, spreadBps: tightestSpread };
        }
        referenceMidTicks = t;
      } catch {
        return { decision: { kind: "clearBook" }, spreadBps: tightestSpread };
      }
    }

    const quotes = new TwoSidedQuote();
    for (const b of bids) quotes.withBid(b.price, b.size);
    for (const a of asks) quotes.withAsk(a.price, a.size);

    try {
      const bookUpdate = buildBookUpdate(
        quotes,
        referenceMidTicks,
        sdkConfig,
        this.isLo,
      );
      return {
        decision: { kind: "updateFull", bookUpdate, structureHash: newHash },
        spreadBps: tightestSpread,
      };
    } catch (e) {
      // build failed → clear the book, matching the Rust warn+ClearBook path
      void e;
      return { decision: { kind: "clearBook" }, spreadBps: tightestSpread };
    }
  }
}

function quantize(v: number): number {
  return Math.round(v * 100) / 100;
}

export function structureHash(
  numLevels: number,
  bidSizesQ: bigint[],
  askSizesQ: bigint[],
  spreadBps: number,
  priceAnchor: bigint,
): bigint {
  let h = BigInt(numLevels);
  for (const s of bidSizesQ) {
    h = wrapAdd(wrapMul(h, HASH_MUL), s);
  }
  for (const s of askSizesQ) {
    h = wrapAdd(wrapMul(h, HASH_MUL), BigInt.asUintN(64, s + 1n));
  }
  h = wrapAdd(wrapMul(h, HASH_MUL), BigInt(Math.round(spreadBps * 10)));
  h = wrapAdd(wrapMul(h, HASH_MUL), priceAnchor);
  return h;
}
