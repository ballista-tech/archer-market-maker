// Port of src/archer/accounts.rs — balance and active-level helpers over a
// decoded MakerBook.

import type { MakerBook } from "../generated/accounts/makerBook";
import { baseLotsToAmount, quoteLotsToAmount } from "./math";
import type { MarketConfig } from "./marketConfig";

export interface MakerBalances {
  baseFree: number;
  baseLocked: number;
  quoteFree: number;
  quoteLocked: number;
  baseTotal: number;
  quoteTotal: number;
}

export function makerBalances(
  book: MakerBook,
  config: MarketConfig,
): MakerBalances {
  const baseFree = baseLotsToAmount(book.baseFree, config);
  const baseLocked = baseLotsToAmount(book.baseLocked, config);
  const quoteFree = quoteLotsToAmount(book.quoteFree, config);
  const quoteLocked = quoteLotsToAmount(book.quoteLocked, config);
  return {
    baseFree,
    baseLocked,
    quoteFree,
    quoteLocked,
    baseTotal: baseFree + baseLocked,
    quoteTotal: quoteFree + quoteLocked,
  };
}

// Levels are packed contiguously from index 0; the first zero-size level ends
// the ladder (take_while in Rust).
function countActive(levels: readonly { sizeInBaseLots: bigint }[]): number {
  let n = 0;
  for (const lvl of levels) {
    if (lvl.sizeInBaseLots > 0n) {
      n += 1;
    } else {
      break;
    }
  }
  return n;
}

export function activeBidLevels(book: MakerBook): number {
  return countActive(book.bidLevels);
}

export function activeAskLevels(book: MakerBook): number {
  return countActive(book.askLevels);
}
