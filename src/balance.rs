//! Balance derivation: [`balance_of`] and [`AccountBalance`].
//!
//! Balances are **never stored**. They are a pure fold over the posting
//! history: credit side adds, debit side subtracts. The derivation is the
//! source of truth — there is no second copy to drift, corrupt, or
//! reconcile, and any two observers deriving from the same journal get the
//! same number bit-for-bit.

use decimal_money::CurrencyAmount;

use crate::account::{Account, AccountId};
use crate::error::LedgerError;
use crate::posting::Posting;
use crate::MonetaryAmount;

/// Derives the balance of `account` from a posting slice: the signed sum
/// of every posting where the account is the credit side (adds) or the
/// debit side (subtracts).
///
/// A posting that does not touch `account` contributes nothing. Postings
/// touching `account` must share one currency — decimal-money's
/// [`CurrencyAmount`] arithmetic is currency-checked and a mixed-currency
/// history is surfaced, not silently summed.
///
/// # Errors
///
/// - [`LedgerError::InvalidAccount`] when no posting touches `account`
///   (the currency of an all-zero balance would be undeterminable — the
///   registry-backed [`Ledger::balance_of`](crate::Ledger::balance_of)
///   knows it instead);
/// - [`LedgerError::InvalidPosting`] when the history mixes currencies on
///   one account.
pub fn balance_of(
    account: &AccountId,
    postings: &[Posting],
) -> Result<MonetaryAmount, LedgerError> {
    let mut running: Option<MonetaryAmount> = None;
    for posting in postings {
        let effect = if posting.credit() == account {
            posting.amount().clone()
        } else if posting.debit() == account {
            posting.amount().negate()
        } else {
            continue;
        };
        running = Some(match running {
            None => effect,
            Some(current) => {
                if current.currency != effect.currency {
                    return Err(LedgerError::InvalidPosting {
                        reason: format!(
                            "mixed currencies on account {account}: {} and {} both touch it",
                            current.currency.code(),
                            effect.currency.code()
                        ),
                    });
                }
                (current + effect).map_err(|e| LedgerError::InvalidPosting {
                    reason: e.to_string(),
                })?
            }
        });
    }
    running.ok_or(LedgerError::InvalidAccount)
}

/// A derived balance snapshot: the [`Account`] plus the [`MonetaryAmount`]
/// folded from the journal at read time.
///
/// Constructed by [`Ledger::balance`](crate::Ledger::balance_of) /
/// [`Ledger::trial_balance`](crate::Ledger::trial_balance); never stored.
/// The sign is meaningful: negative means the account owes (legal under
/// [`BalancePolicy::AllowNegative`](crate::BalancePolicy::AllowNegative)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountBalance {
    /// The account the balance belongs to.
    pub account: Account,
    /// The derived balance, signed by posting side.
    pub balance: MonetaryAmount,
}

impl AccountBalance {
    /// Derives a snapshot from a posting slice; see [`balance_of`].
    ///
    /// # Errors
    ///
    /// As [`balance_of`].
    pub fn derive(account: Account, postings: &[Posting]) -> Result<Self, LedgerError> {
        Ok(Self {
            balance: if postings.is_empty() {
                CurrencyAmount::new(0, account.currency)
            } else {
                balance_of(&account.id, postings)?
            },
            account,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::MonetaryAmount;
    use decimal_money::Currency;
    use idempotency_kit::IdempotencyKey;

    fn account(name: &str) -> AccountId {
        AccountId::new(name).unwrap()
    }

    fn key(n: u8) -> IdempotencyKey {
        IdempotencyKey::derive("ledger-kit.test", &[n]).expect("scope is valid")
    }

    fn usd(cents: i64) -> MonetaryAmount {
        MonetaryAmount::new(cents, Currency::USD)
    }

    fn post_at(
        n: u8,
        debit: &AccountId,
        credit: &AccountId,
        cents: i64,
        currency: Currency,
        recorded_at: u64,
    ) -> Posting {
        Posting::new(
            debit.clone(),
            credit.clone(),
            MonetaryAmount::new(cents, currency),
            String::new(),
            key(n),
            recorded_at,
        )
        .unwrap()
    }

    #[test]
    fn credit_adds_debit_subtracts() {
        let cash = account("cash-usd");
        let revenue = account("revenue");
        let p1 = post_at(1, &cash, &revenue, 1_000, Currency::USD, 1);
        let p2 = post_at(2, &revenue, &cash, 400, Currency::USD, 2);
        let postings = vec![p1, p2];
        // cash: -10.00 +4.00 = -6.00; revenue: +10.00 -4.00 = +6.00
        assert_eq!(balance_of(&cash, &postings).unwrap(), usd(-600));
        assert_eq!(balance_of(&revenue, &postings).unwrap(), usd(600));
    }

    #[test]
    fn uninvolved_postings_and_unknown_accounts() {
        let cash = account("cash-usd");
        let a = account("a");
        let b = account("b");
        let stranger = account("stranger");
        let postings = vec![post_at(1, &a, &b, 500, Currency::USD, 1)];
        assert_eq!(
            balance_of(&cash, &postings).err(),
            Some(LedgerError::InvalidAccount)
        );
        assert_eq!(
            balance_of(&stranger, &postings).err(),
            Some(LedgerError::InvalidAccount)
        );
    }

    #[test]
    fn empty_history_has_no_currency_to_report() {
        let cash = account("cash-usd");
        assert_eq!(
            balance_of(&cash, &[]).err(),
            Some(LedgerError::InvalidAccount)
        );
    }

    #[test]
    fn mixed_currencies_on_one_account_error() {
        let cash = account("cash");
        let eur_side = account("eur-side");
        let postings = vec![
            post_at(1, &eur_side, &cash, 500, Currency::USD, 1),
            post_at(2, &eur_side, &cash, 300, Currency::EUR, 2),
        ];
        let err = balance_of(&cash, &postings).unwrap_err();
        assert!(err.to_string().contains("mixed currencies"), "{err}");
    }

    #[test]
    fn same_currency_sums_across_many_postings() {
        let cash = account("cash-usd");
        let a = account("a");
        let b = account("b");
        let postings: Vec<Posting> = (0u8..8)
            .map(|i| {
                let (debit, credit, cents) = if i % 2 == 0 {
                    (a.clone(), cash.clone(), 100 + i64::from(i))
                } else {
                    (cash.clone(), b.clone(), i64::from(i))
                };
                post_at(i, &debit, &credit, cents, Currency::USD, u64::from(i))
            })
            .collect();
        // credit postings (even i): 100+102+104+106 = 412¢ credited
        // debit postings (odd i): 1+3+5+7 = 16¢ debited
        assert_eq!(balance_of(&cash, &postings).unwrap(), usd(396));
    }

    #[test]
    fn derived_snapshot_zero_when_untouched_uses_account_currency() {
        let acct = Account::new("cash-gbp", Currency::GBP).unwrap();
        let snap = AccountBalance::derive(acct.clone(), &[]).unwrap();
        assert_eq!(snap.account, acct);
        assert_eq!(snap.balance, MonetaryAmount::new(0, Currency::GBP));
        assert!(snap.balance.is_zero());
    }
}
