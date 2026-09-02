// Port of src/feed.rs — Binance bookTicker WebSocket feed with optional
// cross-rate synthetic pricing and reconnect backoff. Uses the global
// WebSocket (Bun / Node >=22). Cancellation is via an AbortSignal.

import type { MMConfig } from "./config";
import { nowUs, type SharedState } from "./state";
import { VolatilityTracker } from "./volatility";

interface BookTicker {
  symbol: string;
  bid: number;
  ask: number;
}

function parseBookTicker(txt: string): BookTicker | undefined {
  let msg: unknown;
  try {
    msg = JSON.parse(txt);
  } catch {
    return undefined;
  }
  if (typeof msg !== "object" || msg === null) return undefined;
  const m = msg as Record<string, unknown>;
  if (typeof m.s !== "string" || typeof m.b !== "string" || typeof m.a !== "string") {
    return undefined;
  }
  const bid = Number(m.b);
  const ask = Number(m.a);
  if (bid > 0 && ask > 0 && ask >= bid) {
    return { symbol: m.s, bid, ask };
  }
  return undefined;
}

function handleTick(
  state: SharedState,
  vol: VolatilityTracker,
  bid: number,
  ask: number,
): void {
  const mid = (bid + ask) * 0.5;
  state.midPrice = mid;
  state.priceTimestampUs = nowUs();
  vol.push(mid);
  state.volatilityBps = vol.realizedVolBps();
}

const sleep = (ms: number, signal: AbortSignal): Promise<void> =>
  new Promise((resolve) => {
    const t = setTimeout(resolve, ms);
    signal.addEventListener("abort", () => {
      clearTimeout(t);
      resolve();
    }, { once: true });
  });

export async function runFeed(
  state: SharedState,
  config: MMConfig["feed"],
  volWindow: number,
  signal: AbortSignal,
): Promise<void> {
  const primary = config.binance_symbol.toUpperCase();
  const cross = config.cross_symbol.toUpperCase();
  const useCross = cross.length > 0;

  const streams = [`${primary.toLowerCase()}@bookTicker`];
  if (useCross) streams.push(`${cross.toLowerCase()}@bookTicker`);
  const subscribeMsg = JSON.stringify({ method: "SUBSCRIBE", params: streams, id: 1 });

  let backoffMs = 100;
  const vol = new VolatilityTracker(volWindow);

  let primaryBid = 0;
  let primaryAsk = 0;
  let crossBid = 0;
  let crossAsk = 0;

  while (!signal.aborted) {
    try {
      await new Promise<void>((resolve, reject) => {
        const ws = new WebSocket(config.binance_ws_url);
        const onAbort = () => ws.close();
        signal.addEventListener("abort", onAbort, { once: true });

        ws.addEventListener("open", () => {
          backoffMs = 100;
          state.feedAlive = true;
          ws.send(subscribeMsg);
        });

        ws.addEventListener("message", (ev) => {
          const bt = parseBookTicker(String(ev.data));
          if (!bt) return;
          const sym = bt.symbol.toUpperCase();
          if (sym === primary) {
            primaryBid = bt.bid;
            primaryAsk = bt.ask;
          } else if (useCross && sym === cross) {
            crossBid = bt.bid;
            crossAsk = bt.ask;
          } else {
            return;
          }

          if (primaryBid <= 0 || primaryAsk <= 0) return;
          if (useCross && (crossBid <= 0 || crossAsk <= 0)) return;

          let bid: number;
          let ask: number;
          if (useCross) {
            const crossMid = (crossBid + crossAsk) * 0.5;
            bid = primaryBid / crossMid;
            ask = primaryAsk / crossMid;
          } else {
            bid = primaryBid;
            ask = primaryAsk;
          }

          handleTick(state, vol, bid, ask);
          state.priceNotify.notifyOne();
          state.feedAlive = true;
        });

        ws.addEventListener("close", () => {
          signal.removeEventListener("abort", onAbort);
          state.feedAlive = false;
          resolve();
        });
        ws.addEventListener("error", () => {
          state.feedAlive = false;
          reject(new Error("Binance WS error"));
        });
      });
    } catch {
      state.feedAlive = false;
    }

    if (signal.aborted) return;
    await sleep(backoffMs, signal);
    backoffMs = Math.min(backoffMs * 2, 5000);
  }
}
