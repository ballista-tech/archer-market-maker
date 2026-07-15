# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [2026-07-15]

### Added
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
