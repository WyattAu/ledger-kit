//! Double-entry ledger for Rust — decimal-money postings, outbox-backed
//! durable journal, idempotent posting, tamper-evident history.
//!
//! `ledger-kit` composes the estate's primitives into one audited posting
//! path. Amounts are [`decimal-money`](decimal_money) [`CurrencyAmount`]s
//! (re-exported as [`MonetaryAmount`]); posting is idempotent through
//! idempotency-kit claims; every accepted posting is appended to a
//! [`Journal`] — the default in-process [`MemoryJournal`], or wrap it in
//! [`OutboxJournal`] to mirror accepted postings into an outbox-kit store
//! for durability across restarts — and, under the `audit` feature, chained
//! into a tamper-evident audit log.
//!
//! # The two laws
//!
//! 1. **Balances are derived, never stored.** [`balance_of`] folds the
//!    journal (credit adds, debit subtracts); there is no mutable balance
//!    field, so there is nothing to drift or corrupt. The derivation is
//!    the source of truth.
//! 2. **The sign lives in the sides, not the amount.** A [`Posting`] moves
//!    a strictly positive amount *from* [`Posting::debit`] *to*
//!    [`Posting::credit`]; reversals are new postings in the other
//!    direction.
//!
//! # Example
//!
//! ```
//! use decimal_money::Currency;
//! use ledger_kit::{AccountId, Ledger, MonetaryAmount};
//!
//! # async fn demo() -> Result<(), ledger_kit::LedgerError> {
//! let ledger = Ledger::new();
//! ledger.open_account(AccountId::new("cash-usd")?, Currency::USD)?;
//! ledger.open_account(AccountId::new("revenue")?, Currency::USD)?;
//!
//! // Idempotent: retry the same caller key and nothing double-posts —
//! // the original posting's id comes back as `DuplicatePosting`.
//! let posting = ledger
//!     .post(
//!         &AccountId::new("cash-usd")?,
//!         &AccountId::new("revenue")?,
//!         MonetaryAmount::new(42_50, Currency::USD),
//!         "sale #42",
//!         "order-42",
//!     )
//!     .await?;
//!
//! // Balances are derived from the journal, never stored:
//! assert_eq!(
//!     ledger
//!         .balance_of(&AccountId::new("cash-usd")?)
//!         .await?
//!         .amount
//!         .to_string(),
//!     "-42.50"
//! );
//! # let _ = posting;
//! # Ok(())
//! # }
//! ```
//!
//! # Negative balances
//!
//! [`BalancePolicy::AllowNegative`] (the default) leaves credit decisions
//! to the domain; [`BalancePolicy::RejectNegative`] checks each account's
//! *projected* balance (derived + this posting's effect) and answers
//! [`LedgerError::InsufficientFunds`] before anything is recorded.
//!
//! # Tamper evidence
//!
//! Under the `audit` feature (default on), every accepted posting is
//! chained into a `tamper-audit` log — previous-hash → entry-hash, the
//! posting captured verbatim in the entry's details — and
//! [`Ledger::verify_chain`] re-verifies the whole chain.
//!
//! # Durability
//!
//! The default journal is in-process. For durability, wrap it in an
//! [`OutboxJournal`] backed by an outbox-kit store (swap in their `sqlite`
//! store for durability across restarts): accepted postings are mirrored
//! as event envelopes and [`OutboxJournal::restore`] replays them into a
//! fresh journal after a crash — at-least-once, idempotent on the posting
//! id.
//!
//! # Roadmap
//!
//! - `sqlite` journal feature: an outbox-kit-style durable `Journal`
//!   (WAL, embedded schema), same trait, same ordering.
//! - Pluggable idempotency stores beyond the injectable
//!   [`LedgerBuilder::idempotency_store`] (the kit reads
//!   `Arc<dyn IdempotencyStore>`, so idempotency-kit's Redis store already
//!   drops in).
//! - Audit export/replay tooling for the chain.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod account;
pub mod balance;
pub mod error;
pub mod journal;
pub mod ledger;
pub mod memory;
pub mod posting;

#[cfg(feature = "outbox")]
pub mod outbox;

pub use account::{Account, AccountId};
pub use balance::{balance_of, AccountBalance};
pub use decimal_money::{self, Currency, CurrencyAmount as MonetaryAmount};
pub use error::LedgerError;
pub use journal::Journal;
pub use ledger::{BalancePolicy, Ledger, LedgerBuilder};
pub use memory::MemoryJournal;
pub use posting::{amount, Posting, PostingId};

#[cfg(feature = "outbox")]
pub use outbox::OutboxJournal;

/// Current unix time in milliseconds (wall clock) — the kit's shared clock
/// for `recorded_at`. Returns `0` if the host clock is before the epoch (a
/// broken clock is a deployment problem; the kit stays total).
///
/// The same wall-clock convention outbox-kit uses for envelope timestamps,
/// so a posting's `recorded_at` and its outbox envelope's `created_at`
/// agree to the millisecond.
#[must_use]
pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn now_millis_is_total_and_monotone_enough() {
        let a = now_millis();
        let b = now_millis();
        assert!(a <= b, "wall clock must not go backwards: {a} > {b}");
    }
}
