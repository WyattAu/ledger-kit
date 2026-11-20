# PATTERN_ADOPTION

How `ledger-kit` relates to the WyattAu pattern canon
(`WyattAu/pattern-standards`). Validated by
`pattern-standards/scripts/adoption-check.py`.

| Pattern | Status | Evidence |
|---|---|---|
| T27 | adopted | src/posting.rs:66 (the double-entry unit: debit moves to credit, debit != credit enforced at :120, strictly positive amount at :125 so the sign lives in the sides; balances are a pure fold over the append-only journal, src/balance.rs:16) |
| T32 | adopted | src/outbox.rs:43 (OutboxJournal mirrors each accepted posting as an outbox envelope whose id is the posting id; outbox-first write order per the module doc at src/outbox.rs:5, crash replay at :80) |
| T33 | adopted | src/ledger.rs:198 (posting claims a caller key plus a request-fingerprint key; a replayed key answers DuplicatePosting with the original posting id, failed attempts release claims at :256 so corrected retries proceed) |
| T28 | partial | src/ledger.rs:564 (accepted postings chained via tamper-audit, previous-hash to entry-hash with the posting verbatim in entry details; a linear hash chain, not a Merkle DAG, and chain heads are not externally anchored) |
| P60 | adopted | src/posting.rs:21 (PostingId is a UUIDv7, time-ordered so journal ids sort chronologically) |
| P64 | adopted | tests/properties.rs:63 (500-case generated balanced posting sets; trial_balance conservation and same-currency sums checked as invariants) |
| P12 | partial | src/ledger.rs:353 (currency consistency enforced at the posting boundary against the account registry, which transitively enforces same-currency debit/credit; the dimension-typed amount itself is decimal-money's CurrencyAmount, phantom units live there, not here) |
| P11 | n/a | canon baseline line names this repo; code shows no implementation of P11: no split or allocation API, postings move one whole amount (src/posting.rs:66); split math is billing-kit's declared surface |

## Refusals

The stored-balance approach the canon refuses (R4, mutable authority
columns) is refused here by construction, not by omission: no balance
field exists anywhere in the crate, balances are derived on read
(src/balance.rs:3). Trial balance proving net zero over any posting set
is the check that keeps the refusal honest.

## Near-miss patterns omitted

- P11 (remainder-preserving partition): no split or allocation API in
  this crate; postings move one whole amount. Split math is billing-kit's
  declared surface (its P11 row is planned there); ledger-kit consumes
  its results as single postings.
- T29 (MVCC / persistent snapshots): deliberately absent. Balances are
  derived per read and nothing keeps a snapshot between calls
  (src/balance.rs:67). The journal is append-only, so readers always
  fold a consistent history; there is no concurrent-writer tearing for
  snapshots to solve.
