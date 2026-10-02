//! Accounts: [`AccountId`] and [`Account`].

use std::fmt;

use decimal_money::Currency;

use crate::error::LedgerError;

/// Accounts must match `[a-z0-9_.-]{1,64}`.
///
/// The charset is the estate's shared identifier pattern (the same shape
/// as idempotency-kit's key scopes and outbox-kit's topics): safe as URL
/// path segments, map keys, and ledger column values without escaping.
pub(crate) fn validate_account_id(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-'))
}

/// The identity of a ledger account: a validated newtype.
///
/// Identifiers are constrained to `[a-z0-9_.-]{1,64}` — lowercase ASCII
/// alphanumerics plus `_`, `.`, `-`. Construct with [`AccountId::new`]
/// (validated, [`LedgerError::InvalidAccount`] on rejection) or
/// [`Account::id`](crate::Account) helpers; [`FromStr`] parses the same
/// charset. Derives [`Ord`] so account maps and trial balances render in
/// one deterministic order everywhere.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AccountId {
    /// The validated identifier; also the [`Display`] form.
    value: String,
}

impl AccountId {
    /// Creates a validated account id.
    ///
    /// # Errors
    ///
    /// [`LedgerError::InvalidAccount`] unless `value` matches
    /// `[a-z0-9_.-]{1,64}`.
    pub fn new(value: impl Into<String>) -> Result<Self, LedgerError> {
        let value = value.into();
        if validate_account_id(&value) {
            Ok(Self { value })
        } else {
            Err(LedgerError::InvalidAccount)
        }
    }

    /// The identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.value
    }

    /// Consumes the id, returning the inner `String`.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.value
    }
}

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.value)
    }
}

impl std::str::FromStr for AccountId {
    type Err = LedgerError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s.to_owned())
    }
}

/// A registered ledger account: an [`AccountId`] plus its single
/// [`Currency`].
///
/// Deliberately **not** a balance: balances are derived from postings
/// (see [`balance_of`](crate::balance_of)) and never stored — a stored
/// mutable balance would be a second source of truth and the one thing a
/// ledger must not have. The derived snapshot type is
/// [`AccountBalance`](crate::AccountBalance).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Account {
    /// The account identity.
    pub id: AccountId,
    /// The single currency the account carries; every posting touching
    /// this account must carry the same currency.
    pub currency: Currency,
}

impl Account {
    /// Registers an account, validating the id.
    ///
    /// # Errors
    ///
    /// [`LedgerError::InvalidAccount`] unless `id` matches
    /// `[a-z0-9_.-]{1,64}`.
    pub fn new(id: impl Into<String>, currency: Currency) -> Result<Self, LedgerError> {
        Ok(Self {
            id: AccountId::new(id)?,
            currency,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn valid_ids_accept() {
        for ok in ["a", "cash", "cash_usd", "user-42.acct", "0", "x-y.z_9"] {
            AccountId::new(ok).unwrap();
        }
        let long = "a".repeat(64);
        AccountId::new(long.as_str()).unwrap();
    }

    #[test]
    fn invalid_ids_reject() {
        for bad in [
            "",
            "UPPER",
            "has space",
            "has/slash",
            "emoji😀",
            "ünïcode",
            "semi;colon",
        ] {
            assert_eq!(
                AccountId::new(bad).err(),
                Some(LedgerError::InvalidAccount),
                "'{bad}' should be invalid"
            );
        }
        let long = "a".repeat(65);
        assert_eq!(
            AccountId::new(long).err(),
            Some(LedgerError::InvalidAccount)
        );
    }

    #[test]
    fn display_fromstr_and_accessors_roundtrip() {
        let id = AccountId::new("cash-usd").unwrap();
        assert_eq!(id.as_str(), "cash-usd");
        assert_eq!(id.to_string(), "cash-usd");
        let parsed: AccountId = "cash-usd".parse().unwrap();
        assert_eq!(parsed, id);
        assert_eq!(parsed.into_inner(), "cash-usd");
    }

    #[test]
    fn ordering_is_lexicographic_and_deterministic() {
        let a = AccountId::new("acct-a").unwrap();
        let b = AccountId::new("acct-b").unwrap();
        assert!(a < b);
        let mut ids = vec![b.clone(), a.clone()];
        ids.sort();
        assert_eq!(ids, vec![a, b]);
    }

    #[test]
    fn account_new_validates_and_carries_currency() {
        let acct = Account::new("cash-eur", Currency::EUR).unwrap();
        assert_eq!(acct.id.as_str(), "cash-eur");
        assert_eq!(acct.currency, Currency::EUR);
        assert_eq!(
            Account::new("bad id!", Currency::USD).err(),
            Some(LedgerError::InvalidAccount)
        );
    }
}
