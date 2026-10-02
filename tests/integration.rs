//! End-to-end scenarios: the multi-institution ledger flow across the
//! estate's primitives — registry, idempotent posting, derived balances,
//! tamper-evident history, and outbox-backed restore.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use decimal_money::Currency;
use ledger_kit::{AccountId, Journal, Ledger, LedgerError, MonetaryAmount};

fn acct(name: &str) -> AccountId {
    AccountId::new(name).unwrap()
}

fn usd(cents: i64) -> MonetaryAmount {
    MonetaryAmount::new(cents, Currency::USD)
}

/// Two institutions, four accounts, a funding entry, three transfers.
async fn seeded() -> Ledger<ledger_kit::MemoryJournal> {
    let ledger = Ledger::new();
    for name in [
        "inst-a.cash",
        "inst-a.due-from-b",
        "inst-b.cash",
        "inst-b.due-to-a",
    ] {
        ledger.open_account(acct(name), Currency::USD).unwrap();
    }
    // Funding: inst-a's capital arrives (equity may run negative).
    ledger
        .post(
            &acct("inst-b.cash"),
            &acct("inst-a.cash"),
            usd(100_000),
            "opening capital",
            "opening-1",
        )
        .await
        .unwrap();
    ledger
}

#[tokio::test]
async fn multi_institution_flow_nets_to_zero() {
    let ledger = seeded().await;
    for (i, (cents, memo)) in [
        (25_000, "settlement 1"),
        (10_000, "settlement 2"),
        (7_550, "settlement 3"),
    ]
    .into_iter()
    .enumerate()
    {
        ledger
            .post(
                &acct("inst-a.cash"),
                &acct("inst-b.cash"),
                usd(cents),
                memo,
                &format!("settle-{i}"),
            )
            .await
            .unwrap();
    }

    let accounts: Vec<AccountId> = [
        "inst-a.cash",
        "inst-a.due-from-b",
        "inst-b.cash",
        "inst-b.due-to-a",
    ]
    .iter()
    .map(|n| acct(n))
    .collect();
    let lines = ledger.trial_balance(&accounts).await.unwrap();

    // Conservation: the trial balance nets to exactly zero.
    let total = lines
        .iter()
        .fold(None, |acc: Option<MonetaryAmount>, (_, b)| match acc {
            None => Some(b.clone()),
            Some(t) => Some((t + b.clone()).unwrap()),
        })
        .unwrap();
    assert!(total.is_zero(), "trial balance must net to zero: {total}");

    // inst-a.cash: +1000.00 -250.00 -100.00 -75.50 = 574.50
    let (cash_id, cash_balance) = lines.first().unwrap();
    assert_eq!(cash_id, &acct("inst-a.cash"));
    assert_eq!(cash_balance, &usd(57_450));
}

#[tokio::test]
async fn duplicate_replay_leaves_one_posting_and_answers_the_original_id() {
    let ledger = seeded().await;
    let first = ledger
        .post(
            &acct("inst-a.cash"),
            &acct("inst-b.cash"),
            usd(1_000),
            "wire",
            "wire-1",
        )
        .await
        .unwrap();

    for _ in 0..3 {
        let err = ledger
            .post(
                &acct("inst-a.cash"),
                &acct("inst-b.cash"),
                usd(1_000),
                "wire",
                "wire-1",
            )
            .await
            .unwrap_err();
        assert_eq!(
            err,
            LedgerError::DuplicatePosting { id: first.id() },
            "every replay answers the original id"
        );
    }
    // opening + wire = 2 postings, not 5.
    assert_eq!(ledger.journal().len(), 2);
}

#[tokio::test]
async fn policy_blocks_overdraw_then_allows_a_corrected_retry() {
    let ledger = seeded().await;
    ledger.set_balance_policy(ledger_kit::BalancePolicy::RejectNegative);

    let err = ledger
        .post(
            &acct("inst-a.cash"),
            &acct("inst-b.cash"),
            usd(200_000),
            "overdraw",
            "over-1",
        )
        .await
        .unwrap_err();
    assert_eq!(
        err,
        LedgerError::InsufficientFunds {
            account: acct("inst-a.cash"),
            attempted: usd(200_000),
        }
    );

    // The claim was released: the corrected retry reuses the same key.
    let retry = ledger
        .post(
            &acct("inst-a.cash"),
            &acct("inst-b.cash"),
            usd(50_000),
            "smaller wire",
            "over-1",
        )
        .await
        .unwrap();
    assert_eq!(retry.amount(), &usd(50_000));
    assert_eq!(
        ledger.balance_of(&acct("inst-a.cash")).await.unwrap(),
        usd(50_000)
    );
}

#[tokio::test]
async fn balances_are_derived_replayably_from_the_journal_alone() {
    let ledger = seeded().await;
    ledger
        .post(
            &acct("inst-a.cash"),
            &acct("inst-b.cash"),
            usd(4_200),
            "x",
            "x-1",
        )
        .await
        .unwrap();

    // Rebuild a fresh journal + registry purely from the posted history:
    // the derivation must reproduce the same balances — there is no
    // stored balance to carry over, because none exists.
    let history = ledger.journal().all_postings(usize::MAX).await.unwrap();
    let rebuilt = Ledger::with_journal(ledger_kit::MemoryJournal::new());
    for name in [
        "inst-a.cash",
        "inst-a.due-from-b",
        "inst-b.cash",
        "inst-b.due-to-a",
    ] {
        rebuilt.open_account(acct(name), Currency::USD).unwrap();
    }
    for posting in &history {
        rebuilt.journal().append(posting).await.unwrap();
    }
    for name in [
        "inst-a.cash",
        "inst-a.due-from-b",
        "inst-b.cash",
        "inst-b.due-to-a",
    ] {
        assert_eq!(
            rebuilt.balance_of(&acct(name)).await.unwrap(),
            ledger.balance_of(&acct(name)).await.unwrap(),
            "{name} derives identically from the journal alone"
        );
    }
}

#[cfg(feature = "audit")]
#[tokio::test]
async fn audit_chain_verifies_clean_and_detects_memo_tampering() {
    use tamper_audit::AuditChain;

    let ledger = seeded().await;
    let posted = ledger
        .post(
            &acct("inst-a.cash"),
            &acct("inst-b.cash"),
            usd(900),
            "audited transfer",
            "aud-1",
        )
        .await
        .unwrap();

    // Clean chain verifies (genesis + one entry per accepted posting:
    // the seed's opening posting and this one).
    ledger.verify_chain().await.unwrap();
    let entries = ledger.audit_entries().await.unwrap();
    assert_eq!(entries.len(), 3, "genesis + opening + the audited posting");
    let genesis = entries.first().unwrap();
    let posting_entry = entries.get(2).unwrap();
    assert_eq!(genesis.action, "log.created");
    assert_eq!(
        posting_entry
            .details
            .get("posting_id")
            .and_then(serde_json::Value::as_str),
        Some(posted.id().to_string().as_str())
    );

    // Tamper: flip a memo byte in a cloned store view. The posting is
    // captured verbatim in the entry details, so the entry hash no longer
    // matches and the chain is provably broken at that entry.
    let mut tampered = entries.clone();
    let entry = tampered.get_mut(2).unwrap();
    let memo = entry
        .details
        .get("memo")
        .and_then(serde_json::Value::as_str)
        .unwrap()
        .to_owned();
    let mut flipped = memo.into_bytes();
    let head = flipped.first().copied().unwrap_or(b'x');
    let turned = if head == b'a' { b'b' } else { b'a' };
    *flipped.first_mut().unwrap() = turned;
    if let Some(map) = entry.details.as_object_mut() {
        map.insert(
            "memo".to_owned(),
            serde_json::Value::String(String::from_utf8(flipped).unwrap()),
        );
    }

    let verdict = AuditChain::verify(&tampered);
    assert!(
        !verdict.is_valid(),
        "a flipped memo byte must break the chain"
    );
    assert_eq!(verdict.broken_at, Some(2));

    // The live ledger's chain (unmutated) still verifies.
    ledger.verify_chain().await.unwrap();
}

#[cfg(feature = "outbox")]
#[tokio::test]
async fn outbox_restore_rebuilds_balances_after_a_restart() {
    use ledger_kit::{MemoryJournal, OutboxJournal};
    use outbox_kit::{MemoryStore, OutboxStore};
    use std::sync::Arc;

    let outbox: Arc<dyn OutboxStore> = Arc::new(MemoryStore::new());
    let ledger_journal =
        OutboxJournal::with_inner_and_store(MemoryJournal::new(), Arc::clone(&outbox));
    let ledger = Ledger::with_journal(ledger_journal);
    for name in [
        "inst-a.cash",
        "inst-a.due-from-b",
        "inst-b.cash",
        "inst-b.due-to-a",
    ] {
        ledger.open_account(acct(name), Currency::USD).unwrap();
    }
    ledger
        .post(
            &acct("inst-b.cash"),
            &acct("inst-a.cash"),
            usd(80_000),
            "funding",
            "funding-1",
        )
        .await
        .unwrap();
    ledger
        .post(
            &acct("inst-a.cash"),
            &acct("inst-b.cash"),
            usd(12_000),
            "settlement",
            "settle-1",
        )
        .await
        .unwrap();
    let before_a = ledger.balance_of(&acct("inst-a.cash")).await.unwrap();
    let before_b = ledger.balance_of(&acct("inst-b.cash")).await.unwrap();

    // "Restart": a fresh journal + ledger over the SAME durable outbox.
    let recovered_journal = OutboxJournal::with_inner_and_store(MemoryJournal::new(), outbox);
    let restored = recovered_journal.restore().await.unwrap();
    assert_eq!(restored, 2, "both accepted postings replay from the outbox");

    let recovered = Ledger::with_journal(recovered_journal);
    for name in [
        "inst-a.cash",
        "inst-a.due-from-b",
        "inst-b.cash",
        "inst-b.due-to-a",
    ] {
        recovered.open_account(acct(name), Currency::USD).unwrap();
    }
    assert_eq!(
        recovered.balance_of(&acct("inst-a.cash")).await.unwrap(),
        before_a
    );
    assert_eq!(
        recovered.balance_of(&acct("inst-b.cash")).await.unwrap(),
        before_b
    );
}
