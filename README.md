# ledger-kit

Double-entry ledger for Rust — decimal-money postings, outbox-backed
durable journal, idempotent posting, tamper-evident history.

- **Balances are derived, never stored.** A balance is a pure fold over
  the posting history — credit adds, debit subtracts. There is no mutable
  balance field, so there is nothing to drift, corrupt, or reconcile: the
  derivation is the source of truth.
- **The sign lives in the sides.** A posting moves a strictly positive
  `MonetaryAmount` (decimal-money's `CurrencyAmount` — exact decimal
  arithmetic, no floats) *from* the debit account *to* the credit
  account; reversals are new postings in the other direction. The
  constructor enforces debit ≠ credit and amount > 0 as a typed error;
  `Ledger::post` additionally checks both accounts against their
  registered currency.
- **Posting is idempotent** through
  [`idempotency-kit`](https://crates.io/crates/idempotency-kit) claims: a
  replayed caller key answers `DuplicatePosting` with the original
  posting's id instead of double-posting, and a key reused with a
  *different* request is a typed conflict (the classic idempotency-key
  trap, caught).
- **Durable by outbox.** `OutboxJournal` mirrors every accepted posting
  into an [`outbox-kit`](https://crates.io/crates/outbox-kit) store
  (outbox-first write order, envelope id = posting id) and replays them
  into a fresh journal after a crash — at-least-once, idempotent on both
  sides, re-validated on decode. Swap in their `sqlite` store for
  durability across restarts.
- **Tamper-evident history.** Under the `audit` feature, every accepted
  posting is chained into a
  [`tamper-audit`](https://crates.io/crates/tamper-audit) log
  (previous-hash → entry-hash, the posting captured verbatim in entry
  details); `Ledger::verify_chain` re-verifies the whole chain and a
  flipped memo byte anywhere breaks it.
- **`#![forbid(unsafe_code)]`, `#![deny(missing_docs)]`**, clippy
  `unwrap_used`/`expect_used`/`panic`/`indexing_slicing` denied.

## Install

```toml
[dependencies]
ledger-kit = "0.1"
```

## Example

```rust
use decimal_money::Currency;
use ledger_kit::{AccountId, Ledger, MonetaryAmount};

# async fn demo() -> Result<(), ledger_kit::LedgerError> {
let ledger = Ledger::new();
ledger.open_account(AccountId::new("cash-usd")?, Currency::USD)?;
ledger.open_account(AccountId::new("revenue")?, Currency::USD)?;

// Idempotent: retrying the same caller key answers `DuplicatePosting`
// with the original posting's id — nothing double-posts.
let posting = ledger
    .post(
        &AccountId::new("cash-usd")?,
        &AccountId::new("revenue")?,
        MonetaryAmount::new(42_50, Currency::USD),
        "sale #42",
        "order-42",
    )
    .await?;

// Balances are derived from the journal, never stored:
assert_eq!(
    ledger
        .balance_of(&AccountId::new("cash-usd")?)
        .await?
        .amount
        .to_string(),
    "-42.50"
);
# let _ = posting;
# Ok(())
# }
# # let _ = demo();
```

## The two laws

1. **Balances are derived, never stored.** `balance_of(account, postings)`
   is the only balance computation in the crate; the ledger methods are
   thin projections of it over the journal. `AccountBalance` is a derived
   snapshot type — nothing keeps one between calls.
2. **The sign lives in the sides, not the amount.** Every posting moves a
   positive amount; the sides carry the direction. This is what makes
   `trial_balance` provably net to zero over any posting set — the crate's
   proptest suite (500-case balanced sets) enforces it continuously.

## Negative balances

`BalancePolicy::AllowNegative` (default) leaves credit decisions to the
domain. `BalancePolicy::RejectNegative` checks the **debited** account's
*projected* balance (derived + this posting's effect) and answers
`InsufficientFunds { account, attempted }` before anything is recorded —
and the failed claim is released, so a corrected retry with the same key
proceeds. The check is debit-side by construction: under double-entry
conservation the total of all balances is always zero, so a both-sides
rule could never accept any posting at all (funding accounts carry the
negative side by design).

## API tour

| Piece | Role |
|-------|------|
| `AccountId` | validated `[a-z0-9_.-]{1,64}` newtype (estate identifier charset) |
| `Account` | registered id + single `Currency` (via `Ledger::open_account`) |
| `Posting` | the validated double-entry unit; `PostingId` is a time-ordered UUIDv7 |
| `Journal` | object-safe async store trait; `MemoryJournal` is the default |
| `OutboxJournal` | `Journal` that mirrors into an outbox-kit store (`outbox` feature) |
| `balance_of` | the pure derivation: signed fold over postings |
| `Ledger` | the facade: `post`, `balance_of`, `balance`, `trial_balance`, `verify_chain` |
| `BalancePolicy` | `AllowNegative` / `RejectNegative` |
| `LedgerError` | `InvalidAccount`, `InvalidPosting`, `InsufficientFunds`, `DuplicatePosting`, `ChainBroken`, `Store` |

## Features

| Feature | Default | Effect |
|---------|---------|--------|
| `audit`  | ✅ | tamper-audit chain per accepted posting; `verify_chain` / `audit_entries` |
| `outbox` | ✅ | `OutboxJournal` + compact deterministic codec on outbox-kit |

```toml
# Core only — no audit chain, no outbox mirror:
ledger-kit = { version = "0.1", default-features = false }
```

## Honest notes

- `MonetaryAmount` is a re-export of decimal-money's `CurrencyAmount` —
  one amount type across the estate, no wrapper drift.
- The `audit` chain anchors each posting *as recorded at acceptance*; it
  proves the audit record was not edited, not that a hostile writer
  cannot rewrite history wholesale. Anchor chain heads externally for
  stronger guarantees.
- Outbox replay is at-least-once: restore re-appends idempotently by
  posting id, but consumers downstream of the mirrored events must be
  idempotent too.
- A native `sqlite` `Journal` (outbox-kit-style: WAL, embedded schema) is
  the durable-store roadmap's next step; `OutboxJournal` over their
  `sqlite` store is today's durability path.

## Estate layer

`ledger-kit` is an **L2 domain** crate in the WyattAu estate: it composes
L0/L2 primitives (`decimal-money` L0, `idempotency-kit` / `outbox-kit` /
`tamper-audit` L2) and must never be depended on by them. CI enforces the
layer model on every push.

## License

MIT OR Apache-2.0 — see [LICENSE-MIT](LICENSE-MIT) and
[LICENSE-APACHE](LICENSE-APACHE).
