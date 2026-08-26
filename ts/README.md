# archer-market-maker (TypeScript)

A TypeScript port of the Rust `archer-market-maker` — a market maker for the
Archer Exchange on Solana. It streams live prices, quotes bid/ask levels around
mid, and manages an on-chain maker book.

This port is built to stay in lockstep with the Rust bot: the on-chain contract
layer is **code-generated from the program IDL**, and only the trading logic is
hand-written. See [Keeping in sync](#keeping-in-sync).

## Requirements

- [Bun](https://bun.sh) ≥ 1.3 (used as runtime, test runner, and package manager)
- A Solana RPC endpoint (and websocket endpoint for `run`)
- A Solana keypair (`id.json`) for any command that signs a transaction

## Install

```sh
cd ts
bun install
```

## Configuration

Commands read a TOML config (default `config/default.toml`, override with
`--config`). The schema mirrors the Rust bot:

```toml
[market]
market_pubkey      = "u8tnfCb1JSSghuNFquQ2beStYgAN1kmd1f1Lhxbaec4"
maker_keypair_path = "~/.config/solana/id.json"
# Optional delegate setup (owner key stays offline):
# delegate_keypair_path = "~/.config/solana/delegate.json"
# maker_owner_pubkey    = "OWNER_PUBKEY"

[connection]
rpc_url = "https://api.mainnet-beta.solana.com"
# ws_url is derived from rpc_url when omitted (https→wss, http→ws)

[feed]
binance_symbol = "SOLUSDT"
# cross_symbol = "USDCUSDT"   # synthetic price = primary / cross_mid
staleness_timeout_ms = 5000

[strategy]
spread_levels_bps       = [2, 5, 7, 10, 12, 15, 20, 25]
inventory_pct           = 80
vol_window              = 300
vol_baseline_bps        = 5
vol_max_multiplier      = 5
max_price_deviation_pct = 5

[execution]
heartbeat_interval_ms      = 100
priority_fee_microlamports = 100
shadow_mode                = false

[monitoring]
log_level = "info"
```

## Commands

Run via `bun run src/cli.ts <command>` (or wire a `bin` entry).

| Command | What it does |
| --- | --- |
| `run [--shadow]` | Start the market maker. `--shadow` computes quotes but sends no transactions. |
| `markets list [--all]` | List Archer markets (active only unless `--all`). |
| `markets view [--market <pubkey>]` | One market's config + live top-of-book. |
| `status` | On-chain maker-book status for the configured market. |
| `init [--kind mm\|lo]` | Initialize your maker book (one-time). |
| `deposit --base <amt> --quote <amt>` | Deposit tokens into the book. |
| `withdraw` | Withdraw all funds (clears the book first if funds are locked). |
| `kill` | Emergency: clear all orders immediately. |
| `set-delegate [--delegate <pubkey>]` | Set or (omit / `clear`) remove the order-management delegate. |
| `set-expiry --slots <n>` | Set `expiry_in_slots` (0 disables the aggregator expiry check). |

All commands take `-c, --config <path>` (default `config/default.toml`).

Example — dry-run against mainnet:

```sh
bun run src/cli.ts run --config config/default.toml --shadow
```

## Testing & type-checking

```sh
bun test          # unit + byte-parity + instruction-encoding tests
bun run typecheck  # tsc --noEmit
```

Tests include parity checks that pin the TS output to the Rust behavior:
account/instruction byte sizes, the ported volatility suite, and the
wrapping-u64 structure hash.

## Keeping in sync

The repository is polyglot: the Rust bot lives at the repo root, the TS bot in
`ts/`, and the program IDL at `idl/archer_v1.json` is the single source of truth.

- **Generated code** (`ts/src/generated/`) — account layouts, instruction
  encoders, events, errors. Produced by `node codegen.mjs` (Codama). **Never
  edit by hand.**
- **Hand-written code** (`ts/src/`) — the trading logic (feed, strategy, engine,
  CLI). This is the only part that needs a human when behavior changes.

When the on-chain program changes: update the IDL, run `node codegen.mjs`, and
commit the regenerated `ts/src/generated/`. CI (`scripts/check-codegen.sh`)
fails if the committed bindings drift from the IDL, so the schema layers can
never silently diverge.

## Numeric convention

On-chain integer quantities — lots, ticks, sequence numbers — are `bigint`.
Only the floating-point price/size math is `number`, matching exactly where the
Rust code uses `f64`. This avoids JavaScript's 2⁵³ precision limit on on-chain
values.
