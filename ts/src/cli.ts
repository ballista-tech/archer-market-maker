#!/usr/bin/env bun
// Read-only CLI commands, ported from src/main.rs (markets list/view, status).
// These are safe to diff field-for-field against the Rust binary on the same
// market. Write commands + `run` come in later phases.

import { createSolanaRpc, getBase64Encoder, type Address } from "@solana/kit";
import { Command } from "commander";

import { activeAskLevels, activeBidLevels, makerBalances } from "./archer/accounts";
import { ArcherClient } from "./archer/client";
import { findMakerBookPda, findMakerRegistryPda } from "./archer/pda";
import { getMakerRegistryDecoder } from "./generated/accounts/makerRegistry";
import { getMarketStateHeaderDecoder } from "./generated/accounts/marketStateHeader";

const base64Encoder = getBase64Encoder();
import {
  loadConfig,
  loadMarketsContext,
  resolvePath,
} from "./config";
import { printTable, type Align } from "./table";

function marketStatusStr(status: number): string {
  switch (status) {
    case 0:
      return "Active";
    case 1:
      return "Paused";
    case 2:
      return "Closed";
    default:
      return "Unknown";
  }
}

function bookStatusStr(status: number): string {
  switch (status) {
    case 1:
      return "Active";
    case 2:
      return "Suspended";
    default:
      return "Unknown";
  }
}

function modeStr(mode: number): string {
  switch (mode) {
    case 0:
      return "Continuous";
    case 1:
      return "Asynchronous";
    case 2:
      return "Hybrid";
    default:
      return "Unknown";
  }
}

function kindStr(kind: number): string {
  return kind === 1 ? "LO" : kind === 0 ? "MM" : "Unknown";
}

// Is our book registered in the market registry? None = no registry.
async function checkRegistry(
  client: ArcherClient,
  rpc: ReturnType<typeof createSolanaRpc>,
  market: Address,
  makerBook: Address,
): Promise<boolean | undefined> {
  const [registryPda] = await findMakerRegistryPda(market);
  const { value } = await rpc
    .getAccountInfo(registryPda, { encoding: "base64" })
    .send();
  if (!value) return undefined;
  try {
    const bytes = new Uint8Array(base64Encoder.encode(value.data[0]));
    const registry = getMakerRegistryDecoder().decode(bytes);
    const makers = registry.makers.slice(0, registry.numMakers);
    return makers.includes(makerBook);
  } catch {
    return undefined;
  }
}

async function cmdMarketsList(configPath: string, all: boolean): Promise<void> {
  const ctx = loadMarketsContext(configPath);
  const rpc = createSolanaRpc(ctx.rpcUrl);
  const client = new ArcherClient(rpc);

  let markets = await client.getAllMarkets();
  if (!all) {
    markets = markets.filter(([, h]) => h.status === 0);
  }

  if (markets.length === 0) {
    console.log(
      all
        ? "No markets found on the Archer program."
        : "No active markets found (use `--all` to include paused/closed).",
    );
    return;
  }

  markets.sort(([aPk, a], [bPk, b]) =>
    a.status !== b.status ? a.status - b.status : aPk.localeCompare(bPk),
  );

  const mintSet = new Set<Address>();
  for (const [, h] of markets) {
    mintSet.add(h.baseMint);
    mintSet.add(h.quoteMint);
  }
  const symbols = await client.getTokenSymbols([...mintSet]);
  const sym = (mint: Address) => symbols.get(mint) ?? "?";

  const rows = markets.map(([pubkey, h], i) => [
    String(i + 1),
    marketStatusStr(h.status),
    sym(h.baseMint),
    sym(h.quoteMint),
    (h.makerFeePpm / 100).toFixed(2),
    (h.takerFeePpm / 100).toFixed(2),
    pubkey,
    h.baseMint,
    h.quoteMint,
  ]);

  console.log(`Found ${markets.length} market(s):\n`);
  const aligns: Align[] = [
    "right",
    "left",
    "left",
    "left",
    "right",
    "right",
    "left",
    "left",
    "left",
  ];
  printTable(
    ["#", "Status", "Base", "Quote", "MkrBps", "TkrBps", "Market", "Base Token", "Quote Token"],
    aligns,
    rows,
  );
  console.log("\nRun `markets view --market <pubkey>` for full details.");
}

async function cmdMarketsView(
  configPath: string,
  marketArg: string | undefined,
): Promise<void> {
  const ctx = loadMarketsContext(configPath);
  const marketStr = marketArg ?? ctx.defaultMarket;
  if (!marketStr) {
    throw new Error(
      "no market given: pass --market <pubkey> or set market_pubkey in config",
    );
  }
  const market = marketStr as Address;

  const rpc = createSolanaRpc(ctx.rpcUrl);
  const client = new ArcherClient(rpc);
  const cfg = await client.getMarketConfig(market);

  const symbols = await client.getTokenSymbols([cfg.baseMint, cfg.quoteMint]);
  const sym = (mint: Address) => symbols.get(mint) ?? "?";

  console.log(`=== Market ${market} ===`);
  console.log(`Pair:         ${sym(cfg.baseMint)} / ${sym(cfg.quoteMint)}`);
  console.log(`Base mint:    ${cfg.baseMint} (${sym(cfg.baseMint)}, ${cfg.baseDecimals} decimals)`);
  console.log(`Quote mint:   ${cfg.quoteMint} (${sym(cfg.quoteMint)}, ${cfg.quoteDecimals} decimals)`);
  console.log(`Base vault:   ${cfg.baseVault}`);
  console.log(`Quote vault:  ${cfg.quoteVault}`);
  console.log(`Tick size:    ${cfg.tickSizeInQuoteAtomsPerBaseUnit} quote atoms/base unit`);
  console.log(`Base lot:     ${cfg.baseAtomsPerBaseLot} atoms`);
  console.log(`Quote lot:    ${cfg.quoteAtomsPerQuoteLot} atoms`);
  console.log(`Maker fee:    ${cfg.makerFeePpm} ppm (${(cfg.makerFeePpm / 100).toFixed(2)} bps)`);
  console.log(`Taker fee:    ${cfg.takerFeePpm} ppm (${(cfg.takerFeePpm / 100).toFixed(2)} bps)`);

  const books = await client.getMakerBooksForMarket(market);
  const active = books.filter((b) => b.status === 1);

  let bestBid: number | undefined;
  let bestAsk: number | undefined;
  const factor = cfg.ticksToPriceFactor;
  for (const book of active) {
    const mid = Number(book.midPriceTicks);
    for (const lvl of book.bidLevels) {
      if (lvl.sizeInBaseLots > 0n) {
        const ticks = mid + Number(lvl.priceOffsetTicks);
        if (ticks > 0) {
          const price = ticks * factor;
          bestBid = bestBid === undefined ? price : Math.max(bestBid, price);
        }
      }
    }
    for (const lvl of book.askLevels) {
      if (lvl.sizeInBaseLots > 0n) {
        const ticks = mid + Number(lvl.priceOffsetTicks);
        if (ticks > 0) {
          const price = ticks * factor;
          bestAsk = bestAsk === undefined ? price : Math.min(bestAsk, price);
        }
      }
    }
  }

  console.log("\n--- Liquidity ---");
  console.log(`Maker books:  ${books.length} (${active.length} active)`);
  if (bestBid !== undefined && bestAsk !== undefined) {
    const spreadBps = bestAsk > 0 ? ((bestAsk - bestBid) / bestAsk) * 10_000 : 0;
    console.log(`Best bid:     ${bestBid.toFixed(6)}`);
    console.log(`Best ask:     ${bestAsk.toFixed(6)}`);
    console.log(`Spread:       ${spreadBps.toFixed(2)} bps`);
  } else if (bestBid !== undefined) {
    console.log(`Best bid:     ${bestBid.toFixed(6)}  (no asks)`);
  } else if (bestAsk !== undefined) {
    console.log(`Best ask:     ${bestAsk.toFixed(6)}  (no bids)`);
  } else {
    console.log("No live quotes on this market.");
  }
}

async function cmdStatus(configPath: string): Promise<void> {
  const cfg = loadConfig(configPath);
  const market = cfg.market.market_pubkey as Address;
  // Owner pubkey: from the keypair path in a later phase; for read-only status
  // we accept maker_owner_pubkey when the key isn't loaded here.
  const maker = (cfg.market.maker_owner_pubkey || "") as Address;
  if (!maker) {
    throw new Error(
      "status needs maker_owner_pubkey in config (keypair loading lands in a later phase)",
    );
  }

  const rpc = createSolanaRpc(cfg.connection.rpc_url);
  const client = new ArcherClient(rpc);
  const sdkConfig = await client.getMarketConfig(market);
  const book = await client.getMakerBook(market, maker);
  const bal = makerBalances(book, sdkConfig);

  const marketAcc = await rpc
    .getAccountInfo(market, { encoding: "base64" })
    .send();
  if (!marketAcc.value) throw new Error("Failed to fetch market account");
  const headerBytes = new Uint8Array(base64Encoder.encode(marketAcc.value.data[0]));
  const header = getMarketStateHeaderDecoder().decode(headerBytes);

  const [makerBookPda] = await findMakerBookPda(market, maker);
  const registered = await checkRegistry(client, rpc, market, makerBookPda);
  const registeredStr =
    registered === true
      ? "yes"
      : registered === false
        ? "NO (book not in registry)"
        : "n/a (no registry)";

  const ZERO = "11111111111111111111111111111111";
  const delegate = book.delegate === ZERO ? "none" : book.delegate;

  console.log("=== Archer Market Maker Status ===");
  console.log(`Market:       ${market}`);
  console.log(`Maker:        ${maker}`);
  console.log(`Book PDA:     ${makerBookPda}`);
  console.log(`Mode:         ${modeStr(header.mode)}`);
  console.log(`Book kind:    ${kindStr(book.kind)}`);
  console.log(`Book status:  ${bookStatusStr(book.status)}`);
  console.log(`Registered:   ${registeredStr}`);
  console.log(`Delegate:     ${delegate}`);
  console.log(`Sync spread:  ${book.syncSpreadTicks} ticks`);
  console.log(`Expiry slots: ${book.expiryInSlots}`);
  console.log(`Mid ticks:    ${book.midPriceTicks}`);
  console.log(`Bid levels:   ${activeBidLevels(book)}`);
  console.log(`Ask levels:   ${activeAskLevels(book)}`);
  console.log(`Base free:    ${bal.baseFree.toFixed(6)}`);
  console.log(`Base locked:  ${bal.baseLocked.toFixed(6)}`);
  console.log(`Quote free:   ${bal.quoteFree.toFixed(4)}`);
  console.log(`Quote locked: ${bal.quoteLocked.toFixed(4)}`);
}

const DEFAULT_CONFIG = "config/default.toml";

const program = new Command();
program
  .name("archer-market-maker")
  .description("A simple market maker for Archer Exchange on Solana");

const markets = program.command("markets").description("Explore Archer markets");
markets
  .command("list")
  .option("-c, --config <path>", "config file", DEFAULT_CONFIG)
  .option("--all", "include paused/closed markets", false)
  .action((opts) => cmdMarketsList(resolvePath(opts.config), opts.all));
markets
  .command("view")
  .option("-c, --config <path>", "config file", DEFAULT_CONFIG)
  .option("--market <pubkey>", "market pubkey")
  .action((opts) => cmdMarketsView(resolvePath(opts.config), opts.market));

program
  .command("status")
  .option("-c, --config <path>", "config file", DEFAULT_CONFIG)
  .action((opts) => cmdStatus(resolvePath(opts.config)));

program.parseAsync().catch((err) => {
  console.error(err instanceof Error ? err.message : err);
  process.exit(1);
});
