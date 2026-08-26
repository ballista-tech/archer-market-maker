// Instruction-encoding checks for the write path. These build instructions with
// dummy signers and assert the wire bytes match the Rust ix_builder layout —
// proving correctness without submitting a transaction.

import { expect, test } from "bun:test";
import { address, createNoopSigner, type ReadonlyUint8Array } from "@solana/kit";

import { getClearBookInstruction } from "./generated/instructions/clearBook";
import { getUpdateExpiryInSlotsInstruction } from "./generated/instructions/updateExpiryInSlots";
import { ARCHER_V1_PROGRAM_ADDRESS } from "./generated/programs/archerV1";

const SIGNER = createNoopSigner(address("11111111111111111111111111111112"));
const BOOK = address("11111111111111111111111111111113");

function u64le(data: ReadonlyUint8Array, offset: number): bigint {
  const bytes = data as Uint8Array;
  return new DataView(
    bytes.buffer,
    bytes.byteOffset,
    bytes.byteLength,
  ).getBigUint64(offset, true);
}

test("clearBook encodes disc(9) + u64 seq", () => {
  const ix = getClearBookInstruction({
    makerAccount: SIGNER,
    makerBookAccount: BOOK,
    sequenceNumber: 42n,
  });
  expect(ix.programAddress).toBe(ARCHER_V1_PROGRAM_ADDRESS);
  expect(ix.data.length).toBe(9);
  expect(ix.data[0]).toBe(9);
  expect(u64le(ix.data, 1)).toBe(42n);
  expect(ix.accounts.length).toBe(2);
});

test("updateExpiryInSlots encodes disc(30) + u64 slots", () => {
  const ix = getUpdateExpiryInSlotsInstruction({
    authorityAccount: SIGNER,
    makerBookAccount: BOOK,
    expiryInSlots: 1000n,
  });
  expect(ix.data.length).toBe(9);
  expect(ix.data[0]).toBe(30);
  expect(u64le(ix.data, 1)).toBe(1000n);
  expect(ix.accounts.length).toBe(2);
});
