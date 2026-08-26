// Ported verbatim from the Rust #[cfg(test)] module in src/volatility.rs.
// These pin the TS port to the same numeric behavior as Rust.

import { expect, test } from "bun:test";

import { VolatilityTracker } from "./volatility";

test("empty tracker returns zero", () => {
  const t = new VolatilityTracker(300);
  expect(t.realizedVol()).toBe(0);
});

test("single price returns zero", () => {
  const t = new VolatilityTracker(300);
  t.push(100);
  expect(t.realizedVol()).toBe(0);
});

test("constant price returns zero", () => {
  const t = new VolatilityTracker(300);
  for (let i = 0; i < 50; i++) {
    t.push(100);
  }
  expect(t.realizedVol()).toBeLessThan(1e-15);
});

test("known volatility", () => {
  const t = new VolatilityTracker(10);
  const prices = [
    100.0, 101.0, 99.5, 100.5, 102.0, 101.0, 100.0, 99.0, 100.0, 101.0,
  ];
  for (const p of prices) {
    t.push(p);
  }
  const vol = t.realizedVol();
  expect(vol).toBeGreaterThan(0);
  expect(vol).toBeLessThan(0.05);
});

test("wraps around ring buffer", () => {
  const t = new VolatilityTracker(5);
  // Push 10 prices — only last 5 should be kept.
  for (let i = 1; i <= 10; i++) {
    t.push(100 + i);
  }
  expect(t.sampleCount).toBe(5);
  expect(t.realizedVol()).toBeGreaterThan(0);
});

test("ignores invalid prices", () => {
  const t = new VolatilityTracker(300);
  t.push(100);
  t.push(NaN);
  t.push(-5);
  t.push(0);
  t.push(Infinity);
  expect(t.sampleCount).toBe(1);
});
