# Changelog

All notable changes to this project are documented here. Format: [Keep a
Changelog](https://keepachangelog.com/) — versions follow [semver](https://semver.org).

## [0.1.1] - 2026-10-05

### Changed

- `OutboxJournal` now builds on `outbox-kit` **0.2**, up from `^0.1`. The
  0.2 release replaced `DispatcherConfig::batch_size` with an adaptive
  `FetchBatch { min, max, park_after }` and added a dead-letter view
  (`dead_letter`, `dead_letters`) to `OutboxStore`. The `OutboxStore` methods
  this crate uses (`append`, `fetch_due`, `mark_dispatched`, `mark_failed`,
  `pending_count`, `parked_count`) are unchanged, so no source change was
  needed.

  This was the blocker holding `estate-integration`'s outbox-kit pin at 0.1.0
  (recorded in engineering-standards `estate.yml` under `pins_held`): with
  ledger-kit on 0.1 and outbox-kit's newer release at 0.2, any graph
  containing both majors failed with E0308 on the `OutboxStore` trait object.
  The `restore` path — the durable journal an accounting product depends on —
  now runs on the current outbox release.

## [0.1.0] - 2026-10-02

### Added

- `Posting`: the double-entry unit — `PostingId` (`UUIDv7`, time-ordered,
  `Ord`-stable tiebreaks), validated `debit`/`credit` accounts, a strictly
  positive `MonetaryAmount` (the sign lives in the sides, never the
  amount), memo, estate idempotency key, and `recorded_at` (unix millis).
  The constructor enforces debit ≠ credit and amount > 0 as a typed
  error; `rebuild` adopts an id minted elsewhere (restore/replay,
  migrations) under the same invariants.
- `AccountId`: validated `[a-z0-9_.-]{1,64}` typed newtype (the estate's
  shared identifier charset, as in idempotency-kit's key scopes);
  `Account` pairs it with its single `Currency`.
- Balances are **derived, never stored**: `balance_of` folds the posting
  history (credit adds, debit subtracts) and is the only balance source —
  there is no mutable balance field to drift, corrupt, or reconcile.
  `Ledger::balance_of` / `Ledger::balance` /
  `Ledger::trial_balance` project the derivation over the registry;
  `AccountBalance` is the derived snapshot type.
- `Journal` trait (object-safe via `async_trait`, outbox-kit's store-trait
  conventions): idempotent `append` (INSERT-OR-IGNORE on posting id),
  `postings_for` / `all_postings` ordered by `(recorded_at, id)`, and
  `posting_by_id`. `MemoryJournal` (default) is a `BTreeMap` keyed by
  `(recorded_at, id)` with poisoning recovered, not propagated.
- `Ledger::post`: the acceptance pipeline — idempotency claims
  (caller-key claim + canonical-request fingerprint via idempotency-kit's
  `MemoryStore`, injectable), registry/invariant checks (both accounts
  registered in the amount's currency, which enforces debit/credit
  same-currency), balance policy, journal append, audit chain append,
  claim completion. Replayed keys answer `DuplicatePosting` with the
  original posting's id; a key reused with a *different* request is a
  typed `InvalidPosting` conflict; failed attempts release their claims so
  corrected retries proceed.
- `BalancePolicy`: `AllowNegative` (default) or `RejectNegative` — the
  debited account's *projected* balance may not go negative
  (`InsufficientFunds { account, attempted }`), checked against the
  derivation, never a stored number. Debit-side by construction: under
  conservation (total ≡ 0) a both-sides rule could accept no posting at
  all.
- `audit` feature (default on): every accepted posting is chained into a
  `tamper-audit` log — previous-hash → entry-hash, the posting captured
  verbatim in the entry details. `Ledger::verify_chain` re-verifies the
  whole chain (`ChainBroken { at }`); `Ledger::audit_entries` exposes it.
- `outbox` feature (default on): `OutboxJournal` mirrors accepted postings
  into an outbox-kit `OutboxStore` (envelope id = posting id, deterministic
  compact codec, outbox-first write order) and `restore` replays them into
  a fresh journal after a crash — at-least-once, idempotent on both sides,
  re-validated on decode.
- `LedgerError`: documented, `#[non_exhaustive]` — `InvalidAccount`,
  `InvalidPosting { reason }`, `InsufficientFunds { account, attempted }`,
  `DuplicatePosting { id }`, `ChainBroken { at }`, `Store(String)`.
- `#![forbid(unsafe_code)]`, `#![deny(missing_docs)]`, clippy
  `unwrap_used`/`expect_used`/`panic`/`indexing_slicing` denied.
