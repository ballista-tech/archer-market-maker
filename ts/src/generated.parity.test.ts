// Byte-parity check between the generated TS codecs and the Rust program's
// #[repr(C)] bytemuck layouts. These sizes are size_of::<T>() in archer-v1
// (program/src/state/*.rs). If codegen ever drifts from the on-chain layout,
// one of these assertions fails.
//
// Run: bun test  (from ts/)

import { expect, test } from "bun:test";

import { getMakerBookEncoder } from "./generated/accounts/makerBook";
import { getMarketStateHeaderEncoder } from "./generated/accounts/marketStateHeader";
import { getMakerRegistryEncoder } from "./generated/accounts/makerRegistry";
import { getMakerLevelEncoder } from "./generated/types/makerLevel";
import { getUpdateBookInstructionDataCodec } from "./generated/instructions/updateBook";

// Authoritative sizes from archer-v1 (std::mem::size_of on the repr(C) structs).
const RUST_SIZES = {
  MakerLevel: 16,
  MakerBook: 776,
  MarketStateHeader: 270,
  MakerRegistry: 2128,
  UpdateBookData: 536, // includes the 1-byte instruction discriminator
};

test("MakerLevel is 16 bytes", () => {
  expect(getMakerLevelEncoder().fixedSize).toBe(RUST_SIZES.MakerLevel);
});

test("MakerBook decodes as a fixed 776-byte layout", () => {
  expect(getMakerBookEncoder().fixedSize).toBe(RUST_SIZES.MakerBook);
});

test("MarketStateHeader is 270 bytes", () => {
  expect(getMarketStateHeaderEncoder().fixedSize).toBe(
    RUST_SIZES.MarketStateHeader,
  );
});

test("MakerRegistry is 2128 bytes", () => {
  expect(getMakerRegistryEncoder().fixedSize).toBe(RUST_SIZES.MakerRegistry);
});

test("UpdateBook instruction data is 536 bytes", () => {
  expect(getUpdateBookInstructionDataCodec().fixedSize).toBe(
    RUST_SIZES.UpdateBookData,
  );
});
