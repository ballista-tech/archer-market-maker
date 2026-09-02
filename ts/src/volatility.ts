// Tracks realized volatility using log returns over a rolling window.
//
// Feed pushes each new price; the tracker maintains a ring buffer and computes
// the standard deviation of log(p_t / p_{t-1}) on demand. Port of the Rust
// VolatilityTracker (src/volatility.rs) — all math stays in `number` (f64).

export class VolatilityTracker {
  private prices: number[];
  private head = 0;
  private count = 0;
  private readonly capacity: number;

  constructor(window: number) {
    this.capacity = window;
    this.prices = new Array<number>(window).fill(0);
  }

  // Record a new price sample.
  push(price: number): void {
    if (!Number.isFinite(price) || price <= 0) {
      return;
    }
    this.prices[this.head] = price;
    this.head = (this.head + 1) % this.capacity;
    if (this.count < this.capacity) {
      this.count += 1;
    }
  }

  // Realized volatility = std dev of log returns over the window.
  // Returns 0 if fewer than 2 samples.
  realizedVol(): number {
    if (this.count < 2) {
      return 0;
    }

    const nReturns = this.count - 1;

    // Walk the ring buffer in chronological order to compute log returns.
    const start = this.count < this.capacity ? 0 : this.head; // oldest sample

    let sum = 0;
    let sumSq = 0;

    let prev = this.prices[start % this.capacity]!;
    for (let i = 1; i < this.count; i++) {
      const idx = (start + i) % this.capacity;
      const cur = this.prices[idx]!;
      const lr = Math.log(cur / prev);
      sum += lr;
      sumSq += lr * lr;
      prev = cur;
    }

    const mean = sum / nReturns;
    const variance = sumSq / nReturns - mean * mean;
    return Math.sqrt(Math.max(variance, 0));
  }

  // Realized vol expressed in basis points (1 bps = 0.0001).
  realizedVolBps(): number {
    return this.realizedVol() * 10_000;
  }

  // Test accessor mirroring the Rust field checks.
  get sampleCount(): number {
    return this.count;
  }
}
