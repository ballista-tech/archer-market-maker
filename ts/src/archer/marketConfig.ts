// Port of src/archer/config.rs — the resolved per-market configuration plus the
// precomputed float conversion factors the strategy/math layers use. On-chain
// integer quantities are bigint; the conversion factors are number (f64), as in
// Rust.

import type { Address } from "@solana/kit";

import type { MarketStateHeader } from "../generated/accounts/marketStateHeader";

export interface MarketConfigInit {
  marketPubkey: Address;
  header: MarketStateHeader;
  baseDecimals: number;
  quoteDecimals: number;
  baseTokenProgram: Address;
  quoteTokenProgram: Address;
  baseVault: Address;
  quoteVault: Address;
}

export class MarketConfig {
  readonly marketPubkey: Address;
  readonly baseMint: Address;
  readonly quoteMint: Address;
  readonly baseAtomsPerBaseLot: bigint;
  readonly quoteAtomsPerQuoteLot: bigint;
  readonly tickSizeInQuoteAtomsPerBaseUnit: bigint;
  readonly rawBaseUnitsPerBaseUnit: bigint;
  readonly makerFeePpm: number;
  readonly takerFeePpm: number;
  readonly baseDecimals: number;
  readonly quoteDecimals: number;
  readonly baseVault: Address;
  readonly quoteVault: Address;
  readonly baseTokenProgram: Address;
  readonly quoteTokenProgram: Address;

  // Precomputed conversion factors (f64), matching config.rs.
  readonly ticksToPriceFactor: number;
  readonly lotsToBaseAmountFactor: number;
  readonly lotsToQuoteAmountFactor: number;

  constructor(init: MarketConfigInit) {
    const { header } = init;
    const quoteAtomsDivisor = 10 ** init.quoteDecimals;
    const baseAtomsDivisor = 10 ** init.baseDecimals;

    this.marketPubkey = init.marketPubkey;
    this.baseMint = header.baseMint;
    this.quoteMint = header.quoteMint;
    this.baseAtomsPerBaseLot = header.baseAtomsPerBaseLot;
    this.quoteAtomsPerQuoteLot = header.quoteAtomsPerQuoteLot;
    this.tickSizeInQuoteAtomsPerBaseUnit =
      header.tickSizeInQuoteAtomsPerBaseUnit;
    this.rawBaseUnitsPerBaseUnit = header.rawBaseUnitsPerBaseUnit;
    this.makerFeePpm = header.makerFeePpm;
    this.takerFeePpm = header.takerFeePpm;
    this.baseDecimals = init.baseDecimals;
    this.quoteDecimals = init.quoteDecimals;
    this.baseVault = init.baseVault;
    this.quoteVault = init.quoteVault;
    this.baseTokenProgram = init.baseTokenProgram;
    this.quoteTokenProgram = init.quoteTokenProgram;

    this.ticksToPriceFactor =
      Number(header.tickSizeInQuoteAtomsPerBaseUnit) /
      (Number(header.rawBaseUnitsPerBaseUnit) * quoteAtomsDivisor);
    this.lotsToBaseAmountFactor =
      Number(header.baseAtomsPerBaseLot) / baseAtomsDivisor;
    this.lotsToQuoteAmountFactor =
      Number(header.quoteAtomsPerQuoteLot) / quoteAtomsDivisor;
  }

  get priceToTicksFactor(): number {
    return 1 / this.ticksToPriceFactor;
  }

  get lotsToBaseFactor(): number {
    return this.lotsToBaseAmountFactor;
  }

  get lotsToQuoteFactor(): number {
    return this.lotsToQuoteAmountFactor;
  }

  get baseToLotsFactor(): number {
    return 1 / this.lotsToBaseAmountFactor;
  }

  get quoteToLotsFactor(): number {
    return 1 / this.lotsToQuoteAmountFactor;
  }
}
