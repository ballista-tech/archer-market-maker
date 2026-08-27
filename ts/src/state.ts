// Port of src/state.rs. Rust used atomics only because tokio is multi-threaded;
// JS is single-threaded, so the shared state is a plain mutable object and the
// price Notify becomes a simple promise-based signal.

export function nowUs(): number {
  // Microseconds since the epoch. performance.timeOrigin + performance.now()
  // gives sub-millisecond resolution; Date.now() is the fallback.
  return Math.floor((performance.timeOrigin + performance.now()) * 1000);
}

// One-shot notifier the feed uses to wake the engine on each price tick.
// notifyOne() resolves any pending waiter; notified() resolves on the next
// notifyOne() after it is called.
export class Notify {
  private resolvers: (() => void)[] = [];

  notifyOne(): void {
    const r = this.resolvers.shift();
    if (r) r();
  }

  notified(): Promise<void> {
    return new Promise((resolve) => {
      this.resolvers.push(resolve);
    });
  }
}

export class SharedState {
  midPrice = 0;
  priceTimestampUs = 0;
  feedAlive = false;

  priceNotify = new Notify();

  cachedMidTicks = 0n;
  baseTotalLots = 0n;
  quoteTotalLots = 0n;
  onchainSequenceNumber = 0n;

  volatilityBps = 0;

  consecutiveFailures = 0;

  cyclesTotal = 0;
  updatesSent = 0;
  midOnlyUpdates = 0;
  bookUpdates = 0;
  clearBookSends = 0;
  heartbeatSends = 0;

  fillsCount = 0;
  fillBaseLots = 0n;
  fillQuoteLots = 0n;
  bookResyncs = 0;

  engineAlive = false;
}
