// Pins the wrapping-u64 structure hash to values computed independently from
// the Rust algorithm (src/strategy.rs structure_hash). If the bigint wrapping
// ever drifts from Rust's wrapping_mul/wrapping_add, these fail.

import { expect, test } from "bun:test";

import { structureHash } from "./strategy";

test("structureHash matches Rust (MM, anchor 0)", () => {
  const h = structureHash(2, [100n, 200n], [150n, 250n], 2.5, 0n);
  expect(h).toBe(15666357210869655657n);
});

test("structureHash matches Rust (LO, anchor folded in)", () => {
  const h = structureHash(2, [100n, 200n], [150n, 250n], 2.5, 100000n);
  expect(h).toBe(15666357210869755657n);
});

test("structureHash stays within u64", () => {
  const h = structureHash(16, Array(16).fill(9_999_999n), Array(16).fill(9_999_999n), 250, 999999n);
  expect(h).toBeGreaterThanOrEqual(0n);
  expect(h).toBeLessThan(1n << 64n);
});
