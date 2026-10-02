//! The posting: [`PostingId`] and [`Posting`].

use std::fmt;

use decimal_money::CurrencyAmount;
use idempotency_kit::IdempotencyKey;
use uuid::Uuid;

use crate::account::AccountId;
use crate::error::LedgerError;
use crate::MonetaryAmount;

/// The identifier of a posting: a `UUIDv7`, time-ordered so ids sort
/// roughly by creation time.
///
/// Time-ordered ids mean `ORDER BY id` is a stable tiebreak for
/// `recorded_at` collisions, and pagination by id approximates
/// chronological order without a separate sequence. Mirrors outbox-kit's
/// `EventId` so journal ids and outbox envelope ids can be the same value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PostingId(Uuid);

impl PostingId {
    /// Generate a fresh id from the wall clock (`UUIDv7`).
    #[must_use]
    pub fn now_v7() -> Self {
        Self(Uuid::now_v7())
    }

    /// Wrap an existing UUID — import an id minted elsewhere (a migration,
    /// a posting reconstructed from an outbox replay).
    #[must_use]
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// The inner UUID.
    #[must_use]
    pub fn as_uuid(self) -> Uuid {
        self.0
    }

    /// Parse the hyphenated string form (as produced by [`Display`]).
    ///
    /// # Errors
    ///
    /// [`LedgerError::InvalidPosting`] unless `input` is a valid
    /// hyphenated (or simple/braced — anything [`Uuid::parse_str`]
    /// accepts) UUID string.
    pub fn parse(input: &str) -> Result<Self, LedgerError> {
        Uuid::parse_str(input)
            .map(Self)
            .map_err(|_| LedgerError::InvalidPosting {
                reason: format!("invalid posting id {input:?}: expected a UUID string"),
            })
    }
}

impl fmt::Display for PostingId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Hyphenated lower-case — the canonical transport and storage form.
        write!(f, "{}", self.0.hyphenated())
    }
}

/// A double-entry posting: move `amount` from [`Posting::debit`] to
/// [`Posting::credit`].
///
/// Construct with [`Posting::new`], which enforces the invariants as a
/// typed error; nothing else can build a `Posting` (fields are private),
/// so every `Posting` value in existence satisfies:
///
/// - `debit != credit` — a posting never nets to nothing;
/// - `amount > 0` — the sign lives in the debit/credit sides, never in
///   the amount; a reversal is a new posting in the other direction.
///
/// Currency consistency (`debit`, `credit`, and the amount carry the same
/// registered currency) is enforced where currencies are known:
/// [`Ledger::post`](crate::Ledger::post) checks both accounts' registry
/// entries against the amount's currency before anything is recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Posting {
    /// The posting id (`UUIDv7`, time-ordered).
    id: PostingId,
    /// The account that is debited (its derived balance decreases).
    debit: AccountId,
    /// The account that is credited (its derived balance increases).
    credit: AccountId,
    /// The strictly positive amount moved.
    amount: MonetaryAmount,
    /// Free-form human-readable memo.
    memo: String,
    /// The estate idempotency key this posting was accepted under.
    idempotency_key: IdempotencyKey,
    /// The raw caller key the idempotency key was derived from, when the
    /// ledger minted it ([`Ledger::post`](crate::Ledger::post)). Carried so
    /// the outbox codec can re-derive the key bit-identically on restore —
    /// `IdempotencyKey` is one-way (BLAKE3) and cannot be rebuilt from its
    /// rendered form.
    caller_key: Option<String>,
    /// Unix millis at acceptance (wall clock).
    recorded_at: u64,
}

impl Posting {
    /// Creates a validated posting.
    ///
    /// # Errors
    ///
    /// [`LedgerError::InvalidPosting`] when `debit == credit` or
    /// `amount` is not strictly positive.
    pub fn new(
        debit: AccountId,
        credit: AccountId,
        amount: MonetaryAmount,
        memo: impl Into<String>,
        idempotency_key: IdempotencyKey,
        recorded_at: u64,
    ) -> Result<Self, LedgerError> {
        if debit == credit {
            return Err(LedgerError::InvalidPosting {
                reason: format!("debit and credit are the same account ({debit})"),
            });
        }
        if !amount.is_positive() {
            return Err(LedgerError::InvalidPosting {
                reason: format!(
                    "amount must be strictly positive, got {} (sign belongs to the sides)",
                    amount.amount
                ),
            });
        }
        Ok(Self {
            id: PostingId::now_v7(),
            debit,
            credit,
            amount,
            memo: memo.into(),
            idempotency_key,
            caller_key: None,
            recorded_at,
        })
    }

    /// Rebuilds a posting under an id minted elsewhere — the decode path
    /// for journal restore/replay, and migrations importing a legacy
    /// history.
    ///
    /// The same invariants as [`Posting::new`] are enforced; the id is
    /// adopted, not minted, so a restored posting is byte-identical to
    /// the one that was accepted (and journal id-idempotency deduplicates
    /// the replay).
    ///
    /// # Errors
    ///
    /// [`LedgerError::InvalidPosting`] when `debit == credit` or `amount`
    /// is not strictly positive.
    pub fn rebuild(
        id: PostingId,
        debit: AccountId,
        credit: AccountId,
        amount: MonetaryAmount,
        memo: impl Into<String>,
        idempotency_key: IdempotencyKey,
        recorded_at: u64,
    ) -> Result<Self, LedgerError> {
        if debit == credit {
            return Err(LedgerError::InvalidPosting {
                reason: format!("debit and credit are the same account ({debit})"),
            });
        }
        if !amount.is_positive() {
            return Err(LedgerError::InvalidPosting {
                reason: format!(
                    "amount must be strictly positive, got {} (sign belongs to the sides)",
                    amount.amount
                ),
            });
        }
        Ok(Self {
            id,
            debit,
            credit,
            amount,
            memo: memo.into(),
            idempotency_key,
            caller_key: None,
            recorded_at,
        })
    }

    /// Attaches the raw caller key an idempotency key was derived from.
    ///
    /// [`Ledger::post`](crate::Ledger::post) always attaches it; direct
    /// construction ([`Posting::new`] / [`Posting::rebuild`]) leaves it
    /// `None`. Postings without it cannot round-trip the outbox codec
    /// (see [`decode_posting`](crate::outbox::decode_posting)) — their
    /// key cannot be re-derived from its rendered form.
    #[must_use]
    pub fn with_caller_key(mut self, raw: impl Into<String>) -> Self {
        self.caller_key = Some(raw.into());
        self
    }

    /// The raw caller key, when the ledger minted the idempotency key.
    #[must_use]
    pub fn caller_key(&self) -> Option<&str> {
        self.caller_key.as_deref()
    }

    /// The posting id.
    #[must_use]
    pub fn id(&self) -> PostingId {
        self.id
    }

    /// The debited account (balance decreases by the amount).
    #[must_use]
    pub fn debit(&self) -> &AccountId {
        &self.debit
    }

    /// The credited account (balance increases by the amount).
    #[must_use]
    pub fn credit(&self) -> &AccountId {
        &self.credit
    }

    /// The amount moved (always strictly positive).
    pub const fn amount(&self) -> &MonetaryAmount {
        &self.amount
    }

    /// The memo.
    #[must_use]
    pub fn memo(&self) -> &str {
        &self.memo
    }

    /// The idempotency key the posting was accepted under.
    #[must_use]
    pub const fn idempotency_key(&self) -> &IdempotencyKey {
        &self.idempotency_key
    }

    /// Unix millis at acceptance.
    #[must_use]
    pub const fn recorded_at(&self) -> u64 {
        self.recorded_at
    }
}

/// Convenience: build a [`CurrencyAmount`] (the crate's
/// [`MonetaryAmount`]) in one call.
///
/// # Errors
///
/// [`LedgerError::InvalidPosting`] when `amount` does not parse as a
/// decimal (e.g. `"12.50"`).
pub fn amount(
    amount: &str,
    currency: decimal_money::Currency,
) -> Result<MonetaryAmount, LedgerError> {
    CurrencyAmount::from_str_values(amount, currency).map_err(|e| LedgerError::InvalidPosting {
        reason: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use decimal_money::Currency;

    fn key(n: u8) -> IdempotencyKey {
        IdempotencyKey::derive("ledger-kit.test", &[n]).expect("scope is valid")
    }

    fn usd(cents: i64) -> MonetaryAmount {
        MonetaryAmount::new(cents, Currency::USD)
    }

    #[test]
    fn posting_id_newtypes() {
        let id = PostingId::now_v7();
        let text = id.to_string();
        assert_eq!(PostingId::parse(&text).unwrap(), id);
        assert_eq!(id.as_uuid(), Uuid::parse_str(&text).unwrap());
        let wrapped = PostingId::from_uuid(Uuid::from_u128(42));
        assert_eq!(wrapped.as_uuid(), Uuid::from_u128(42));
        assert_eq!(
            PostingId::parse("not-a-uuid").err(),
            Some(LedgerError::InvalidPosting {
                reason: "invalid posting id \"not-a-uuid\": expected a UUID string".to_owned()
            })
        );
    }

    #[test]
    fn valid_posting_holds_and_reads() {
        let debit = AccountId::new("cash-usd").unwrap();
        let credit = AccountId::new("revenue").unwrap();
        let p = Posting::new(
            debit.clone(),
            credit.clone(),
            usd(1_250),
            "coffee sale",
            key(1),
            1_700_000_000_000,
        )
        .unwrap();
        assert_eq!(p.debit(), &debit);
        assert_eq!(p.credit(), &credit);
        assert_eq!(p.amount(), &usd(1_250));
        assert_eq!(p.memo(), "coffee sale");
        assert_eq!(p.recorded_at(), 1_700_000_000_000);
        assert_eq!(p.idempotency_key(), &key(1));
    }

    #[test]
    fn debit_must_differ_from_credit() {
        let same = AccountId::new("cash-usd").unwrap();
        let err = Posting::new(same.clone(), same, usd(1), "", key(2), 0).unwrap_err();
        assert!(matches!(err, LedgerError::InvalidPosting { .. }));
        assert!(err.to_string().contains("same account"));
    }

    #[test]
    fn zero_and_negative_amounts_reject() {
        let debit = AccountId::new("cash-usd").unwrap();
        let credit = AccountId::new("revenue").unwrap();
        for bad in [0, -5] {
            let err =
                Posting::new(debit.clone(), credit.clone(), usd(bad), "", key(3), 0).unwrap_err();
            assert!(
                err.to_string().contains("strictly positive"),
                "{bad} should be rejected: {err}"
            );
        }
    }

    #[test]
    fn amount_helper_parses_and_rejects() {
        let parsed = amount("19.99", Currency::USD).unwrap();
        assert_eq!(parsed.currency, Currency::USD);
        assert!(parsed.is_positive());
        let err = amount("not-a-decimal", Currency::USD).unwrap_err();
        assert!(matches!(err, LedgerError::InvalidPosting { .. }));
    }
}
