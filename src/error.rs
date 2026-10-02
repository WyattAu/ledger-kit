//! The error surface: [`LedgerError`].
//!
//! Documented failure modes are part of the API contract; every variant
//! lists the condition that produces it.

use crate::account::AccountId;
use crate::posting::PostingId;
use crate::MonetaryAmount;

/// A failure of a ledger operation.
///
/// The crate fails closed: nothing is appended to the journal, mirrored
/// into the outbox, or chained into the audit log unless every
/// double-entry invariant and balance-policy check passed first.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LedgerError {
    /// The account is unknown or invalid: it was never opened with
    /// [`Ledger::open_account`](crate::Ledger::open_account), the
    /// identifier fails the `[a-z0-9_.-]{1,64}` charset, or (for
    /// `open_account`) the identifier is already registered. The rejected
    /// identifier travels in the operation, not in this variant.
    #[error("invalid or unknown ledger account")]
    InvalidAccount,
    /// A double-entry invariant was violated. Produced by
    /// [`Posting::new`](crate::Posting::new) (debit equals credit, or the
    /// amount is not strictly positive) and by
    /// [`Ledger::post`](crate::Ledger::post) (either account's registered
    /// currency differs from the amount's currency; a derived balance is
    /// not currency-consistent; or an idempotency key was reused with a
    /// different request). The violation reason is included verbatim.
    #[error("invalid posting: {reason}")]
    InvalidPosting {
        /// The violated invariant, as a human-readable diagnostic.
        reason: String,
    },
    /// [`BalancePolicy::RejectNegative`]
    /// rejected the posting: the **debited account's** derived balance
    /// would go negative. Nothing was recorded. `attempted` is the
    /// posting amount that was attempted (not the shortfall); subtract it
    /// from the current derived balance to see the overdraw.
    #[error("insufficient funds: {account} would go negative attempting {attempted}")]
    InsufficientFunds {
        /// The account whose projected balance would go negative.
        account: AccountId,
        /// The attempted posting amount.
        attempted: MonetaryAmount,
    },
    /// The idempotency claim replayed a posting that was already accepted:
    /// the caller key was seen before within its TTL window. `id` is the
    /// id of the existing posting — nothing was double-posted. Treat this
    /// as confirmation the original request landed (or look the posting up
    /// with [`Journal::posting_by_id`](crate::Journal::posting_by_id)).
    #[error("duplicate posting {id}: idempotency key already accepted")]
    DuplicatePosting {
        /// The id of the already-accepted posting.
        id: PostingId,
    },
    /// The audit chain failed verification at entry `at` (0-based index
    /// into the chain, including the genesis entry). Produced only by
    /// [`Ledger::verify_chain`](crate::Ledger::verify_chain) under the
    /// `audit` feature; the crate never writes to a chain it just proved
    /// broken.
    #[error("audit chain broken at entry {at}")]
    ChainBroken {
        /// 0-based index of the first broken entry.
        at: usize,
    },
    /// A backend (`journal`, idempotency store, audit store, outbox codec)
    /// failed. The `String` is the backend's own diagnostic; the kit never
    /// embeds posting data in it. The crate's state is unchanged for a
    /// failed append; retry the operation.
    #[error("ledger store backend failure: {0}")]
    Store(String),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn account(name: &str) -> AccountId {
        AccountId::new(name).unwrap()
    }

    #[test]
    fn invalid_account_displays_and_compares() {
        let err = LedgerError::InvalidAccount;
        assert_eq!(err, LedgerError::InvalidAccount);
        assert_eq!(err.to_string(), "invalid or unknown ledger account");
    }

    #[test]
    fn invalid_posting_carries_reason() {
        let err = LedgerError::InvalidPosting {
            reason: "debit equals credit".to_owned(),
        };
        assert!(err.to_string().contains("debit equals credit"));
        assert_eq!(
            err,
            LedgerError::InvalidPosting {
                reason: "debit equals credit".to_owned()
            }
        );
    }

    #[test]
    fn insufficient_funds_names_account_and_attempt() {
        let err = LedgerError::InsufficientFunds {
            account: account("cash-usd"),
            attempted: MonetaryAmount::new(5, crate::Currency::USD),
        };
        let text = err.to_string();
        assert!(text.contains("cash-usd"));
        assert!(text.contains('5'));
    }

    #[test]
    fn duplicate_and_chain_errors_display_ids() {
        let dup = LedgerError::DuplicatePosting {
            id: PostingId::now_v7(),
        };
        assert!(dup.to_string().contains("duplicate posting"));
        let chain = LedgerError::ChainBroken { at: 3 };
        assert_eq!(chain.to_string(), "audit chain broken at entry 3");
    }

    #[test]
    fn store_error_displays_diagnostic() {
        let err = LedgerError::Store("disk full".to_owned());
        assert!(err.to_string().contains("disk full"));
    }
}
