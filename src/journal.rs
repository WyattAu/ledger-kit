//! The journal contract: [`Journal`].

use async_trait::async_trait;

use crate::account::AccountId;
use crate::error::LedgerError;
use crate::posting::{Posting, PostingId};

/// Append-only posting storage.
///
/// The trait is object-safe (`Arc<dyn Journal>` works) so applications can
/// swap stores per deployment tier — [`MemoryJournal`](crate::MemoryJournal)
/// for tests and single processes, a durable store (roadmap: outbox-kit-style
/// SQLite behind a `sqlite` feature) for durability across restarts —
/// without generic plumbing.
///
/// # Contract notes
///
/// - `append` is **idempotent on `Posting::id`** (INSERT-OR-IGNORE
///   semantics): a producer that crashes between acceptance and read-back
///   can safely retry the append; the journal keeps exactly one row per id
///   and returns `Ok(())` both times. Higher-level duplicate detection
///   (idempotency keys → [`LedgerError::DuplicatePosting`]) lives in
///   [`Ledger`](crate::Ledger), not in the journal.
/// - Reads are ordered by `(recorded_at, id)` — a deterministic,
///   time-ordered sequence on every backend (the same scheduling model as
///   outbox-kit's stores). `postings_for` and `all_postings` return the
///   **most recent `limit`** postings in that ascending order; pass
///   [`usize::MAX`] for the full history.
/// - Methods return [`LedgerError`] so backends can fail closed
///   ([`LedgerError::Store`]) instead of silently serving partial history —
///   a silently-truncated balance is exactly the corruption a ledger must
///   never paper over.
#[async_trait]
pub trait Journal: Send + Sync {
    /// Record a posting. Idempotent on [`Posting::id`]: appending the same
    /// id twice leaves one row and returns `Ok(())` both times.
    ///
    /// # Errors
    ///
    /// [`LedgerError::Store`] if the backend failed (the posting was not
    /// recorded).
    async fn append(&self, posting: &Posting) -> Result<(), LedgerError>;

    /// The most recent `limit` postings touching `account` (debit or
    /// credit side), ascending by `(recorded_at, id)`. An account with no
    /// postings yields an empty vec, not an error.
    ///
    /// # Errors
    ///
    /// [`LedgerError::Store`] if the backend failed.
    async fn postings_for(
        &self,
        account: &AccountId,
        limit: usize,
    ) -> Result<Vec<Posting>, LedgerError>;

    /// The most recent `limit` postings across all accounts, ascending by
    /// `(recorded_at, id)`.
    ///
    /// # Errors
    ///
    /// [`LedgerError::Store`] if the backend failed.
    async fn all_postings(&self, limit: usize) -> Result<Vec<Posting>, LedgerError>;

    /// Look a posting up by id.
    ///
    /// # Errors
    ///
    /// [`LedgerError::Store`] if the backend failed.
    async fn posting_by_id(&self, id: &PostingId) -> Result<Option<Posting>, LedgerError>;
}
