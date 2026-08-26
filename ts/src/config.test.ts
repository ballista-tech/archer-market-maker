// Tests the config validation rules ported from validate_config in config.rs.

import { expect, test } from "bun:test";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { loadConfig, loadMarketsContext, resolvePath } from "./config";

function writeConfig(body: string): string {
  const dir = mkdtempSync(join(tmpdir(), "archer-cfg-"));
  const p = join(dir, "config.toml");
  writeFileSync(p, body);
  return p;
}

const VALID = `
[market]
market_pubkey = "u8tnfCb1JSSghuNFquQ2beStYgAN1kmd1f1Lhxbaec4"
maker_keypair_path = "~/.config/solana/id.json"
[connection]
rpc_url = "https://api.mainnet-beta.solana.com"
[feed]
binance_symbol = "SOLUSDT"
[strategy]
spread_levels_bps = [2.0, 5.0]
[execution]
[monitoring]
`;

test("loads a valid config with defaults applied", () => {
  const c = loadConfig(writeConfig(VALID));
  expect(c.strategy.inventory_pct).toBe(80.0);
  expect(c.strategy.vol_window).toBe(300);
  expect(c.execution.heartbeat_interval_ms).toBe(100);
  expect(c.feed.binance_ws_url).toBe("wss://stream.binance.com:9443/ws");
});

test("rejects empty spread levels", () => {
  const body = VALID.replace("spread_levels_bps = [2.0, 5.0]", "spread_levels_bps = []");
  expect(() => loadConfig(writeConfig(body))).toThrow(/at least 1 spread level/);
});

test("rejects non-positive spread levels", () => {
  const body = VALID.replace("spread_levels_bps = [2.0, 5.0]", "spread_levels_bps = [2.0, 0.0]");
  expect(() => loadConfig(writeConfig(body))).toThrow(/must be positive/);
});

test("rejects inventory_pct out of range", () => {
  const body = VALID.replace("[strategy]", "[strategy]\ninventory_pct = 120.0");
  expect(() => loadConfig(writeConfig(body))).toThrow(/inventory_pct/);
});

test("markets context takes rpc_url and optional default market", () => {
  const ctx = loadMarketsContext(writeConfig(VALID));
  expect(ctx.rpcUrl).toBe("https://api.mainnet-beta.solana.com");
  expect(ctx.defaultMarket).toBe("u8tnfCb1JSSghuNFquQ2beStYgAN1kmd1f1Lhxbaec4");
});

test("resolvePath expands ~/", () => {
  expect(resolvePath("~/foo").endsWith("/foo")).toBe(true);
  expect(resolvePath("/abs/path")).toBe("/abs/path");
});
