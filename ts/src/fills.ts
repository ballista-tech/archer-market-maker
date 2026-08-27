// Port of src/fills.rs — live fill + inventory subscriptions over the RPC
// websocket. Fill events come from `Program data:` log lines; inventory is
// resynced from the maker-book account subscription (the exact source of truth).

import {
  getBase64Encoder,
  type Address,
  type RpcSubscriptions,
  type SolanaRpcSubscriptionsApi,
} from "@solana/kit";

import { getMakerBookDecoder } from "./generated/accounts/makerBook";
import {
  getMakerFillEventEventDecoder,
  MAKER_FILL_EVENT_EVENT_DISCRIMINATOR,
} from "./generated/events/makerFillEvent";
import { baseLotsToAmount, quoteLotsToAmount } from "./archer/math";
import type { MarketConfig } from "./archer/marketConfig";
import type { SharedState } from "./state";

const base64 = getBase64Encoder();
const fillDecoder = getMakerFillEventEventDecoder();
const bookDecoder = getMakerBookDecoder();

function startsWith(data: Uint8Array, prefix: Uint8Array): boolean {
  if (data.length < prefix.length) return false;
  for (let i = 0; i < prefix.length; i++) {
    if (data[i] !== prefix[i]) return false;
  }
  return true;
}

const disc = new Uint8Array(MAKER_FILL_EVENT_EVENT_DISCRIMINATOR);

function handleLogs(
  state: SharedState,
  sdkConfig: MarketConfig,
  logs: readonly string[] | null,
  err: unknown,
  signature: string,
): void {
  if (err) return; // failed transactions never settle fills
  if (!logs) return;
  for (const line of logs) {
    const b64 = line.startsWith("Program data: ")
      ? line.slice("Program data: ".length)
      : undefined;
    if (!b64) continue;
    let bytes: Uint8Array;
    try {
      bytes = new Uint8Array(base64.encode(b64.trim()));
    } catch {
      continue;
    }
    if (!startsWith(bytes, disc)) continue;
    let ev;
    try {
      ev = fillDecoder.decode(bytes);
    } catch {
      continue;
    }
    state.fillsCount += 1;
    state.fillBaseLots += ev.baseLotsFilled;
    state.fillQuoteLots += ev.quoteLotsFilled;

    const side = ev.side === 0 ? "BID/buy" : "ASK/sell";
    const base = baseLotsToAmount(ev.baseLotsFilled, sdkConfig);
    const quote = quoteLotsToAmount(ev.quoteLotsFilled, sdkConfig);
    const price = Number(ev.absolutePriceTicks) * sdkConfig.ticksToPriceFactor;
    console.log(
      `Fill side=${side} base=${base} quote=${quote} price=${price} seq=${ev.sequenceNumber} sig=${signature}`,
    );
  }
}

function handleAccount(state: SharedState, dataB64: string): void {
  let bytes: Uint8Array;
  try {
    bytes = new Uint8Array(base64.encode(dataB64));
  } catch {
    return;
  }
  let book;
  try {
    book = bookDecoder.decode(bytes);
  } catch {
    return;
  }
  state.baseTotalLots = book.baseFree + book.baseLocked;
  state.quoteTotalLots = book.quoteFree + book.quoteLocked;
  state.cachedMidTicks = book.midPriceTicks;
  // Only ever move the sequence forward; out-of-order ws frames are possible.
  if (book.lastUpdatedSequenceNumber > state.onchainSequenceNumber) {
    state.onchainSequenceNumber = book.lastUpdatedSequenceNumber;
  }
  state.bookResyncs += 1;
}

export async function runFills(
  state: SharedState,
  sdkConfig: MarketConfig,
  rpcSubscriptions: RpcSubscriptions<SolanaRpcSubscriptionsApi>,
  makerBookPda: Address,
  signal: AbortSignal,
): Promise<void> {
  let backoffMs = 200;
  while (!signal.aborted) {
    try {
      await runOnce(state, sdkConfig, rpcSubscriptions, makerBookPda, signal);
      return; // cancelled cleanly
    } catch (e) {
      console.warn(
        `fills subscription dropped: ${e instanceof Error ? e.message : e}; reconnecting in ${backoffMs}ms`,
      );
    }
    if (signal.aborted) return;
    await new Promise((r) => setTimeout(r, backoffMs));
    backoffMs = Math.min(backoffMs * 2, 5000);
  }
}

async function runOnce(
  state: SharedState,
  sdkConfig: MarketConfig,
  subs: RpcSubscriptions<SolanaRpcSubscriptionsApi>,
  makerBookPda: Address,
  signal: AbortSignal,
): Promise<void> {
  const logsSub = await subs
    .logsNotifications({ mentions: [makerBookPda] }, { commitment: "confirmed" })
    .subscribe({ abortSignal: signal });
  const acctSub = await subs
    .accountNotifications(makerBookPda, {
      encoding: "base64",
      commitment: "confirmed",
    })
    .subscribe({ abortSignal: signal });

  console.log(`Fill + book subscriptions active: ${makerBookPda}`);

  await Promise.race([
    (async () => {
      for await (const note of logsSub) {
        handleLogs(
          state,
          sdkConfig,
          note.value.logs,
          note.value.err,
          note.value.signature,
        );
      }
    })(),
    (async () => {
      for await (const note of acctSub) {
        handleAccount(state, note.value.data[0]);
      }
    })(),
  ]);
}
