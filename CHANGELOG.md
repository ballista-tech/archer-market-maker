# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [2026-09-28]

### Changed
- **On-chain layer moved to [`archer-sdk`](https://github.com/ballista-tech/archer-sdk).** The bot no longer declares its own account layouts, discriminators, instruction encoders or tick/lot math. `src/archer/types.rs`, `ix_builder.rs`, `accounts.rs`, `config.rs` and `math.rs` are gone; `src/archer/` is now just `client.rs` (SDK client + market scan, token symbols, confirmed send) and `quote.rs` (MM/LO book construction and delegate-signed quoting instructions). A program change now reaches this bot by bumping the `archer-sdk` rev in `Cargo.toml`, not by hand-mirroring bytes.
- **Ready for the v2 program upgrade.** The v2 program keeps every account layout byte-identical, so no migration is needed, but it changes behaviour the bot relied on:
  - *Sequence numbers are a bounded counter* (`last < seq <= last + 65535`). The engine already derived them from the book; it now also carries forward the last number a two-instruction update used, so the cycle after a mid move is no longer sent with a stale number.
  - *Deferred quote rebalancing.* `UpdateMidPrice` no longer moves `quote_locked`/`quote_free` and can no longer fail on balance; a book repriced beyond its free quote is silently skipped by the matching engine instead. `status` now reports the pending reprice and whether the book is fillable, `run` warns when the book becomes unfundable, and all balance displays use the SDK's projected balances.
  - *Market modes and the per-book sync spread are gone.* `status` no longer prints `Mode` or `Sync spread`.
  - *`UpdateMidPrice` passes the Clock sysvar*, taking the v2 fast path (~89 CUs).
  - *New `Frozen` market status* is recognised in `markets list`, `markets view` and `status`.
- `markets view` top-of-book now counts only books the matching engine would fill (active, unexpired, fundable), the same filter the program applies.
- `withdraw` sends exact lot amounts instead of round-tripping through floats, and clears the book first whenever a reprice is pending.
- `run` refuses books owned by an ArcherAccount (the v2 delegated-identity PDA); this bot drives wallet-owned books, optionally through a delegate key.

## [2026-07-16]

### Added
- **`markets` CLI command.** Explore markets without a fully configured book:
  - `markets list` fetches every `MarketState` on the Archer program (`getProgramAccounts` filtered by the `ACHRMKT1` discriminator) and renders a bordered, copy-paste-friendly table with on-chain token symbols, full mint addresses, and maker/taker fees. Shows **active markets only** by default; pass `--all` to include paused/closed. Rows are sorted active-first.
  - `markets view [--market <pubkey>]` prints a single market's config (pair, mints + decimals, vaults, tick/lot sizes, fees) plus a live top-of-book (best bid/ask + spread) aggregated across all active maker books. Falls back to `market_pubkey` from the config when `--market` is omitted.
  - Both commands only need `[connection].rpc_url` — a fresh user can list markets to discover a `market_pubkey` before setting up a keypair.
- **On-chain token symbol resolution.** New `ArcherClient::get_token_symbols` resolves mint → symbol via Metaplex Token Metadata PDAs, falling back to the Token-2022 metadata extension on the mint itself (batched through `getMultipleAccounts`).

## [2026-07-15]

### Added
- **Price deviation circuit breaker.** New `[strategy].max_price_deviation_pct` (default `5.0`). Before sending a mid update, the engine withholds it if the new mid deviates more than this percent from the last on-chain mid (ticks are linear in price, so the tick ratio is the price deviation). Defends against bad feed ticks — e.g. a cold-start cross-rate glitch — that would otherwise quote a wildly mispriced book. Only applies with a real on-chain reference (fresh/LO books with mid 0, and a `0` config, disable it). Pairs with the cross-quote cold-start fix below.
- **Cross-pricing cold-start fix.** `cross_bid`/`cross_ask` now initialize to `0.0` instead of `1.0`, so the existing `<= 0.0` readiness guard withholds quoting until a real cross tick arrives. Previously a primary tick landing before the first cross tick produced `primary / 1.0` — a price off by the entire cross rate.
- **Delegated signing for `run`.** The market maker can now sign quote updates with a delegate keypair while quoting on an owner's book, so the owner (master) private key never has to live on the trading machine. Two new optional `[market]` fields: `delegate_keypair_path` (when set, `run` signs with it instead of the owner key) and `maker_owner_pubkey` (used to derive the maker book PDA when `maker_keypair_path` is left empty). The engine already separated the signer from the book owner; this wires the config through to it. Owner-only commands (`init`/`deposit`/`withdraw`/`set-delegate`) are unchanged and still require `maker_keypair_path`. Pair with the existing `set-delegate` command to authorize the delegate on-chain first.

## [2026-06-12]

### Added
- **Limit-order (LO) book support.** `init --kind lo` creates an LO `MakerBook` (the program's new init `kind` byte; `mm` remains the default). `MakerBook` now decodes the `kind` field carved from the old status padding. The engine is LO-aware: LO books never send `UpdateMidPrice` (their mid is pinned to 0) and re-quote at absolute price ticks on every move, while MM books keep the cheap mid-shift path.
- **`set-delegate` CLI command.** Wires the existing `SetBookDelegate` builder; pass `--delegate <pubkey>` to set, or omit / `--delegate clear` to remove.
- **Live fill + inventory subscriptions (`fills.rs`).** Over the RPC websocket, `run` now (1) `account_subscribe`s to the maker book to keep inventory (`base/quote_total_lots`, `mid`, sequence) exact in real time instead of only at startup, and (2) `logs_subscribe`s with a `mentions` filter to decode `MakerFillEvent`s (disc `[60,14,66,1,…]`) for per-fill logging and counters. Optional `[connection].ws_url` override; otherwise derived from `rpc_url`.
- **Registry awareness.** `run` and `status` check the market's `MakerRegistry` PDA and warn when the book is not registered (the aggregator may skip unregistered quotes).
- `status` now prints book kind, status, registration, delegate, sync spread, and expiry slots.

## [2026-04-17]

### Added
- `set-expiry` CLI command. Calls the on-chain `UpdateExpiryInSlots` instruction (discriminator `30`) to set `MakerBook.expiry_in_slots`. `--slots 0` disables the aggregator's expiry-skip check.
- `MakerBook` now decodes the new trailing fields `last_updated_slot`, `expiry_in_slots`, and reserved padding added by the on-chain layout resize.

### Changed
- Maker deposit/withdraw instructions now pass `market_account` as readonly, matching the on-chain program's updated account requirements.
- Bumped compute-unit limits: `UpdateMidPrice` 750 → 850, `UpdateBook` 5500 → 5600.
- README CU table updated to reflect the real per-instruction budgets used by the engine (`~180` / `~400` / `~5,000`).
