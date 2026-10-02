//! The durable journal: [`OutboxJournal`], a [`Journal`] that mirrors
//! every accepted posting into an outbox-kit [`OutboxStore`].
//!
//! The posting journal remains the source of truth for reads; the outbox
//! is the *durable record*. A posting is mirrored to the outbox **before**
//! the fast in-memory append, so a crash between the two loses nothing:
//! the envelope survives in the outbox and [`OutboxJournal::restore`]
//! replays it into a fresh journal after restart — at-least-once,
//! idempotent on the posting id on both sides (the outbox deduplicates by
//! envelope id, the journal by posting id).
//!
//! The payload codec is a compact, deterministic byte framing
//! ([`encode_posting`] / [`decode_posting`]) — no serde dependency, the
//! same bytes encode the same posting on every backend, and every decoded
//! posting is re-validated through [`Posting::new`] before it re-enters a
//! journal.

use std::sync::Arc;

use async_trait::async_trait;
use outbox_kit::{EventId, MemoryStore, OutboxEvent, OutboxStore};
use uuid::Uuid;

use crate::account::AccountId;
use crate::error::LedgerError;
use crate::journal::Journal;
use crate::memory::MemoryJournal;
use crate::now_millis;
use crate::posting::{Posting, PostingId};

/// The outbox topic every posting mirrors into. Matches outbox-kit's topic
/// charset (`[a-z0-9_.-]{1,128}`) so the store accepts it verbatim.
pub const OUTBOX_TOPIC: &str = "ledger-kit.postings";

/// Batch size for one [`OutboxJournal::restore`] drain pass.
const RESTORE_BATCH: usize = 1024;

/// A [`Journal`] that mirrors accepted postings into an outbox-kit
/// [`OutboxStore`] (a [`MemoryStore`] by default; their `sqlite` store
/// makes the record durable across restarts).
///
/// Reads and the fast path delegate to the inner journal; `append`
/// additionally mirrors the posting as an [`OutboxEvent`] whose id *is*
/// the posting id (time-ordered on both sides, deduplicated on replay).
pub struct OutboxJournal<J: Journal = MemoryJournal> {
    /// The fast-path journal (the source of truth for reads).
    inner: J,
    /// The durable record.
    outbox: Arc<dyn OutboxStore>,
}

impl OutboxJournal<MemoryJournal> {
    /// A mirror journal with the default in-memory journal and outbox
    /// store.
    #[must_use]
    pub fn new() -> Self {
        Self::with_inner_and_store(MemoryJournal::new(), Arc::new(MemoryStore::new()))
    }
}

impl Default for OutboxJournal<MemoryJournal> {
    fn default() -> Self {
        Self::new()
    }
}

impl<J: Journal> OutboxJournal<J> {
    /// A mirror journal over `inner`, mirroring into `outbox`.
    #[must_use]
    pub fn with_inner_and_store(inner: J, outbox: Arc<dyn OutboxStore>) -> Self {
        Self { inner, outbox }
    }

    /// The inner journal (the source of truth for reads).
    #[must_use]
    pub const fn inner(&self) -> &J {
        &self.inner
    }

    /// Replays undelivered envelopes from the outbox into the inner
    /// journal — the crash-recovery path.
    ///
    /// Each due envelope is decoded, re-validated through [`Posting::new`],
    /// and appended (idempotent on the posting id), then marked dispatched
    /// so it is not replayed twice. Returns the number of postings
    /// restored; already-present postings count as restored (they were
    /// recovered), just not duplicated.
    ///
    /// # Errors
    ///
    /// [`LedgerError::Store`] when the outbox backend fails or an envelope
    /// fails to decode or validate — the corrupt envelope is left in
    /// place (fail closed), not dropped.
    pub async fn restore(&self) -> Result<usize, LedgerError> {
        let mut restored = 0;
        loop {
            let due = self
                .outbox
                .fetch_due(RESTORE_BATCH, now_millis())
                .await
                .map_err(|e| LedgerError::Store(e.to_string()))?;
            let batch = due.len();
            for event in due {
                let posting = decode_posting(&event.payload)?;
                self.inner.append(&posting).await?;
                self.outbox
                    .mark_dispatched(&event.id)
                    .await
                    .map_err(|e| LedgerError::Store(e.to_string()))?;
                restored += 1;
            }
            if batch < RESTORE_BATCH {
                return Ok(restored);
            }
        }
    }
}

#[async_trait]
impl<J: Journal> Journal for OutboxJournal<J> {
    /// Mirror to the durable outbox **first**, then the inner journal: if
    /// the process dies between the two, the envelope survives and
    /// [`OutboxJournal::restore`] recovers the posting; the reverse order
    /// could lose an accepted posting.
    ///
    /// # Errors
    ///
    /// [`LedgerError::Store`] when either backend fails; nothing was
    /// accepted unless both succeed (a posting mirrored but not appended
    /// is recovered by `restore`).
    async fn append(&self, posting: &Posting) -> Result<(), LedgerError> {
        let event = OutboxEvent {
            id: EventId::from_uuid(posting.id().as_uuid()),
            topic: OUTBOX_TOPIC.to_owned(),
            payload: encode_posting(posting)?,
            headers: std::collections::BTreeMap::new(),
            created_at: posting.recorded_at(),
            attempts: 0,
            last_error: None,
        };
        self.outbox
            .append(&event)
            .await
            .map_err(|e| LedgerError::Store(e.to_string()))?;
        self.inner.append(posting).await
    }

    async fn postings_for(
        &self,
        account: &AccountId,
        limit: usize,
    ) -> Result<Vec<Posting>, LedgerError> {
        self.inner.postings_for(account, limit).await
    }

    async fn all_postings(&self, limit: usize) -> Result<Vec<Posting>, LedgerError> {
        self.inner.all_postings(limit).await
    }

    async fn posting_by_id(&self, id: &PostingId) -> Result<Option<Posting>, LedgerError> {
        self.inner.posting_by_id(id).await
    }
}

/// Encodes a posting into the deterministic wire framing:
/// `recorded_at` (u64 LE) · id (16-byte UUID) · debit · credit · currency
/// · amount-decimal-string · memo · raw caller key, each string as
/// `u16 LE length + bytes`.
///
/// The last field carries the **raw caller key** (the BLAKE3 preimage),
/// not the rendered key: `IdempotencyKey` is one-way, so only the preimage
/// can re-derive the key bit-identically on restore. Postings without one
/// (never built by [`Ledger::post`](crate::Ledger::post)) encode an empty
/// field and [`decode_posting`] fails closed on them.
/// # Errors
///
/// [`LedgerError::Store`] when a field exceeds the 64 KiB per-field wire
/// bound (in practice: an absurd memo).
pub fn encode_posting(posting: &Posting) -> Result<Vec<u8>, LedgerError> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&posting.recorded_at().to_le_bytes());
    buf.extend_from_slice(posting.id().as_uuid().as_bytes());
    for field in [
        posting.debit().as_str(),
        posting.credit().as_str(),
        posting.amount().currency.code(),
        posting.amount().amount.to_string().as_str(),
        posting.memo(),
        posting.caller_key().unwrap_or_default(),
    ] {
        let bytes = field.as_bytes();
        let len = u16::try_from(bytes.len())
            .map_err(|_| LedgerError::Store("posting field exceeds 64 KiB".to_owned()))?;
        buf.extend_from_slice(&len.to_le_bytes());
        buf.extend_from_slice(bytes);
    }
    Ok(buf)
}

/// Decodes and re-validates a posting from [`encode_posting`] framing.
///
/// # Errors
///
/// [`LedgerError::Store`] when the framing is malformed, a field is not
/// valid UTF-8, the currency is unknown, or the decoded posting violates
/// a double-entry invariant (it is re-validated before it can re-enter a
/// journal).
pub fn decode_posting(payload: &[u8]) -> Result<Posting, LedgerError> {
    /// Reads exactly `want` bytes at `cursor`, failing closed on a
    /// truncated or overflowing payload.
    fn take(payload: &[u8], cursor: usize, want: usize) -> Result<&[u8], LedgerError> {
        let end = cursor
            .checked_add(want)
            .ok_or_else(|| LedgerError::Store("posting payload length overflows".to_owned()))?;
        payload
            .get(cursor..end)
            .ok_or_else(|| LedgerError::Store("posting payload is truncated".to_owned()))
    }

    /// Reconstructs the posting's idempotency key from the raw caller key
    /// carried on the wire: the ledger always derives under
    /// [`POSTING_SCOPE`], and `derive` is deterministic, so re-deriving
    /// over the decoded preimage reproduces the key bit-identically.
    fn rebuild_key(raw: &str) -> Result<idempotency_kit::IdempotencyKey, LedgerError> {
        use idempotency_kit::IdempotencyKey;

        if raw.is_empty() {
            return Err(LedgerError::Store(
                "payload carries no raw caller key: this posting was not built by Ledger::post \
                 and its idempotency key cannot be re-derived"
                    .to_owned(),
            ));
        }
        IdempotencyKey::derive(crate::ledger::POSTING_SCOPE, raw.as_bytes())
            .map_err(|e| LedgerError::Store(format!("payload idempotency key: {e}")))
    }

    let mut cursor = 0usize;
    let recorded_at = u64::from_le_bytes(
        take(payload, cursor, 8)?
            .try_into()
            .map_err(|_| LedgerError::Store("recorded_at framing".to_owned()))?,
    );
    cursor += 8;
    let id_bytes: [u8; 16] = take(payload, cursor, 16)?
        .try_into()
        .map_err(|_| LedgerError::Store("posting id framing".to_owned()))?;
    cursor += 16;
    let id = PostingId::from_uuid(Uuid::from_bytes(id_bytes));

    let mut strings = Vec::new();
    for _ in 0..6 {
        let len = u16::from_le_bytes(
            take(payload, cursor, 2)?
                .try_into()
                .map_err(|_| LedgerError::Store("string length framing".to_owned()))?,
        );
        cursor += 2;
        let bytes = take(payload, cursor, len as usize)?;
        cursor += bytes.len();
        strings.push(
            String::from_utf8(bytes.to_vec())
                .map_err(|_| LedgerError::Store("payload field is not UTF-8".to_owned()))?,
        );
    }
    let [debit, credit, currency_code, amount_str, memo, key_str] = strings
        .try_into()
        .map_err(|_| LedgerError::Store("payload field count".to_owned()))?;

    let currency: crate::Currency = currency_code
        .parse()
        .map_err(|_| LedgerError::Store(format!("unknown currency {currency_code:?}")))?;
    let amount = crate::posting::amount(&amount_str, currency)?;
    let debit_id = AccountId::new(debit.clone()).map_err(|_| {
        LedgerError::Store(format!("payload carries an invalid account id {debit:?}"))
    })?;
    let credit_id = AccountId::new(credit.clone()).map_err(|_| {
        LedgerError::Store(format!("payload carries an invalid account id {credit:?}"))
    })?;
    Ok(Posting::rebuild(
        id,
        debit_id,
        credit_id,
        amount,
        memo,
        rebuild_key(&key_str)?,
        recorded_at,
    )?
    .with_caller_key(key_str))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use crate::MonetaryAmount;
    use idempotency_kit::IdempotencyKey;

    use super::*;

    fn key(n: u8) -> IdempotencyKey {
        IdempotencyKey::derive("ledger-kit.posting", &[n]).expect("scope is valid")
    }

    fn sample(at: u64) -> Posting {
        let key = IdempotencyKey::derive(crate::ledger::POSTING_SCOPE, b"order-42")
            .expect("scope is valid");
        Posting::new(
            AccountId::new("cash-usd").unwrap(),
            AccountId::new("revenue").unwrap(),
            MonetaryAmount::new(1_250, crate::Currency::USD),
            "sale #1",
            key,
            at,
        )
        .unwrap()
        .with_caller_key("order-42")
    }

    #[test]
    fn codec_roundtrips_every_field() {
        let posting = sample(1_700_000_000_123);
        let posting = Posting::new(
            posting.debit().clone(),
            posting.credit().clone(),
            posting.amount().clone(),
            "memo with spaces, ünïcode, and \t tabs",
            IdempotencyKey::derive(crate::ledger::POSTING_SCOPE, b"retry-9")
                .expect("scope is valid"),
            posting.recorded_at(),
        )
        .unwrap()
        .with_caller_key("retry-9");
        let decoded = decode_posting(&encode_posting(&posting).unwrap()).unwrap();
        assert_eq!(decoded.id(), posting.id());
        assert_eq!(decoded.recorded_at(), posting.recorded_at());
        assert_eq!(decoded.debit(), posting.debit());
        assert_eq!(decoded.credit(), posting.credit());
        assert_eq!(decoded.amount(), posting.amount());
        assert_eq!(decoded.memo(), posting.memo());
        assert_eq!(decoded.idempotency_key(), posting.idempotency_key());
        assert_eq!(decoded.caller_key(), posting.caller_key());
    }

    #[test]
    fn foreign_keyed_postings_fail_closed_in_the_codec() {
        // A posting whose key was minted outside Ledger::post carries no
        // raw caller key: its rendered form is a one-way hash, so the
        // codec refuses it rather than restoring a different key.
        let posting = Posting::new(
            AccountId::new("cash-usd").unwrap(),
            AccountId::new("revenue").unwrap(),
            MonetaryAmount::new(1, crate::Currency::USD),
            "foreign",
            key(9),
            1,
        )
        .unwrap();
        let err = decode_posting(&encode_posting(&posting).unwrap()).unwrap_err();
        assert!(err.to_string().contains("cannot be re-derived"), "{err}");
    }

    #[test]
    fn truncated_payloads_fail_closed() {
        let encoded = encode_posting(&sample(5)).unwrap();
        for cut in [0usize, 8, 15, 24, 27, encoded.len() - 1, encoded.len() - 3] {
            let err = decode_posting(encoded.get(..cut).unwrap_or(&encoded)).unwrap_err();
            assert!(
                err.to_string().contains("truncated")
                    || err.to_string().contains("framing")
                    || err.to_string().contains("length"),
                "cut {cut}: {err}"
            );
        }
    }

    /// String-field start offsets in the encoded framing, mirroring the
    /// encoder's layout.
    fn string_offsets(posting: &Posting) -> [usize; 6] {
        let mut cursor = 8 + 16;
        let mut starts = [0usize; 6];
        for (i, field) in [
            posting.debit().as_str(),
            posting.credit().as_str(),
            posting.amount().currency.code(),
            posting.amount().amount.to_string().as_str(),
            posting.memo(),
            posting.caller_key().unwrap_or_default(),
        ]
        .into_iter()
        .enumerate()
        {
            cursor += 2;
            if let Some(slot) = starts.get_mut(i) {
                *slot = cursor;
            }
            cursor += field.len();
        }
        starts
    }

    #[test]
    fn corrupted_fields_fail_validation() {
        let posting = sample(5);
        let encoded = encode_posting(&posting).unwrap();
        let offsets = string_offsets(&posting);

        // Flip the currency code to garbage.
        let mut corrupted = encoded.clone();
        for (i, byte) in b"XXX".iter().enumerate() {
            *corrupted.get_mut(offsets[2] + i).unwrap() = *byte;
        }
        let err = decode_posting(&corrupted).unwrap_err();
        assert!(err.to_string().contains("unknown currency"), "{err}");

        // Flip the amount's sign digit: the decoded posting is
        // re-validated and a non-positive amount is rejected.
        let mut corrupted = encoded.clone();
        *corrupted.get_mut(offsets[3]).unwrap() = b'-';
        let err = decode_posting(&corrupted).unwrap_err();
        assert!(err.to_string().contains("strictly positive"), "{err}");

        // Flip a debit byte into an invalid account-id character.
        let mut corrupted = encoded;
        *corrupted.get_mut(offsets[0]).unwrap() = b'X';
        let err = decode_posting(&corrupted).unwrap_err();
        assert!(err.to_string().contains("invalid account id"), "{err}");
    }

    #[tokio::test]
    async fn append_mirrors_then_inner_append_holds() {
        let journal = OutboxJournal::new();
        let posting = sample(1_000);
        journal.append(&posting).await.unwrap();
        assert_eq!(journal.inner().len(), 1);
        assert_eq!(
            journal.posting_by_id(&posting.id()).await.unwrap(),
            Some(posting.clone())
        );
        assert_eq!(journal.all_postings(usize::MAX).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn restore_replays_the_outbox_into_a_fresh_journal() {
        let outbox: Arc<dyn OutboxStore> = Arc::new(MemoryStore::new());
        let journal =
            OutboxJournal::with_inner_and_store(MemoryJournal::new(), Arc::clone(&outbox));
        let p1 = sample(1_000);
        let p2 = {
            let key = IdempotencyKey::derive(crate::ledger::POSTING_SCOPE, b"refund-1")
                .expect("scope is valid");
            Posting::new(
                AccountId::new("revenue").unwrap(),
                AccountId::new("cash-usd").unwrap(),
                MonetaryAmount::new(3, crate::Currency::USD),
                "refund",
                key,
                2_000,
            )
            .unwrap()
            .with_caller_key("refund-1")
        };
        journal.append(&p1).await.unwrap();
        journal.append(&p2).await.unwrap();

        // "Restart": a fresh fast journal over the SAME durable store.
        let recovered = OutboxJournal::with_inner_and_store(MemoryJournal::new(), outbox);
        let restored = recovered.restore().await.unwrap();
        assert_eq!(restored, 2);
        assert_eq!(recovered.inner().len(), 2);
        assert_eq!(
            recovered
                .posting_by_id(&p1.id())
                .await
                .unwrap()
                .unwrap()
                .amount(),
            p1.amount()
        );

        // Restore is idempotent: the envelopes are dispatched now.
        let again = recovered.restore().await.unwrap();
        assert_eq!(again, 0);
        assert_eq!(recovered.inner().len(), 2);
    }

    #[tokio::test]
    async fn restore_of_a_corrupt_envelope_fails_closed() {
        let outbox: Arc<dyn OutboxStore> = Arc::new(MemoryStore::new());
        let event = OutboxEvent {
            id: EventId::from_uuid(PostingId::now_v7().as_uuid()),
            topic: OUTBOX_TOPIC.to_owned(),
            payload: vec![1, 2, 3],
            headers: std::collections::BTreeMap::new(),
            created_at: 0,
            attempts: 0,
            last_error: None,
        };
        outbox.append(&event).await.unwrap();
        let journal = OutboxJournal::with_inner_and_store(MemoryJournal::new(), outbox);
        let err = journal.restore().await.unwrap_err();
        assert!(err.to_string().contains("truncated"), "{err}");
        assert_eq!(journal.inner().len(), 0);
    }
}
