#!/usr/bin/env bun
// Read-only CLI commands, ported from src/main.rs (markets list/view, status).
// These are safe to diff field-for-field against the Rust binary on the same
// market. Write commands + `run` come in later phases.

import {
  createSolanaRpc,
  createSolanaRpcSubscriptions,
  getBase64Encoder,
  type Address,
} from "@solana/kit";
import { findAssociatedTokenPda } from "@solana-program/token";
import { Command } from "commander";

import { activeAskLevels, activeBidLevels, makerBalances } from "./archer/accounts";
import { ArcherClient } from "./archer/client";
import type { MarketConfig } from "./archer/marketConfig";
import { baseAmountToLots, quoteAmountToLots } from "./archer/math";
import { findMakerBookPda, findMakerRegistryPda } from "./archer/pda";
import { getClearBookInstruction } from "./generated/instructions/clearBook";
import { getInitializeMakerBookInstruction } from "./generated/instructions/initializeMakerBook";
import { getMakerDepositFundsInstruction } from "./generated/instructions/makerDepositFunds";
import { getMakerWithdrawFundsInstruction } from "./generated/instructions/makerWithdrawFunds";
import { getSetBookDelegateInstruction } from "./generated/instructions/setBookDelegate";
import { getUpdateExpiryInSlotsInstruction } from "./generated/instructions/updateExpiryInSlots";
import { getMakerRegistryDecoder } from "./generated/accounts/makerRegistry";
import { getMarketStateHeaderDecoder } from "./generated/accounts/marketStateHeader";
import { loadKeypairSigner } from "./signer";
import { sendInstructions, TxSender, type TxClients } from "./tx";
import { SharedState } from "./state";
import { runFeed } from "./feed";
import { runFills } from "./fills";
import { runEngine } from "./engine";

const base64Encoder = getBase64Encoder();

const MAKER_KIND_MM = 0;
const MAKER_KIND_LO = 1;
const SYSTEM_PROGRAM = "11111111111111111111111111111111" as Address;

function makeClients(rpcUrl: string): TxClients {
  const wsUrl = rpcUrl.replace(/^http/, "ws");
  return {
    rpc: createSolanaRpc(rpcUrl),
    rpcSubscriptions: createSolanaRpcSubscriptions(wsUrl),
  };
}

function parseBookKind(kind: string): number {
  switch (kind.toLowerCase()) {
    case "mm":
    case "maker":
      return MAKER_KIND_MM;
    case "lo":
    case "limit":
    case "limit-order":
      return MAKER_KIND_LO;
    default:
      throw new Error(`invalid book kind '${kind}' (expected 'mm' or 'lo')`);
  }
}

async function makerAtas(
  owner: Address,
  cfg: MarketConfig,
): Promise<{ base: Address; quote: Address }> {
  const [base] = await findAssociatedTokenPda({
    owner,
    mint: cfg.baseMint,
    tokenProgram: cfg.baseTokenProgram,
  });
  const [quote] = await findAssociatedTokenPda({
    owner,
    mint: cfg.quoteMint,
    tokenProgram: cfg.quoteTokenProgram,
  });
  return { base, quote };
}
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

  const marketAcc = await rpc.getAccountInfo(market, { encoding: "base64" }).send();
  if (!marketAcc.value) throw new Error("Failed to fetch market account");
  const header = getMarketStateHeaderDecoder().decode(
    new Uint8Array(base64Encoder.encode(marketAcc.value.data[0])),
  );

  const symbols = await client.getTokenSymbols([cfg.baseMint, cfg.quoteMint]);
  const sym = (mint: Address) => symbols.get(mint) ?? "?";

  console.log(`=== Market ${market} ===`);
  console.log(`Status:       ${marketStatusStr(header.status)}`);
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
  // Maker pubkey: from the loaded keypair (matching Rust), falling back to
  // maker_owner_pubkey for a delegate-only config with no owner key.
  let maker: Address;
  if (cfg.market.maker_keypair_path) {
    maker = (await loadKeypairSigner(cfg.market.maker_keypair_path)).address;
  } else if (cfg.market.maker_owner_pubkey) {
    maker = cfg.market.maker_owner_pubkey as Address;
  } else {
    throw new Error("set maker_keypair_path or maker_owner_pubkey");
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

async function cmdInit(configPath: string, kind: string): Promise<void> {
  const cfg = loadConfig(configPath);
  const kindByte = parseBookKind(kind);
  const signer = await loadKeypairSigner(cfg.market.maker_keypair_path);
  const market = cfg.market.market_pubkey as Address;
  const [makerBookPda] = await findMakerBookPda(market, signer.address);

  const ix = getInitializeMakerBookInstruction({
    makerAccount: signer,
    makerBookAccount: makerBookPda,
    marketAccount: market,
    systemProgram: SYSTEM_PROGRAM,
    kind: kindByte,
  });
  const sig = await sendInstructions(
    makeClients(cfg.connection.rpc_url),
    signer,
    [ix],
  );
  const label = kindByte === MAKER_KIND_LO ? "LO (limit-order)" : "MM (market-maker)";
  console.log(`Maker book initialized [${label}]: ${sig}`);
}

async function cmdDeposit(
  configPath: string,
  base: number,
  quote: number,
): Promise<void> {
  const cfg = loadConfig(configPath);
  const signer = await loadKeypairSigner(cfg.market.maker_keypair_path);
  const market = cfg.market.market_pubkey as Address;
  const clients = makeClients(cfg.connection.rpc_url);
  const client = new ArcherClient(clients.rpc);
  const sdkConfig = await client.getMarketConfig(market);
  const [makerBookPda] = await findMakerBookPda(market, signer.address);
  const atas = await makerAtas(signer.address, sdkConfig);

  const baseLots = base > 0 ? baseAmountToLots(base, sdkConfig) : 0n;
  const quoteLots = quote > 0 ? quoteAmountToLots(quote, sdkConfig) : 0n;

  const ix = getMakerDepositFundsInstruction({
    marketAccount: market,
    makerBookAccount: makerBookPda,
    makerAccount: signer,
    baseMint: sdkConfig.baseMint,
    quoteMint: sdkConfig.quoteMint,
    baseVaultAccount: sdkConfig.baseVault,
    quoteVaultAccount: sdkConfig.quoteVault,
    makerBaseTokenAccount: atas.base,
    makerQuoteTokenAccount: atas.quote,
    baseTokenProgram: sdkConfig.baseTokenProgram,
    quoteTokenProgram: sdkConfig.quoteTokenProgram,
    baseLots,
    quoteLots,
  });
  const sig = await sendInstructions(clients, signer, [ix]);
  console.log(`Deposited ${base} base + ${quote} quote: ${sig}`);
}

async function cmdWithdraw(configPath: string): Promise<void> {
  const cfg = loadConfig(configPath);
  const signer = await loadKeypairSigner(cfg.market.maker_keypair_path);
  const market = cfg.market.market_pubkey as Address;
  const clients = makeClients(cfg.connection.rpc_url);
  const client = new ArcherClient(clients.rpc);
  const sdkConfig = await client.getMarketConfig(market);
  const [makerBookPda] = await findMakerBookPda(market, signer.address);
  const book = await client.getMakerBook(market, signer.address);

  const totalBase = book.baseFree + book.baseLocked;
  const totalQuote = book.quoteFree + book.quoteLocked;
  if (totalBase === 0n && totalQuote === 0n) {
    console.log("Nothing to withdraw.");
    return;
  }
  console.log(`  Base:  ${book.baseFree} free, ${book.baseLocked} locked`);
  console.log(`  Quote: ${book.quoteFree} free, ${book.quoteLocked} locked`);

  const instructions = [];
  if (book.baseLocked > 0n || book.quoteLocked > 0n) {
    console.log("  Locked funds detected — prepending ClearBook");
    instructions.push(
      getClearBookInstruction({
        makerAccount: signer,
        makerBookAccount: makerBookPda,
        sequenceNumber: book.lastUpdatedSequenceNumber + 1n,
      }),
    );
  }

  const atas = await makerAtas(signer.address, sdkConfig);
  const wb = book.baseLocked > 0n ? totalBase : book.baseFree;
  const wq = book.quoteLocked > 0n ? totalQuote : book.quoteFree;

  if (wb > 0n || wq > 0n) {
    instructions.push(
      getMakerWithdrawFundsInstruction({
        marketAccount: market,
        makerBookAccount: makerBookPda,
        makerAccount: signer,
        baseMint: sdkConfig.baseMint,
        quoteMint: sdkConfig.quoteMint,
        baseVaultAccount: sdkConfig.baseVault,
        quoteVaultAccount: sdkConfig.quoteVault,
        makerBaseTokenAccount: atas.base,
        makerQuoteTokenAccount: atas.quote,
        baseTokenProgram: sdkConfig.baseTokenProgram,
        quoteTokenProgram: sdkConfig.quoteTokenProgram,
        baseLots: wb,
        quoteLots: wq,
      }),
    );
  }
  const sig = await sendInstructions(clients, signer, instructions);
  console.log(`Withdrawn: ${sig}`);
}

async function cmdKill(configPath: string): Promise<void> {
  const cfg = loadConfig(configPath);
  const signer = await loadKeypairSigner(cfg.market.maker_keypair_path);
  const market = cfg.market.market_pubkey as Address;
  const clients = makeClients(cfg.connection.rpc_url);
  const client = new ArcherClient(clients.rpc);
  const [makerBookPda] = await findMakerBookPda(market, signer.address);
  const book = await client.getMakerBook(market, signer.address);

  const ix = getClearBookInstruction({
    makerAccount: signer,
    makerBookAccount: makerBookPda,
    sequenceNumber: book.lastUpdatedSequenceNumber + 1n,
  });
  const sig = await sendInstructions(clients, signer, [ix], {
    priorityFeeMicroLamports: 500_000n,
  });
  console.log(`Book cleared: ${sig}`);
}

async function cmdSetDelegate(
  configPath: string,
  delegate: string | undefined,
): Promise<void> {
  const cfg = loadConfig(configPath);
  const signer = await loadKeypairSigner(cfg.market.maker_keypair_path);
  const market = cfg.market.market_pubkey as Address;
  const [makerBookPda] = await findMakerBookPda(market, signer.address);

  const lower = delegate?.toLowerCase();
  const clear = !lower || lower === "clear" || lower === "none";
  const delegateAddress = clear ? SYSTEM_PROGRAM : (delegate as Address);

  const ix = getSetBookDelegateInstruction({
    makerAccount: signer,
    makerBookAccount: makerBookPda,
    delegateAccount: delegateAddress,
  });
  const sig = await sendInstructions(
    makeClients(cfg.connection.rpc_url),
    signer,
    [ix],
  );
  console.log(clear ? `Delegate cleared: ${sig}` : `Delegate set to ${delegateAddress}: ${sig}`);
}

async function cmdSetExpiry(configPath: string, slots: bigint): Promise<void> {
  const cfg = loadConfig(configPath);
  const signer = await loadKeypairSigner(cfg.market.maker_keypair_path);
  const market = cfg.market.market_pubkey as Address;
  const [makerBookPda] = await findMakerBookPda(market, signer.address);

  const ix = getUpdateExpiryInSlotsInstruction({
    authorityAccount: signer,
    makerBookAccount: makerBookPda,
    expiryInSlots: slots,
  });
  const sig = await sendInstructions(
    makeClients(cfg.connection.rpc_url),
    signer,
    [ix],
  );
  console.log(
    slots === 0n
      ? `expiry_in_slots set to 0 (disabled): ${sig}`
      : `expiry_in_slots set to ${slots}: ${sig}`,
  );
}

async function cmdRun(configPath: string, shadow: boolean): Promise<void> {
  const cfg = loadConfig(configPath);
  if (shadow) cfg.execution.shadow_mode = true;
  if (cfg.execution.shadow_mode) {
    console.warn("SHADOW MODE — no transactions will be sent");
  }

  // Signing identity: owner key, or a delegate key + owner pubkey.
  const ownerSigner = cfg.market.maker_keypair_path
    ? await loadKeypairSigner(cfg.market.maker_keypair_path)
    : undefined;
  const makerPubkey = (
    ownerSigner
      ? ownerSigner.address
      : cfg.market.maker_owner_pubkey || throwErr("set maker_keypair_path or maker_owner_pubkey")
  ) as Address;
  const signer = cfg.market.delegate_keypair_path
    ? await loadKeypairSigner(cfg.market.delegate_keypair_path)
    : (ownerSigner ?? throwErr("no signer: set maker_keypair_path or delegate_keypair_path"));

  const market = cfg.market.market_pubkey as Address;
  const clients = makeClients(cfg.connection.rpc_url);
  const client = new ArcherClient(clients.rpc);
  const sdkConfig = await client.getMarketConfig(market);
  console.log(`MarketConfig loaded: base=${sdkConfig.baseMint} quote=${sdkConfig.quoteMint}`);

  const initialBook = await client.getMakerBook(market, makerPubkey);
  const isLo = initialBook.kind === MAKER_KIND_LO;
  console.log(`Maker book loaded (kind=${kindStr(initialBook.kind)})`);

  const state = new SharedState();
  state.cachedMidTicks = initialBook.midPriceTicks;
  state.onchainSequenceNumber = initialBook.lastUpdatedSequenceNumber;
  state.baseTotalLots = initialBook.baseFree + initialBook.baseLocked;
  state.quoteTotalLots = initialBook.quoteFree + initialBook.quoteLocked;

  const txSender = new TxSender(
    clients.rpc,
    signer,
    BigInt(cfg.execution.priority_fee_microlamports),
    cfg.execution.shadow_mode,
    state,
  );

  const controller = new AbortController();
  const { signal } = controller;
  const [makerBookPda] = await findMakerBookPda(market, makerPubkey);

  void runFeed(state, cfg.feed, cfg.strategy.vol_window, signal);
  void runFills(state, sdkConfig, clients.rpcSubscriptions, makerBookPda, signal);

  console.log("Waiting for price feed...");
  let waited = 0;
  while (state.midPrice <= 0) {
    await new Promise((r) => setTimeout(r, 100));
    waited += 100;
    if (waited > 15_000) throw new Error("Price feed did not connect within 15 seconds");
  }
  console.log(`Price feed connected (price=${state.midPrice})`);

  const enginePromise = runEngine({
    state,
    sdkConfig,
    mmConfig: cfg,
    signer,
    makerPubkey,
    marketPubkey: market,
    txSender,
    initialSequenceNumber: initialBook.lastUpdatedSequenceNumber,
    isLo,
    signal,
  });

  console.log("Engine running. Press Ctrl+C to stop.");
  await new Promise<void>((resolve) => {
    process.once("SIGINT", () => {
      console.log("Shutting down");
      controller.abort();
      resolve();
    });
  });
  await Promise.race([
    enginePromise,
    new Promise((r) => setTimeout(r, 5000)),
  ]);
  console.log("Stopped");
}

function throwErr(msg: string): never {
  throw new Error(msg);
}

const DEFAULT_CONFIG = "config/default.toml";

const program = new Command();
program
  .name("archer-market-maker")
  .description("A simple market maker for Archer Exchange on Solana");

program
  .command("run")
  .description("Start the market maker")
  .option("-c, --config <path>", "config file", DEFAULT_CONFIG)
  .option("--shadow", "compute quotes but don't send transactions", false)
  .action((opts) => cmdRun(resolvePath(opts.config), opts.shadow));

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

program
  .command("init")
  .description("Initialize your maker book on-chain (one-time)")
  .option("-c, --config <path>", "config file", DEFAULT_CONFIG)
  .option("--kind <kind>", "book kind: mm (default) or lo", "mm")
  .action((opts) => cmdInit(resolvePath(opts.config), opts.kind));

program
  .command("deposit")
  .description("Deposit tokens into your maker book")
  .requiredOption("--base <amount>", "base amount", parseFloat)
  .requiredOption("--quote <amount>", "quote amount", parseFloat)
  .option("-c, --config <path>", "config file", DEFAULT_CONFIG)
  .action((opts) => cmdDeposit(resolvePath(opts.config), opts.base, opts.quote));

program
  .command("withdraw")
  .description("Withdraw all funds from your maker book")
  .option("-c, --config <path>", "config file", DEFAULT_CONFIG)
  .action((opts) => cmdWithdraw(resolvePath(opts.config)));

program
  .command("kill")
  .description("Emergency: clear all orders immediately")
  .option("-c, --config <path>", "config file", DEFAULT_CONFIG)
  .action((opts) => cmdKill(resolvePath(opts.config)));

program
  .command("set-delegate")
  .description("Set (or clear) the delegate allowed to manage orders")
  .option("--delegate <pubkey>", "delegate pubkey; omit or 'clear' to remove")
  .option("-c, --config <path>", "config file", DEFAULT_CONFIG)
  .action((opts) => cmdSetDelegate(resolvePath(opts.config), opts.delegate));

program
  .command("set-expiry")
  .description("Set the maker book's expiry_in_slots (0 disables)")
  .requiredOption("--slots <n>", "expiry in slots", (v) => BigInt(v))
  .option("-c, --config <path>", "config file", DEFAULT_CONFIG)
  .action((opts) => cmdSetExpiry(resolvePath(opts.config), opts.slots));

program.parseAsync().catch((err) => {
  console.error(err instanceof Error ? err.message : err);
  process.exit(1);
});
