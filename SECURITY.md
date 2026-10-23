# Security Policy: ledger-kit

## Supported versions

| Version | Supported |
|---------|-----------|
| 0.1.x   | ✅        |

## Reporting a vulnerability

Report privately via [GitHub security advisories] for this repository, or
email **wyatt_au@protonmail.com**. Do **not** open a public issue for
security reports.

You will receive an acknowledgement within **72 hours**. Coordinated
disclosure: we ask for up to 90 days before public disclosure while a
patch ships.

## Scope notes

`ledger-kit` records double entry over money. Security considerations for
integrators:

- **Balances are derived, not stored.** Every balance answer is a fold
  over the journal at read time. An attacker who can write postings owns
  the balances; protect the journal. There is no stored balance whose
  tampering would (or would not) be detected: the history *is* the
  state.
- **The audit chain detects, not prevents.** The `audit` feature chains
  each accepted posting (captured verbatim in entry details) into a
  SHA-256 previous-hash chain. `verify_chain` proves the chain was not
  edited after the fact; nothing stops an attacker with write access from
  appending new entries or replacing the whole chain. Anchor the chain
  head externally (log export, checkpointing) for tamper evidence
  stronger than local trust.
- **Idempotency claims are TTL-bounded.** A replayed caller key answers
  `DuplicatePosting` only within the claim window (24 h in-memory; scale
  with the idempotency-kit store you inject). After expiry the same key
  can post again as a new operation: make callers treat
  `DuplicatePosting` as success and derive keys from natural request
  identities, not random nonces.
- **`RejectNegative` is debit-side by construction.** Under conservation
  (total of balances ≡ 0) a both-sides negative check could accept no
  posting; the policy bounds overdrafts on debited accounts, it is not a
  solvency control. Credit limits belong in the domain layer.
- **The outbox mirror is at-least-once.** `OutboxJournal::restore`
  replays undelivered envelopes after restart; the posting id deduplicates
  the journal, but downstream consumers of mirrored events must also be
  idempotent (natural keys, idempotency-kit).
- **Amounts are decimal, exact.** `MonetaryAmount` is decimal-money's
  `CurrencyAmount` (rust_decimal): no float rounding. Decode validates
  every field before a posting re-enters a journal; the codec fails
  closed on truncated or tampered payloads.
- **The in-memory journal is volatile.** Without the `outbox` feature (or
  a future durable `Journal`), postings vanish with the process. That is
  a feature for tests; do not ship it as your book of record.
- `#![forbid(unsafe_code)]`: no unsafe blocks exist in this crate.

[GitHub security advisories]:
    https://github.com/WyattAu/ledger-kit/security/advisories/new
