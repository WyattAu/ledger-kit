//! The ledger facade: [`BalancePolicy`] and [`Ledger`].

use std::collections::BTreeMap;
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;

use idempotency_kit::{Claim, IdempotencyKey, IdempotencyStore, MemoryStore};

use crate::account::{Account, AccountId};
use crate::balance::{balance_of, AccountBalance};
use crate::error::LedgerError;
use crate::journal::Journal;
use crate::memory::MemoryJournal;
use crate::posting::{Posting, PostingId};
use crate::{now_millis, Currency, MonetaryAmount};

#[cfg(feature = "audit")]
use tamper_audit::{AuditEntry, AuditLog, AuditQuery};

/// Idempotency scope for posting claims: one claim per caller key.
pub(crate) const POSTING_SCOPE: &str = "ledger-kit.posting";

/// Idempotency scope for request fingerprints: the caller key *and* the
/// canonical request bytes, so a key reused with a different request is
/// detected instead of silently double-posting.
const REQUEST_SCOPE: &str = "ledger-kit.posting.request";

/// How long an accepted-posting claim answers replays: long enough to span
/// client retries and crash-restart recovery, bounded so abandoned keys
/// reclaim themselves.
const CLAIM_TTL: Duration = Duration::from_secs(60 * 60 * 24);

/// Whether derived balances may go negative.
///
/// Checked at post time against the *projected* balance (derived balance
/// plus this posting's signed effect) — never against a stored number,
/// because no stored balance exists.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BalancePolicy {
    /// Accounts may carry negative balances (overdrafts, receivables
    /// accruing before funding). The default: the ledger records double
    /// entry and leaves credit decisions to the domain.
    #[default]
    AllowNegative,
    /// A posting is rejected with [`LedgerError::InsufficientFunds`] when
    /// the **debited account's** projected balance would go negative. The
    /// projection is in the account's registered currency.
    ///
    /// The check is debit-side by construction: under double-entry
    /// conservation the total of all balances is always zero, so a
    /// both-sides rule could never accept any posting — funding accounts
    /// (equity, revenue) carry the negative side by design. A credit only
    /// moves a balance toward or past zero and can never overdraw.
    RejectNegative,
}

impl BalancePolicy {
    /// The policy's name, for logs and diagnostics.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::AllowNegative => "allow-negative",
            Self::RejectNegative => "reject-negative",
        }
    }
}

/// The double-entry ledger.
///
/// A facade composing the estate's primitives into one audited, idempotent
/// posting path:
///
/// - **Balances are derived, never stored** —
///   [`balance_of`](crate::balance_of) folds the journal; there is no
///   mutable balance field to corrupt.
/// - **Posting is idempotent** — claims live in idempotency-kit's
///   [`MemoryStore`] (injectable for a shared store); a replayed caller
///   key answers [`LedgerError::DuplicatePosting`] with the original
///   posting's id instead of double-posting, and a reused key with a
///   *different* request is a typed conflict.
/// - **Every accepted posting is appended** to the configured [`Journal`]
///   and — under the `audit` feature — chained into a tamper-evident audit
///   log.
///
/// The default journal is [`MemoryJournal`]; for durability across
/// restarts wrap it in an [`OutboxJournal`](crate::OutboxJournal) (the
/// `outbox` feature) so accepted postings replay at-least-once from an
/// outbox-kit store.
pub struct Ledger<J: Journal = MemoryJournal> {
    /// The append-only posting journal (the single source of truth).
    journal: J,
    /// The account registry: id → single currency.
    accounts: RwLock<BTreeMap<AccountId, Currency>>,
    /// The active negative-balance policy.
    policy: RwLock<BalancePolicy>,
    /// The idempotency-claim store (a private [`MemoryStore`] unless
    /// injected).
    idempotency: Arc<dyn IdempotencyStore>,
    /// The tamper-evident chain of accepted postings (`audit` feature).
    #[cfg(feature = "audit")]
    chain: AuditLog<tamper_audit::InMemoryStore>,
}

impl Ledger<MemoryJournal> {
    /// A ledger with the default in-memory journal, MemoryStore-backed
    /// idempotency, and [`BalancePolicy::AllowNegative`].
    #[must_use]
    pub fn new() -> Self {
        LedgerBuilder::new().build()
    }

    /// Start configuring a ledger over the default in-memory journal.
    #[must_use]
    pub fn builder() -> LedgerBuilder<MemoryJournal> {
        LedgerBuilder::new()
    }
}

impl Default for Ledger<MemoryJournal> {
    fn default() -> Self {
        Self::new()
    }
}

impl<J: Journal> Ledger<J> {
    /// A ledger over a caller-supplied journal, with MemoryStore-backed
    /// idempotency and [`BalancePolicy::AllowNegative`].
    #[must_use]
    pub fn with_journal(journal: J) -> Self {
        Ledger {
            journal,
            accounts: RwLock::new(BTreeMap::new()),
            policy: RwLock::new(BalancePolicy::AllowNegative),
            idempotency: Arc::new(MemoryStore::new()),
            #[cfg(feature = "audit")]
            chain: AuditLog::new(),
        }
    }

    /// The journal this ledger appends to (exports, reads, restore
    /// flows).
    #[must_use]
    pub const fn journal(&self) -> &J {
        &self.journal
    }

    /// Registers an account — the currency single-source for every
    /// posting that will ever touch it.
    ///
    /// # Errors
    ///
    /// [`LedgerError::InvalidAccount`] when the id is already registered.
    pub fn open_account(&self, id: AccountId, currency: Currency) -> Result<Account, LedgerError> {
        let mut registry = self.accounts_write();
        if registry.contains_key(&id) {
            return Err(LedgerError::InvalidAccount);
        }
        registry.insert(id.clone(), currency);
        Ok(Account { id, currency })
    }

    /// The registered account, if any.
    #[must_use]
    pub fn account(&self, id: &AccountId) -> Option<Account> {
        self.accounts_read().get(id).map(|currency| Account {
            id: id.clone(),
            currency: *currency,
        })
    }

    /// Every registered account, ascending by id.
    #[must_use]
    pub fn accounts(&self) -> Vec<Account> {
        self.accounts_read()
            .iter()
            .map(|(id, currency)| Account {
                id: id.clone(),
                currency: *currency,
            })
            .collect()
    }

    /// The active [`BalancePolicy`].
    #[must_use]
    pub fn balance_policy(&self) -> BalancePolicy {
        *self.policy.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// Swaps the [`BalancePolicy`]; applies to subsequent posts only.
    pub fn set_balance_policy(&self, policy: BalancePolicy) {
        *self.policy.write().unwrap_or_else(PoisonError::into_inner) = policy;
    }

    /// Posts a double entry: move `amount` from `debit` to `credit`.
    ///
    /// The acceptance pipeline, in order:
    ///
    /// 1. **Idempotency claims** — the caller key is derived into a
    ///    scoped [`IdempotencyKey`] and claimed atomically, alongside a
    ///    fingerprint claim over the canonical request. A replayed key
    ///    answers [`LedgerError::DuplicatePosting`] (carrying the
    ///    original posting's id — nothing re-executes); a key reused with
    ///    a *different* request answers [`LedgerError::InvalidPosting`].
    /// 2. **Invariants** — both accounts registered, both registered
    ///    currencies equal to the amount's currency (which enforces the
    ///    debit/credit same-currency invariant), and [`Posting::new`]
    ///    enforces debit ≠ credit with amount > 0.
    /// 3. **Balance policy** — under [`BalancePolicy::RejectNegative`],
    ///    either account's projected balance may not go negative:
    ///    [`LedgerError::InsufficientFunds`].
    /// 4. **Journal append** (idempotent on the posting id), then
    /// 5. **Audit chain append** (`audit` feature), then the claims are
    ///    completed with the posting id so future replays resolve.
    ///
    /// Every failure before step 4 leaves the journal untouched and all
    /// claims released, so a corrected retry proceeds cleanly.
    ///
    /// # Errors
    ///
    /// [`LedgerError`] variants as annotated above;
    /// [`LedgerError::Store`] when a backend fails (retry the same
    /// request — derived posting ids and released claims make retries
    /// safe).
    pub async fn post(
        &self,
        debit: &AccountId,
        credit: &AccountId,
        amount: MonetaryAmount,
        memo: &str,
        idempotency_key: &str,
    ) -> Result<Posting, LedgerError> {
        let key = IdempotencyKey::derive(POSTING_SCOPE, idempotency_key.as_bytes())
            .map_err(|e| LedgerError::Store(e.to_string()))?;
        let request = canonical_request_bytes(idempotency_key, debit, credit, &amount, memo);
        let request_key = IdempotencyKey::derive(REQUEST_SCOPE, &request)
            .map_err(|e| LedgerError::Store(e.to_string()))?;

        // Every claim acquired during this attempt, in order: released
        // below on any failure so a corrected retry can claim again.
        let mut claimed: Vec<&IdempotencyKey> = Vec::new();
        let outcome = self
            .post_claimed(
                PostRequest {
                    debit,
                    credit,
                    amount,
                    memo,
                    raw_key: idempotency_key,
                    caller_claim: &key,
                    request_claim: &request_key,
                },
                &mut claimed,
            )
            .await;
        if outcome.is_err() {
            for key in &claimed {
                let _ = self.idempotency.release(key).await;
            }
        }
        outcome
    }

    /// The claim-and-execute core of [`Ledger::post`]: acquire both
    /// claims, then run validation and the append pipeline. Acquired keys
    /// are pushed onto `claimed` in acquisition order.
    async fn post_claimed<'k>(
        &self,
        request: PostRequest<'k>,
        claimed: &mut Vec<&'k IdempotencyKey>,
    ) -> Result<Posting, LedgerError> {
        // Claim the caller key first, then decide against the request
        // fingerprint: a replayed key with a replayed *request* is a true
        // retry; a replayed key with a fresh request is the classic
        // key-reuse conflict.
        match self
            .idempotency
            .claim(request.caller_claim, CLAIM_TTL)
            .await
        {
            Ok(Claim::First) => claimed.push(request.caller_claim),
            Ok(Claim::Replay(_)) => {
                return self
                    .decide_replay(request.request_claim, claimed, request.caller_claim)
                    .await;
            }
            Ok(Claim::InFlight) => {
                return Err(LedgerError::InvalidPosting {
                    reason: format!(
                        "idempotency key {} is in flight: a concurrent request holds it",
                        request.caller_claim
                    ),
                })
            }
            Err(e) => return Err(LedgerError::Store(e.to_string())),
        }

        match self
            .idempotency
            .claim(request.request_claim, CLAIM_TTL)
            .await
        {
            Ok(Claim::First) => claimed.push(request.request_claim),
            Ok(Claim::Replay(bytes)) => {
                // Fresh caller key, but this exact request already
                // completed — the same payload retried under a key whose
                // claim expired. Answer as a replay, not a conflict.
                return self.replay_conflict(&bytes).await;
            }
            Ok(Claim::InFlight) => {
                return Err(LedgerError::InvalidPosting {
                    reason: "the same request is already in flight".to_owned(),
                })
            }
            Err(e) => return Err(LedgerError::Store(e.to_string())),
        }

        self.post_validated(request).await
    }

    /// A replayed caller key: claim the request fingerprint to tell a
    /// true retry (same request → [`LedgerError::DuplicatePosting`] with
    /// the original posting id) from key reuse with a *different* request
    /// (→ [`LedgerError::InvalidPosting`], the classic idempotency-key
    /// conflict).
    async fn decide_replay<'k>(
        &self,
        request_key: &'k IdempotencyKey,
        claimed: &mut Vec<&'k IdempotencyKey>,
        caller_key: &IdempotencyKey,
    ) -> Result<Posting, LedgerError> {
        match self.idempotency.claim(request_key, CLAIM_TTL).await {
            Ok(Claim::First) => {
                // The request was never completed under any key: this key
                // was used for something else before.
                claimed.push(request_key);
                Err(LedgerError::InvalidPosting {
                    reason: format!(
                        "idempotency key {caller_key} was already used for a different request"
                    ),
                })
            }
            Ok(Claim::Replay(bytes)) => self.replay_conflict(&bytes).await,
            Ok(Claim::InFlight) => Err(LedgerError::InvalidPosting {
                reason: "the same request is already in flight".to_owned(),
            }),
            Err(e) => Err(LedgerError::Store(e.to_string())),
        }
    }

    /// Registry + invariant + policy checks, then the append pipeline.
    /// Both claims are already held (`First`) when this runs.
    async fn post_validated(&self, request: PostRequest<'_>) -> Result<Posting, LedgerError> {
        // Both accounts registered, both in the amount's currency — which
        // transitively enforces debit/credit same-currency.
        {
            let registry = self.accounts_read();
            let debit_currency = registry
                .get(request.debit)
                .ok_or(LedgerError::InvalidAccount)?;
            if *debit_currency != request.amount.currency {
                return Err(LedgerError::InvalidPosting {
                    reason: format!(
                        "account {} carries {}, posting carries {}",
                        request.debit,
                        debit_currency.code(),
                        request.amount.currency.code()
                    ),
                });
            }
            let credit_currency = registry
                .get(request.credit)
                .ok_or(LedgerError::InvalidAccount)?;
            if *credit_currency != request.amount.currency {
                return Err(LedgerError::InvalidPosting {
                    reason: format!(
                        "account {} carries {}, posting carries {}",
                        request.credit,
                        credit_currency.code(),
                        request.amount.currency.code()
                    ),
                });
            }
        }

        let posting = Posting::new(
            request.debit.clone(),
            request.credit.clone(),
            request.amount.clone(),
            request.memo,
            request.caller_claim.clone(),
            now_millis(),
        )?
        .with_caller_key(request.raw_key.to_owned());

        self.check_policy(request.debit, &request.amount).await?;

        self.journal.append(&posting).await?;

        #[cfg(feature = "audit")]
        self.audit_append(&posting).await?;

        // Claims complete last: from here, a replayed caller key resolves
        // to this posting's id instead of re-executing — and the request
        // fingerprint completes too, so a true retry (same key, same
        // request) resolves as a replay rather than "in flight".
        let response = posting.id().to_string().into_bytes();
        self.idempotency
            .complete(request.caller_claim, response.clone())
            .await
            .map_err(|e| LedgerError::Store(e.to_string()))?;
        self.idempotency
            .complete(request.request_claim, response)
            .await
            .map_err(|e| LedgerError::Store(e.to_string()))?;
        Ok(posting)
    }

    /// Projects the debited account's balance and applies the policy.
    ///
    /// The check is deliberately **debit-side only**: under double-entry
    /// conservation the total of all balances is always zero, so a
    /// "no account may be negative" rule on both sides could never accept
    /// any posting at all (funding accounts — equity, revenue — carry the
    /// negative side by design). A credit only moves a balance toward or
    /// past zero, so it can never overdraw.
    async fn check_policy(
        &self,
        debit: &AccountId,
        amount: &MonetaryAmount,
    ) -> Result<(), LedgerError> {
        if self.balance_policy() == BalancePolicy::AllowNegative {
            return Ok(());
        }
        let history = self.journal.postings_for(debit, usize::MAX).await?;
        let base = if history.is_empty() {
            let currency = self
                .account(debit)
                .map(|a: Account| a.currency)
                .ok_or(LedgerError::InvalidAccount)?;
            MonetaryAmount::new(0, currency)
        } else {
            balance_of(debit, &history)?
        };
        let projected = (base + amount.negate()).map_err(|e| LedgerError::InvalidPosting {
            reason: e.to_string(),
        })?;
        if projected.is_negative() {
            return Err(LedgerError::InsufficientFunds {
                account: debit.clone(),
                attempted: amount.clone(),
            });
        }
        Ok(())
    }

    /// Resolves a replayed claim into [`LedgerError::DuplicatePosting`]
    /// carrying the original posting's id.
    async fn replay_conflict(&self, bytes: &[u8]) -> Result<Posting, LedgerError> {
        let text = std::str::from_utf8(bytes).map_err(|_| {
            LedgerError::Store("idempotency replay payload is not a posting id".to_owned())
        })?;
        let id = PostingId::parse(text)?;
        match self.journal.posting_by_id(&id).await? {
            Some(_) => Err(LedgerError::DuplicatePosting { id }),
            None => Err(LedgerError::Store(format!(
                "idempotency replay points at posting {id}, which is no longer in the journal"
            ))),
        }
    }

    /// The derived balance of one registered account.
    ///
    /// # Errors
    ///
    /// [`LedgerError::InvalidAccount`] for an unregistered account;
    /// [`LedgerError::InvalidPosting`] for a mixed-currency history;
    /// [`LedgerError::Store`] on backend failure.
    pub async fn balance_of(&self, account: &AccountId) -> Result<MonetaryAmount, LedgerError> {
        let acct = self.account(account).ok_or(LedgerError::InvalidAccount)?;
        let history = self.journal.postings_for(account, usize::MAX).await?;
        if history.is_empty() {
            // Untouched registered account: zero in its registered
            // currency (the free fn cannot know the currency).
            return Ok(MonetaryAmount::new(0, acct.currency));
        }
        balance_of(account, &history)
    }

    /// A derived [`AccountBalance`] snapshot for one registered account
    /// (zero in the account's currency when untouched).
    ///
    /// # Errors
    ///
    /// As [`Ledger::balance_of`].
    pub async fn balance(&self, account: &AccountId) -> Result<AccountBalance, LedgerError> {
        let acct = self.account(account).ok_or(LedgerError::InvalidAccount)?;
        let history = self.journal.postings_for(account, usize::MAX).await?;
        AccountBalance::derive(acct, &history)
    }

    /// Net (signed) derived balance per requested account, in input
    /// order.
    ///
    /// Every requested account must be registered; untouched accounts
    /// report zero in their registered currency.
    ///
    /// # Errors
    ///
    /// [`LedgerError::InvalidAccount`] for an unregistered account;
    /// [`LedgerError::InvalidPosting`] / [`LedgerError::Store`] as the
    /// derivation requires.
    pub async fn trial_balance(
        &self,
        accounts: &[AccountId],
    ) -> Result<Vec<(AccountId, MonetaryAmount)>, LedgerError> {
        let mut lines = Vec::with_capacity(accounts.len());
        for account in accounts {
            lines.push((account.clone(), self.balance_of(account).await?));
        }
        Ok(lines)
    }
}

#[cfg(feature = "audit")]
impl<J: Journal> Ledger<J> {
    /// Re-verifies the tamper-evident chain: every entry's hash and every
    /// previous-hash link, genesis to head.
    ///
    /// # Errors
    ///
    /// [`LedgerError::ChainBroken`] with the first broken entry's index;
    /// [`LedgerError::Store`] when the audit backend fails.
    pub async fn verify_chain(&self) -> Result<(), LedgerError> {
        let verdict = self
            .chain
            .verify_chain()
            .await
            .map_err(|e| LedgerError::Store(e.to_string()))?;
        if verdict.is_valid() {
            Ok(())
        } else {
            Err(LedgerError::ChainBroken {
                at: verdict.broken_at.unwrap_or_default(),
            })
        }
    }

    /// The audit chain entries (genesis first), for inspection and export.
    ///
    /// # Errors
    ///
    /// [`LedgerError::Store`] when the audit backend fails.
    pub async fn audit_entries(&self) -> Result<Vec<AuditEntry>, LedgerError> {
        self.chain
            .query(&AuditQuery::new())
            .await
            .map_err(|e| LedgerError::Store(e.to_string()))
    }

    /// Chains an accepted posting into the audit log: one entry per
    /// posting, previous-hash → entry-hash, the posting captured verbatim
    /// in `details` (so any tampered byte in the recorded posting breaks
    /// the chain).
    async fn audit_append(&self, posting: &Posting) -> Result<(), LedgerError> {
        let details = serde_json::json!({
            "posting_id": posting.id().to_string(),
            "debit": posting.debit().to_string(),
            "credit": posting.credit().to_string(),
            "amount": posting.amount().amount.to_string(),
            "currency": posting.amount().currency.code(),
            "memo": posting.memo(),
            "idempotency_key": posting.idempotency_key().as_str(),
            "recorded_at": posting.recorded_at(),
        });
        self.chain
            .append(
                posting.id().to_string(),
                "ledger.posting",
                format!("{}->{}", posting.debit(), posting.credit()),
                details,
            )
            .await
            .map(|_entry: tamper_audit::AuditEntry| ())
            .map_err(|e| LedgerError::Store(e.to_string()))
    }
}

impl<J: Journal> Ledger<J> {
    fn accounts_read(&self) -> RwLockReadGuard<'_, BTreeMap<AccountId, Currency>> {
        self.accounts.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn accounts_write(&self) -> RwLockWriteGuard<'_, BTreeMap<AccountId, Currency>> {
        self.accounts
            .write()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl<J: Journal> std::fmt::Debug for Ledger<J> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ledger")
            .field("policy", &self.balance_policy())
            .field("accounts", &self.accounts().len())
            .finish_non_exhaustive()
    }
}

/// Builder for [`Ledger`] with non-default parts.
pub struct LedgerBuilder<J: Journal = MemoryJournal> {
    /// Journal override, if any.
    journal: Option<J>,
    /// Idempotency-store override, if any.
    idempotency: Option<Arc<dyn IdempotencyStore>>,
    /// The policy to install (default [`BalancePolicy::AllowNegative`]).
    policy: BalancePolicy,
}

impl<J: Journal> std::fmt::Debug for LedgerBuilder<J> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LedgerBuilder")
            .field("journal", &self.journal.as_ref().map(|_| ()))
            .field("idempotency", &self.idempotency.is_some())
            .field("policy", &self.policy)
            .finish()
    }
}

impl LedgerBuilder<MemoryJournal> {
    /// A builder over the default in-memory journal.
    #[must_use]
    pub fn new() -> Self {
        Self {
            journal: None,
            idempotency: None,
            policy: BalancePolicy::AllowNegative,
        }
    }
}

impl Default for LedgerBuilder<MemoryJournal> {
    fn default() -> Self {
        Self::new()
    }
}

impl<J: Journal> LedgerBuilder<J> {
    /// Appends into `journal` instead of a fresh [`MemoryJournal`].
    #[must_use]
    pub fn journal(journal: J) -> Self {
        Self {
            journal: Some(journal),
            idempotency: None,
            policy: BalancePolicy::AllowNegative,
        }
    }

    /// Uses `store` for idempotency claims instead of a private
    /// [`MemoryStore`] — share one store across ledgers or tiers.
    #[must_use]
    pub fn idempotency_store(mut self, store: Arc<dyn IdempotencyStore>) -> Self {
        self.idempotency = Some(store);
        self
    }

    /// Installs `policy` as the negative-balance policy.
    #[must_use]
    pub fn balance_policy(mut self, policy: BalancePolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Builds the ledger with a default-constructed journal.
    ///
    /// Custom journals without [`Default`]: pass one to
    /// [`Ledger::with_journal`] or fold the builder by hand.
    ///
    /// # Errors
    ///
    /// Never fails; the signature stays infallible.
    #[must_use]
    pub fn build(self) -> Ledger<J>
    where
        J: Default,
    {
        Ledger {
            journal: self.journal.unwrap_or_default(),
            accounts: RwLock::new(BTreeMap::new()),
            policy: RwLock::new(self.policy),
            idempotency: self
                .idempotency
                .unwrap_or_else(|| Arc::new(MemoryStore::new())),
            #[cfg(feature = "audit")]
            chain: AuditLog::new(),
        }
    }
}

/// The validated request context threaded through the claim pipeline:
/// the posting's inputs plus the two derived claim keys.
struct PostRequest<'k> {
    /// The debited account.
    debit: &'k AccountId,
    /// The credited account.
    credit: &'k AccountId,
    /// The strictly positive amount to move.
    amount: MonetaryAmount,
    /// The memo.
    memo: &'k str,
    /// The raw caller key (the claim preimage).
    raw_key: &'k str,
    /// The derived caller-key claim.
    caller_claim: &'k IdempotencyKey,
    /// The derived request-fingerprint claim.
    request_claim: &'k IdempotencyKey,
}

/// Frames a posting request into canonical claim bytes: fixed field
/// order, `\x1f` separators, the memo last (the only field without a
/// restricted charset), and no server-assigned timestamp — a retry has a
/// new clock but the same request.
fn canonical_request_bytes(
    idempotency_key: &str,
    debit: &AccountId,
    credit: &AccountId,
    amount: &MonetaryAmount,
    memo: &str,
) -> Vec<u8> {
    format!(
        "ledger-kit.posting.v1\x1f{}\x1f{debit}\x1f{credit}\x1f{}\x1f{}\x1f{memo}",
        idempotency_key,
        amount.currency.code(),
        amount.amount
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn account(name: &str) -> AccountId {
        AccountId::new(name).unwrap()
    }

    fn usd(cents: i64) -> MonetaryAmount {
        MonetaryAmount::new(cents, Currency::USD)
    }

    /// cash seeded with 100.00 (revenue carries -100), policy
    /// `RejectNegative`; "fees" untouched.
    async fn funded_ledger() -> Ledger<MemoryJournal> {
        let ledger = Ledger::new();
        ledger.open_account(account("cash"), Currency::USD).unwrap();
        ledger
            .open_account(account("revenue"), Currency::USD)
            .unwrap();
        ledger.open_account(account("fees"), Currency::USD).unwrap();
        ledger
            .post(
                &account("revenue"),
                &account("cash"),
                usd(10_000),
                "opening",
                "opening-1",
            )
            .await
            .unwrap();
        ledger.set_balance_policy(BalancePolicy::RejectNegative);
        ledger
    }

    #[tokio::test]
    async fn post_happy_path_records_and_derives() {
        let ledger = funded_ledger().await;
        let posting = ledger
            .post(
                &account("cash"),
                &account("revenue"),
                usd(1_250),
                "sale #1",
                "sale-1",
            )
            .await
            .unwrap();
        assert_eq!(posting.debit(), &account("cash"));
        assert_eq!(posting.credit(), &account("revenue"));
        assert_eq!(
            ledger.balance_of(&account("cash")).await.unwrap(),
            usd(8_750)
        );
        assert_eq!(
            ledger.balance_of(&account("revenue")).await.unwrap(),
            usd(-8_750)
        );
        assert_eq!(ledger.journal().len(), 2);
        // The snapshot type agrees with the raw derivation.
        let snap = ledger.balance(&account("cash")).await.unwrap();
        assert_eq!(snap.account.id, account("cash"));
        assert_eq!(snap.balance, usd(8_750));
    }

    #[tokio::test]
    async fn unregistered_accounts_reject() {
        let ledger = funded_ledger().await;
        for (debit, credit) in [("ghost", "cash"), ("cash", "ghost")] {
            let err = ledger
                .post(&account(debit), &account(credit), usd(1), "", "k")
                .await
                .unwrap_err();
            assert_eq!(err, LedgerError::InvalidAccount, "{debit}->{credit}");
        }
    }

    #[tokio::test]
    async fn registered_currency_gates_the_post() {
        let ledger = Ledger::new();
        ledger
            .open_account(account("cash-eur"), Currency::EUR)
            .unwrap();
        ledger
            .open_account(account("cash-usd"), Currency::USD)
            .unwrap();
        // USD amount into a EUR account: rejected before anything lands.
        let err = ledger
            .post(
                &account("cash-usd"),
                &account("cash-eur"),
                usd(5),
                "",
                "fx-1",
            )
            .await
            .unwrap_err();
        assert!(matches!(err, LedgerError::InvalidPosting { .. }), "{err}");
        assert_eq!(ledger.journal().len(), 0);
        // EUR amount across the two accounts: also rejected (same-currency
        // invariant enforced against the registry).
        let err = ledger
            .post(
                &account("cash-usd"),
                &account("cash-eur"),
                eur_amount(5),
                "",
                "fx-2",
            )
            .await
            .unwrap_err();
        assert!(matches!(err, LedgerError::InvalidPosting { .. }), "{err}");
        assert_eq!(ledger.journal().len(), 0);
    }

    fn eur_amount(cents: i64) -> MonetaryAmount {
        MonetaryAmount::new(cents, Currency::EUR)
    }

    #[tokio::test]
    async fn duplicate_posting_key_replays_the_original_id() {
        let ledger = funded_ledger().await;
        let first = ledger
            .post(
                &account("cash"),
                &account("revenue"),
                usd(300),
                "once",
                "idem-1",
            )
            .await
            .unwrap();
        let err = ledger
            .post(
                &account("cash"),
                &account("revenue"),
                usd(300),
                "once",
                "idem-1",
            )
            .await
            .unwrap_err();
        assert_eq!(
            err,
            LedgerError::DuplicatePosting { id: first.id() },
            "same key → same posting id, nothing double-posted"
        );
        assert_eq!(ledger.journal().len(), 2);
        assert_eq!(
            ledger.balance_of(&account("cash")).await.unwrap(),
            usd(9_700)
        );
    }

    #[tokio::test]
    async fn reused_key_with_different_request_conflicts() {
        let ledger = funded_ledger().await;
        ledger
            .post(
                &account("cash"),
                &account("revenue"),
                usd(300),
                "once",
                "idem-2",
            )
            .await
            .unwrap();
        // Same caller key, different amount: a typed conflict, not a
        // second posting.
        let err = ledger
            .post(
                &account("cash"),
                &account("revenue"),
                usd(900),
                "once",
                "idem-2",
            )
            .await
            .unwrap_err();
        assert!(matches!(err, LedgerError::InvalidPosting { .. }), "{err}");
        assert_eq!(ledger.journal().len(), 2);
    }

    #[tokio::test]
    async fn in_flight_claim_blocks_concurrent_post() {
        // A claim held outside the ledger: the same key is in flight.
        let store: Arc<dyn IdempotencyStore> = Arc::new(MemoryStore::new());
        let ledger = Ledger::builder()
            .idempotency_store(Arc::clone(&store))
            .build();
        ledger.open_account(account("cash"), Currency::USD).unwrap();
        ledger
            .open_account(account("revenue"), Currency::USD)
            .unwrap();

        let key = IdempotencyKey::derive(POSTING_SCOPE, b"hot").unwrap();
        assert!(matches!(
            store.claim(&key, CLAIM_TTL).await,
            Ok(Claim::First)
        ));

        let err = ledger
            .post(&account("cash"), &account("revenue"), usd(1), "", "hot")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("in flight"),
            "expected in-flight conflict: {err}"
        );
        assert_eq!(ledger.journal().len(), 0);
    }

    #[tokio::test]
    async fn reject_negative_blocks_overdraw_and_releases_the_claim() {
        let ledger = funded_ledger().await;
        let err = ledger
            .post(
                &account("cash"),
                &account("revenue"),
                usd(20_000),
                "too much",
                "over-1",
            )
            .await
            .unwrap_err();
        assert_eq!(
            err,
            LedgerError::InsufficientFunds {
                account: account("cash"),
                attempted: usd(20_000),
            }
        );
        assert_eq!(ledger.journal().len(), 1);

        // The failed claim was released: a corrected retry with the SAME
        // key goes through.
        let retry = ledger
            .post(
                &account("cash"),
                &account("revenue"),
                usd(5_000),
                "fits",
                "over-1",
            )
            .await
            .unwrap();
        assert_eq!(retry.amount(), &usd(5_000));
        assert_eq!(ledger.journal().len(), 2);
    }

    #[tokio::test]
    async fn allow_negative_permits_overdraft() {
        let ledger = funded_ledger().await;
        ledger.set_balance_policy(BalancePolicy::AllowNegative);
        ledger
            .post(
                &account("cash"),
                &account("revenue"),
                usd(20_000),
                "overdraft",
                "over-2",
            )
            .await
            .unwrap();
        assert_eq!(
            ledger.balance_of(&account("cash")).await.unwrap(),
            usd(-10_000)
        );
    }

    #[tokio::test]
    async fn trial_balance_nets_and_reports_zero_for_untouched() {
        let ledger = funded_ledger().await;
        ledger
            .post(&account("cash"), &account("fees"), usd(700), "fee", "fee-1")
            .await
            .unwrap();
        let expected = [
            ("cash", usd(9_300)),
            ("revenue", usd(-10_000)),
            ("fees", usd(700)),
        ];
        let ids: Vec<AccountId> = expected.iter().map(|(name, _)| account(name)).collect();
        let lines = ledger.trial_balance(&ids).await.unwrap();
        assert_eq!(lines.len(), expected.len());
        for ((name, expected_balance), (id, balance)) in expected.iter().zip(&lines) {
            assert_eq!(id, &account(name));
            assert_eq!(balance, expected_balance, "{name} net");
        }

        // Zero-sum: the estate's conservation law.
        let sum = lines
            .iter()
            .fold(None, |acc: Option<MonetaryAmount>, (_, b)| match acc {
                None => Some(b.clone()),
                Some(total) => Some((total + b.clone()).unwrap()),
            })
            .unwrap();
        assert!(sum.is_zero(), "trial balance must net to zero, got {sum}");

        // Unregistered accounts error.
        let err = ledger
            .trial_balance(std::slice::from_ref(&account("ghost")))
            .await
            .unwrap_err();
        assert_eq!(err, LedgerError::InvalidAccount);
    }

    #[tokio::test]
    async fn balances_are_always_rederived_from_the_journal() {
        // No stored balance exists: the same postings derived through a
        // fresh read of the journal equal the ledger's answer, every time.
        let ledger = funded_ledger().await;
        let history = ledger.journal().all_postings(usize::MAX).await.unwrap();
        let via_free_fn = balance_of(&account("cash"), &history).unwrap();
        assert_eq!(
            via_free_fn,
            ledger.balance_of(&account("cash")).await.unwrap()
        );
    }

    #[tokio::test]
    async fn registry_helpers_report_accounts_and_policy() {
        let ledger = funded_ledger().await;
        assert_eq!(ledger.accounts().len(), 3);
        assert_eq!(
            ledger.account(&account("cash")).unwrap().currency,
            Currency::USD
        );
        assert!(ledger.account(&account("ghost")).is_none());
        assert_eq!(ledger.balance_policy(), BalancePolicy::RejectNegative);
        ledger.set_balance_policy(BalancePolicy::AllowNegative);
        assert_eq!(ledger.balance_policy(), BalancePolicy::AllowNegative);
        assert_eq!(ledger.balance_policy().name(), "allow-negative");
        // Duplicate registration is rejected.
        assert_eq!(
            ledger
                .open_account(account("cash"), Currency::USD)
                .unwrap_err(),
            LedgerError::InvalidAccount
        );
        let debug = format!("{ledger:?}");
        assert!(debug.contains("policy"), "{debug}");
    }

    #[tokio::test]
    async fn claims_complete_after_acceptance_and_replay_finds_the_posting() {
        let store: Arc<dyn IdempotencyStore> = Arc::new(MemoryStore::new());
        let ledger = Ledger::builder()
            .idempotency_store(Arc::clone(&store))
            .build();
        ledger.open_account(account("cash"), Currency::USD).unwrap();
        ledger
            .open_account(account("revenue"), Currency::USD)
            .unwrap();
        let posted = ledger
            .post(&account("cash"), &account("revenue"), usd(4), "", "done-1")
            .await
            .unwrap();
        // The claim completed with the posting id: the replay path can
        // resolve it back out of the journal.
        let key = IdempotencyKey::derive(POSTING_SCOPE, b"done-1").unwrap();
        let claim = store.claim(&key, CLAIM_TTL).await.unwrap();
        assert!(
            matches!(claim, Claim::Replay(_)),
            "claim must complete: {claim:?}"
        );
        if let Claim::Replay(bytes) = claim {
            let id = PostingId::parse(std::str::from_utf8(&bytes).unwrap()).unwrap();
            assert_eq!(id, posted.id());
            assert!(ledger.journal().posting_by_id(&id).await.unwrap().is_some());
        }
    }
}
