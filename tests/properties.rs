#![allow(clippy::unwrap_used, clippy::expect_used)]
//! proptest: balanced posting sets. Every generated set is constructed
//! zero-sum (each posting conserves by construction — the debit and the
//! credit are the same amount's two sides), so the suite exercises the
//! estate's conservation law, derivation determinism, and replay safety
//! over arbitrary shapes, not fixed fixtures.

use decimal_money::Currency;
use ledger_kit::{AccountId, Journal, Ledger, LedgerError, MonetaryAmount};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

/// Accounts used by the generated sets.
const ACCOUNTS: [&str; 4] = ["funding", "cash", "receivable", "revenue"];

fn acct(name: &str) -> AccountId {
    AccountId::new(name).unwrap()
}

fn usd(cents: i64) -> MonetaryAmount {
    MonetaryAmount::new(cents, Currency::USD)
}

/// A strategy: a sequence of `(cents, debit_idx, credit_idx)` legs.
/// Cents are strictly positive; debit ≠ credit is enforced by rerolling
/// inside the test (a same-side draw is skipped, keeping the strategy
/// total).
type Legs = Vec<(i64, usize, usize)>;

fn legs() -> impl Strategy<Value = Legs> {
    prop::collection::vec((1_i64..1_000_000, any::<u8>(), any::<u8>()), 1..16).prop_map(|raw| {
        raw.into_iter()
            .map(|(cents, a, b)| {
                (
                    cents,
                    a as usize % ACCOUNTS.len(),
                    b as usize % ACCOUNTS.len(),
                )
            })
            .collect()
    })
}

/// Runs the case on a fresh current-thread runtime.
fn run(
    body: impl std::future::Future<Output = Result<(), TestCaseError>>,
) -> Result<(), TestCaseError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(body)
}

fn fresh_ledger() -> Ledger<ledger_kit::MemoryJournal> {
    let ledger = Ledger::new();
    for name in ACCOUNTS {
        ledger.open_account(acct(name), Currency::USD).unwrap();
    }
    ledger
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn trial_balance_nets_to_zero_over_arbitrary_balanced_sets(
        legs in legs(),
    ) {
        run(async {
            let ledger = fresh_ledger();
            for (i, (cents, debit, credit)) in legs.iter().enumerate() {
                if debit == credit {
                    continue; // a same-side leg is not a posting; skip it
                }
                let outcome = ledger
                    .post(
                        &acct(ACCOUNTS.get(*debit).unwrap()),
                        &acct(ACCOUNTS.get(*credit).unwrap()),
                        usd(*cents),
                        "",
                        &format!("leg-{i}"),
                    )
                    .await;
                prop_assert!(
                    outcome.is_ok(),
                    "leg {i} must post: {:?}",
                    outcome.as_ref().err()
                );
                let _ = outcome.unwrap();
            }

            let ids: Vec<AccountId> = ACCOUNTS.iter().map(|n| acct(n)).collect();
            let lines = ledger.trial_balance(&ids).await.unwrap();
            let total = lines
                .iter()
                .fold(None, |acc: Option<MonetaryAmount>, (_, b)| match acc {
                    None => Some(b.clone()),
                    Some(t) => Some((t + b.clone()).unwrap()),
                })
                .unwrap();
            prop_assert!(total.is_zero(), "trial balance must net to zero, got {total}");

            // Derivation has a single source: the free fold over the
            // journal equals the ledger's answer, per account. (Accounts
            // with no postings are skipped: the free fold cannot know an
            // all-zero balance's currency — the registry does.)
            let history = ledger.journal().all_postings(usize::MAX).await.unwrap();
            for id in &ids {
                let touched = history
                    .iter()
                    .any(|p| p.debit() == id || p.credit() == id);
                if !touched {
                    continue;
                }
                prop_assert_eq!(
                    ledger_kit::balance_of(id, &history).unwrap(),
                    ledger.balance_of(id).await.unwrap()
                );
            }
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn replaying_a_set_never_double_posts(
        legs in legs(),
    ) {
        run(async {
            let ledger = fresh_ledger();
            let mut accepted = 0usize;
            for (i, (cents, debit, credit)) in legs.iter().enumerate() {
                if debit == credit {
                    continue;
                }
                let key = format!("set-{i}");
                if ledger
                    .post(
                        &acct(ACCOUNTS.get(*debit).unwrap()),
                        &acct(ACCOUNTS.get(*credit).unwrap()),
                        usd(*cents),
                        "",
                        &key,
                    )
                    .await
                    .is_ok()
                {
                    accepted += 1;
                }
            }
            let count_after_first_pass = ledger.journal().len();
            prop_assert_eq!(count_after_first_pass, accepted);

            // Identical replay: every leg answers DuplicatePosting (or is
            // skipped as a same-side leg), nothing is added, balances do
            // not move.
            for (i, (cents, debit, credit)) in legs.iter().enumerate() {
                if debit == credit {
                    continue;
                }
                let outcome = ledger
                    .post(
                        &acct(ACCOUNTS.get(*debit).unwrap()),
                        &acct(ACCOUNTS.get(*credit).unwrap()),
                        usd(*cents),
                        "",
                        &format!("set-{i}"),
                    )
                    .await;
                match outcome {
                    Err(LedgerError::DuplicatePosting { .. }) => {}
                    other => prop_assert!(
                        matches!(other, Err(LedgerError::DuplicatePosting { .. })),
                        "replay must answer DuplicatePosting, got {other:?}"
                    ),
                }
            }
            prop_assert_eq!(ledger.journal().len(), count_after_first_pass);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn derivation_is_deterministic_and_order_stable(
        legs in legs(),
    ) {
        run(async {
            let ledger = fresh_ledger();
            for (i, (cents, debit, credit)) in legs.iter().enumerate() {
                if debit == credit {
                    continue;
                }
                ledger
                    .post(
                        &acct(ACCOUNTS.get(*debit).unwrap()),
                        &acct(ACCOUNTS.get(*credit).unwrap()),
                        usd(*cents),
                        "",
                        &format!("det-{i}"),
                    )
                    .await
                    .unwrap();
            }
            let ids: Vec<AccountId> = ACCOUNTS.iter().map(|n| acct(n)).collect();
            let first = ledger.trial_balance(&ids).await.unwrap();
            let second = ledger.trial_balance(&ids).await.unwrap();
            prop_assert_eq!(first, second);

            // Journal order is (recorded_at, id)-stable across reads.
            let a = ledger.journal().all_postings(usize::MAX).await.unwrap();
            let b = ledger.journal().all_postings(usize::MAX).await.unwrap();
            prop_assert_eq!(a, b);
            Ok(())
        })
        .unwrap();
    }
}
