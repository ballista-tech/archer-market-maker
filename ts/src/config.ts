// Port of src/config.rs — TOML config schema, validation, and the lightweight
// markets context used by the read-only commands. Uses zod for the runtime
// validation that serde + validate_config do in Rust.

import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

import toml from "@iarna/toml";
import { z } from "zod";

const marketSettings = z.object({
  market_pubkey: z.string(),
  maker_keypair_path: z.string().default(""),
  delegate_keypair_path: z.string().default(""),
  maker_owner_pubkey: z.string().default(""),
});

const connectionSettings = z.object({
  rpc_url: z.string(),
  ws_url: z.string().default(""),
});

const feedSettings = z.object({
  binance_symbol: z.string(),
  cross_symbol: z.string().default(""),
  binance_ws_url: z.string().default("wss://stream.binance.com:9443/ws"),
  staleness_timeout_ms: z.number().int().default(5000),
});

const strategySettings = z.object({
  spread_levels_bps: z.array(z.number()),
  inventory_pct: z.number().default(80.0),
  vol_window: z.number().int().default(300),
  vol_baseline_bps: z.number().default(5.0),
  vol_max_multiplier: z.number().default(5.0),
  max_price_deviation_pct: z.number().default(5.0),
});

const executionSettings = z.object({
  heartbeat_interval_ms: z.number().int().default(100),
  priority_fee_microlamports: z.number().int().default(100),
  shadow_mode: z.boolean().default(false),
});

const monitoringSettings = z.object({
  log_level: z.string().default("info"),
});

export const mmConfigSchema = z.object({
  market: marketSettings,
  connection: connectionSettings,
  feed: feedSettings,
  strategy: strategySettings,
  execution: executionSettings,
  monitoring: monitoringSettings,
});

export type MMConfig = z.infer<typeof mmConfigSchema>;

function validateConfig(c: MMConfig): void {
  const ensure = (cond: boolean, msg: string) => {
    if (!cond) throw new Error(msg);
  };
  ensure(c.market.market_pubkey.length > 0, "market_pubkey required");
  ensure(
    c.market.maker_keypair_path.length > 0 ||
      c.market.maker_owner_pubkey.length > 0,
    "set maker_keypair_path, or maker_owner_pubkey (+ delegate_keypair_path) for a delegate-only run",
  );
  ensure(c.connection.rpc_url.length > 0, "rpc_url required");
  ensure(c.feed.binance_symbol.length > 0, "binance_symbol required");
  ensure(
    c.strategy.spread_levels_bps.length >= 1,
    "need at least 1 spread level",
  );
  ensure(
    c.strategy.spread_levels_bps.length <= 16,
    "max 16 levels per side",
  );
  ensure(
    c.strategy.spread_levels_bps.every((s) => s > 0),
    "all spread levels must be positive",
  );
  ensure(
    c.strategy.inventory_pct > 0 && c.strategy.inventory_pct <= 100,
    "inventory_pct must be between 0 and 100",
  );
  ensure(c.strategy.vol_window >= 2, "vol_window must be >= 2");
  ensure(c.strategy.vol_baseline_bps > 0, "vol_baseline_bps must be positive");
  ensure(
    c.strategy.vol_max_multiplier >= 1.0,
    "vol_max_multiplier must be >= 1.0",
  );
  ensure(
    c.strategy.max_price_deviation_pct >= 0,
    "max_price_deviation_pct must be >= 0 (0 disables the check)",
  );
}

export function loadConfig(path: string): MMConfig {
  const contents = readFileSync(path, "utf8");
  const config = mmConfigSchema.parse(toml.parse(contents));
  validateConfig(config);
  return config;
}

export interface MarketsContext {
  rpcUrl: string;
  defaultMarket?: string;
}

// Minimal config used by the read-only `markets` commands: rpc_url is required,
// market_pubkey is optional.
export function loadMarketsContext(path: string): MarketsContext {
  const partial = z
    .object({
      connection: connectionSettings,
      market: z.object({ market_pubkey: z.string().default("") }).optional(),
    })
    .parse(toml.parse(readFileSync(path, "utf8")));

  if (partial.connection.rpc_url.length === 0) {
    throw new Error("rpc_url required in config");
  }
  const defaultMarket =
    partial.market?.market_pubkey && partial.market.market_pubkey.length > 0
      ? partial.market.market_pubkey
      : undefined;

  const context: MarketsContext = { rpcUrl: partial.connection.rpc_url };
  if (defaultMarket) {
    context.defaultMarket = defaultMarket;
  }
  return context;
}

export function resolvePath(s: string): string {
  if (s.startsWith("~/")) {
    return join(homedir(), s.slice(2));
  }
  return s;
}
