//! The default in-process journal: [`MemoryJournal`].
//!
//! A `BTreeMap` keyed by `(recorded_at, PostingId)` — the deterministic
//! `(time, id)` ordering every journal backend guarantees. Single process,
//! single writer per map; poisoning is recovered, not propagated. The
//! durable-store roadmap (outbox-kit-style SQLite behind a `sqlite`
//! feature, WAL, embedded schema) will implement this same trait.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;

use crate::account::AccountId;
use crate::error::LedgerError;
use crate::journal::Journal;
use crate::posting::{Posting, PostingId};

/// Most-recent-first windowing: collect everything, then keep the tail.
/// Journals are append-only and bounded by real usage; the in-memory
/// backend keeps it simple and total.
#[cfg(test)]
pub(crate) const FULL_HISTORY: usize = usize::MAX;

/// The default journal: an in-process, ordered `BTreeMap`.
///
/// Ordering is `(recorded_at, PostingId)` — deterministic and
/// time-ordered, identical to outbox-kit's store contract. `append` is
/// INSERT-OR-IGNORE on the posting id.
///
/// For durability across restarts, see
/// [`OutboxJournal`](crate::OutboxJournal) (mirrors every accepted posting
/// into an outbox-kit store) — a native `SqliteJournal` behind a `sqlite`
/// feature is the roadmap's next step.
#[derive(Debug, Default)]
pub struct MemoryJournal {
    /// Postings ordered by `(recorded_at, id)`; id collisions within one
    /// timestamp are impossible (ids are unique), so the key is total.
    postings: Mutex<BTreeMap<(u64, PostingId), Posting>>,
}

impl MemoryJournal {
    /// Creates an empty journal.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The number of stored postings.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// True when nothing has been appended.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Lock the inner map, recovering from poisoning. A panic in another
    /// thread may have corrupted an in-flight append, but the `BTreeMap`
    /// itself stays structurally sound — drop the poison rather than
    /// turning one panicked thread into a permanently poisoned ledger.
    fn lock(&self) -> MutexGuard<'_, BTreeMap<(u64, PostingId), Posting>> {
        self.postings.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Shared tail-window logic: the last `limit` entries, ascending.
    fn tail(map: &BTreeMap<(u64, PostingId), Posting>, limit: usize) -> Vec<Posting> {
        if limit == 0 {
            return Vec::new();
        }
        let total = map.len();
        let skip = total.saturating_sub(limit);
        map.values().skip(skip).cloned().collect()
    }
}

#[async_trait]
impl Journal for MemoryJournal {
    async fn append(&self, posting: &Posting) -> Result<(), LedgerError> {
        // INSERT-OR-IGNORE on the id: a retried append keeps one row and
        // succeeds both times (the trait contract).
        self.lock()
            .insert((posting.recorded_at(), posting.id()), posting.clone());
        Ok(())
    }

    async fn postings_for(
        &self,
        account: &AccountId,
        limit: usize,
    ) -> Result<Vec<Posting>, LedgerError> {
        let matches: Vec<Posting> = self
            .lock()
            .values()
            .filter(|p| p.debit() == account || p.credit() == account)
            .cloned()
            .collect();
        Ok(Self::tail_map(&matches, limit))
    }

    async fn all_postings(&self, limit: usize) -> Result<Vec<Posting>, LedgerError> {
        Ok(Self::tail(&self.lock(), limit))
    }

    async fn posting_by_id(&self, id: &PostingId) -> Result<Option<Posting>, LedgerError> {
        Ok(self.lock().values().find(|p| &p.id() == id).cloned())
    }
}

impl MemoryJournal {
    /// `tail` over an already-filtered sequence (values arrive ascending).
    fn tail_map(values: &[Posting], limit: usize) -> Vec<Posting> {
        if limit == 0 {
            return Vec::new();
        }
        let skip = values.len().saturating_sub(limit);
        values.iter().skip(skip).cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::{Currency, MonetaryAmount};
    use idempotency_kit::IdempotencyKey;

    fn account(name: &str) -> AccountId {
        AccountId::new(name).unwrap()
    }

    fn key(n: u8) -> IdempotencyKey {
        IdempotencyKey::derive("ledger-kit.test", &[n]).expect("scope is valid")
    }

    fn post(n: u8, debit: &str, credit: &str, cents: i64, at: u64) -> Posting {
        Posting::new(
            account(debit),
            account(credit),
            MonetaryAmount::new(cents, Currency::USD),
            String::new(),
            key(n),
            at,
        )
        .unwrap()
    }

    async fn fill(journal: &MemoryJournal) -> Vec<Posting> {
        let p1 = post(1, "cash", "revenue", 1_000, 1_000);
        let p2 = post(2, "cash", "revenue", 500, 2_000);
        let p3 = post(3, "cash", "fees", 100, 3_000);
        for p in [&p1, &p2, &p3] {
            journal.append(p).await.unwrap();
        }
        vec![p1, p2, p3]
    }

    #[tokio::test]
    async fn append_is_idempotent_on_posting_id() {
        let journal = MemoryJournal::new();
        let p = post(1, "cash", "revenue", 10, 1_000);
        journal.append(&p).await.unwrap();
        // The same id again is INSERT-OR-IGNORE: one row, Ok both times.
        journal.append(&p).await.unwrap();
        assert_eq!(journal.len(), 1);
        assert_eq!(journal.all_postings(FULL_HISTORY).await.unwrap(), vec![p]);
    }

    #[tokio::test]
    async fn reads_are_time_and_id_ordered() {
        let journal = MemoryJournal::new();
        let filled = fill(&journal).await;
        assert_eq!(
            journal.all_postings(FULL_HISTORY).await.unwrap(),
            filled,
            "ascending by (recorded_at, id)"
        );
    }

    #[tokio::test]
    async fn postings_for_filters_both_sides_and_limits_to_recent() {
        let journal = MemoryJournal::new();
        fill(&journal).await;
        let cash = account("cash");
        let recent = journal.postings_for(&cash, 2).await.unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(
            recent.first().unwrap().recorded_at(),
            2_000,
            "most recent window, ascending"
        );
        assert_eq!(recent.get(1).unwrap().recorded_at(), 3_000);

        let none = journal.postings_for(&account("ghost"), 10).await.unwrap();
        assert!(none.is_empty(), "no postings is empty vec, not an error");

        let zero_limit = journal.postings_for(&cash, 0).await.unwrap();
        assert!(zero_limit.is_empty());
    }

    #[tokio::test]
    async fn all_postings_honors_limit() {
        let journal = MemoryJournal::new();
        fill(&journal).await;
        let recent = journal.all_postings(2).await.unwrap();
        assert_eq!(
            recent.iter().map(Posting::recorded_at).collect::<Vec<_>>(),
            vec![2_000, 3_000]
        );
        assert_eq!(journal.all_postings(0).await.unwrap().len(), 0);
        assert_eq!(journal.all_postings(10).await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn lookup_by_id_hits_and_misses() {
        let journal = MemoryJournal::new();
        let filled = fill(&journal).await;
        let first = filled.first().unwrap();
        assert_eq!(
            journal.posting_by_id(&first.id()).await.unwrap(),
            Some(first.clone())
        );
        assert_eq!(
            journal.posting_by_id(&PostingId::now_v7()).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn empty_journal_is_empty() {
        let journal = MemoryJournal::new();
        assert!(journal.is_empty());
        assert_eq!(journal.len(), 0);
    }
}
