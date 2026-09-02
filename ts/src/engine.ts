// Port of src/engine.rs — the event-driven + heartbeat quoting loop, with the
// same circuit breakers (consecutive-failure clear, staleness clear, price
// deviation band) and the same sequence-number bookkeeping.

import type { Address } from "@solana/kit";

import type { MarketConfig } from "./archer/marketConfig";
import { getClearBookInstruction } from "./generated/instructions/clearBook";
import { getUpdateBookInstruction } from "./generated/instructions/updateBook";
import { getUpdateMidPriceInstruction } from "./generated/instructions/updateMidPrice";
import type { KeyPairSigner } from "@solana/kit";
import type { MMConfig } from "./config";
import { findMakerBookPda } from "./archer/pda";
import { MAX_LEVELS, type BookUpdate } from "./archer/math";
import { nowUs, type SharedState } from "./state";
import { Strategy } from "./strategy";
import { TxPriority, type TxSender } from "./tx";

const CU_CLEAR_BOOK = 650;
const CU_MID_ONLY = 850;
const CU_FULL_UPDATE = 5600;

export interface EngineDeps {
  state: SharedState;
  sdkConfig: MarketConfig;
  mmConfig: MMConfig;
  signer: KeyPairSigner;
  makerPubkey: Address;
  marketPubkey: Address;
  txSender: TxSender;
  initialSequenceNumber: bigint;
  isLo: boolean;
  signal: AbortSignal;
}

// Pad the two ladders to MAX_LEVELS with zero levels, matching the fixed 32-slot
// UpdateBookData the program expects (bids 0..15, asks 16..31).
function buildUpdateLevels(update: BookUpdate) {
  const zero = { sizeInBaseLots: 0n, priceOffsetTicks: 0n };
  const levels = [];
  for (let i = 0; i < MAX_LEVELS; i++) {
    levels.push(update.bidLevels[i] ?? zero);
  }
  for (let i = 0; i < MAX_LEVELS; i++) {
    levels.push(update.askLevels[i] ?? zero);
  }
  return levels;
}

export async function runEngine(deps: EngineDeps): Promise<void> {
  const {
    state,
    sdkConfig,
    mmConfig,
    signer,
    makerPubkey,
    marketPubkey,
    txSender,
    initialSequenceNumber,
    isLo,
    signal,
  } = deps;

  const strategy = new Strategy(mmConfig.strategy, isLo);
  const heartbeatMs = mmConfig.execution.heartbeat_interval_ms;
  const stalenessUs = mmConfig.feed.staleness_timeout_ms * 1000;
  const [makerBookPda] = await findMakerBookPda(marketPubkey, makerPubkey);

  let lastStructureHash = 0n;
  let lastSentMidTicks = 0n;
  let needsInitialBook = true;
  let localSeq = initialSequenceNumber;

  const clearBookIx = (seq: bigint) =>
    getClearBookInstruction({
      makerAccount: signer,
      makerBookAccount: makerBookPda,
      sequenceNumber: seq,
    });

  state.engineAlive = true;

  while (true) {
    // Wait for either a price update or the heartbeat timeout.
    let isHeartbeat = false;
    let heartbeatTimer: ReturnType<typeof setTimeout> | undefined;
    const heartbeat = new Promise<void>((resolve) => {
      heartbeatTimer = setTimeout(() => {
        isHeartbeat = true;
        resolve();
      }, heartbeatMs);
    });
    const aborted = new Promise<void>((resolve) =>
      signal.addEventListener("abort", () => resolve(), { once: true }),
    );
    await Promise.race([state.priceNotify.notified(), heartbeat, aborted]);
    if (heartbeatTimer) clearTimeout(heartbeatTimer);

    if (signal.aborted) {
      console.log("Engine shutting down, clearing book");
      state.engineAlive = false;
      localSeq += 1n;
      txSender.fire([clearBookIx(localSeq)], TxPriority.Emergency, CU_CLEAR_BOOK);
      return;
    }

    if (state.consecutiveFailures >= 10) {
      localSeq += 1n;
      txSender.fire([clearBookIx(localSeq)], TxPriority.Emergency, CU_CLEAR_BOOK);
      state.clearBookSends += 1;
      needsInitialBook = true;
      lastStructureHash = 0n;
      await new Promise((r) => setTimeout(r, 1000));
      continue;
    }

    const priceAgeUs = Math.max(0, nowUs() - state.priceTimestampUs);
    if (priceAgeUs > stalenessUs && state.priceTimestampUs > 0) {
      console.warn(`Price feed stale (age ${Math.floor(priceAgeUs / 1000)}ms), clearing book`);
      localSeq += 1n;
      txSender.fire([clearBookIx(localSeq)], TxPriority.Emergency, CU_CLEAR_BOOK);
      state.clearBookSends += 1;
      needsInitialBook = true;
      lastStructureHash = 0n;
      continue;
    }

    const midPrice = state.midPrice;
    if (midPrice <= 0 || !Number.isFinite(midPrice)) {
      state.cyclesTotal += 1;
      continue;
    }

    const onchainSeq = state.onchainSequenceNumber;
    if (onchainSeq > localSeq) localSeq = onchainSeq;

    const cachedMid = state.cachedMidTicks;
    const referenceMid = cachedMid > 0n ? cachedMid : lastSentMidTicks;
    const effectiveHash = needsInitialBook ? 0n : lastStructureHash;

    const { decision } = strategy.compute(
      midPrice,
      referenceMid,
      effectiveHash,
      sdkConfig,
      state.baseTotalLots,
      state.quoteTotalLots,
      state.volatilityBps,
    );

    const maxDevPct = mmConfig.strategy.max_price_deviation_pct;
    if (maxDevPct > 0 && cachedMid > 0n) {
      let candidate: bigint | undefined;
      if (decision.kind === "updateMidOnly") candidate = decision.newMidTicks;
      else if (decision.kind === "updateFull")
        candidate = decision.bookUpdate.newMidPriceTicks;
      if (candidate !== undefined) {
        const devPct =
          (Math.abs(Number(candidate) - Number(cachedMid)) / Number(cachedMid)) * 100;
        if (devPct > maxDevPct) {
          console.warn(
            `Mid deviates beyond band (${devPct.toFixed(2)}% > ${maxDevPct}%) — withholding update`,
          );
          state.cyclesTotal += 1;
          continue;
        }
      }
    }

    switch (decision.kind) {
      case "noop":
        state.cyclesTotal += 1;
        continue;
      case "clearBook":
        localSeq += 1n;
        txSender.fire([clearBookIx(localSeq)], TxPriority.Normal, CU_CLEAR_BOOK);
        state.clearBookSends += 1;
        state.updatesSent += 1;
        lastStructureHash = 0n;
        needsInitialBook = true;
        break;
      case "updateMidOnly": {
        if (decision.newMidTicks === lastSentMidTicks && !isHeartbeat) {
          state.cyclesTotal += 1;
          continue;
        }
        localSeq += 1n;
        txSender.fire(
          [
            getUpdateMidPriceInstruction({
              makerAccount: signer,
              makerBookAccount: makerBookPda,
              newMidPriceTicks: decision.newMidTicks,
              sequenceNumber: localSeq,
            }),
          ],
          TxPriority.Normal,
          CU_MID_ONLY,
        );
        state.midOnlyUpdates += 1;
        state.updatesSent += 1;
        if (isHeartbeat) state.heartbeatSends += 1;
        lastSentMidTicks = decision.newMidTicks;
        break;
      }
      case "updateFull": {
        const update = decision.bookUpdate;
        // Mirror build_update_instructions: advance localSeq once, then the
        // prepended UpdateMidPrice (if any) uses that seq and UpdateBook uses
        // seq+1 — without advancing localSeq a second time.
        localSeq += 1n;
        let seq = localSeq;
        const ixs = [];
        if (update.midPriceChanged) {
          ixs.push(
            getUpdateMidPriceInstruction({
              makerAccount: signer,
              makerBookAccount: makerBookPda,
              newMidPriceTicks: update.newMidPriceTicks,
              sequenceNumber: seq,
            }),
          );
          seq += 1n;
        }
        ixs.push(
          getUpdateBookInstruction({
            makerAccount: signer,
            makerBookAccount: makerBookPda,
            marketAccount: marketPubkey,
            sequenceNumber: seq,
            midPriceTicks: update.newMidPriceTicks,
            numBids: Math.min(update.bidLevels.length, MAX_LEVELS),
            numAsks: Math.min(update.askLevels.length, MAX_LEVELS),
            padding: new Uint8Array(5),
            levels: buildUpdateLevels(update),
          }),
        );
        txSender.fire(ixs, TxPriority.Normal, CU_FULL_UPDATE);
        state.bookUpdates += 1;
        state.updatesSent += 1;
        lastSentMidTicks = update.newMidPriceTicks;
        lastStructureHash = decision.structureHash;
        needsInitialBook = false;
        break;
      }
    }

    state.cyclesTotal += 1;
  }
}
