# UltraCortex

UltraCortex is a Rust implementation of a self-policing shared-memory
substrate for multi-agent AI systems: one binary, one data directory,
and zero external dependencies. Agents communicate over a CBOR wire
protocol, every state change passes through a governance chain, and
stored knowledge is continuously checked by an intentionally asymmetric
curator pair.

The project is grounded in the v1.0 blueprint corpus under `docs/`.
Implementation coverage, deviations, and known gaps are tracked in
`IMPLEMENTATION_STATUS.md`.

## What it is trying to do

- Keep memory governance native to the substrate instead of bolting it
  on afterward.
- Require every read and write to declare intent, capability, and work
  budget.
- Make curator collusion structurally difficult through asymmetric
  visibility and substrate-policed oversight.
- Stay operationally simple: one Rust binary, standard library only,
  local data directory, no external services required.

## Current status

- Version: `1.0.0-alpha.1`
- Package: `ultracortex`
- Toolchain target: Rust `1.75+`
- License field: `LicenseRef-TBD`

> Honesty note: this tree was authored in an environment without a Rust
> toolchain, so it has not been compile-verified here yet. The included
> tests use known vectors for hashing, HMAC, CRC32C, and canonical CBOR,
> so the first real `cargo test` run is a meaningful readiness gate.

## Build and test

```bash
cargo build --release
cargo test
cargo fmt --check
```

## Run

```bash
./target/release/ultracortex run
./target/release/ultracortex run --config path.toml --set listen.tcp=127.0.0.1:7741
./target/release/ultracortex run --dry-run
```

A healthy boot prints:

```text
ready node_id=ultracortex-0 proto_version=1
```

Boot order is Trinity-first and fatal: Contract, SpecAnchor,
DecisionLedger, Gap, Congruence, WorkBudget/Quarantine, curators, then
the rest of the node plus an eleven-point self-test through the real
router. A node that cannot police itself refuses to serve.

## Protocol model

UltraCortex uses CBOR envelopes over `u32-LE length || body` frames.
Every envelope, including reads, must include a `work_budget`, a
capability token, and an intent. State changes also require a
`spec_anchor` that cites the governing document section.

Core verbs:

- `recall`
- `hydrate`
- `write`
- `subscribe`
- `view`
- `supersede`

Governed rejections are quarantined rather than dropped, and responses
can carry `tokens_emitted`, `next_tier_hint`, and `quarantine_id`.

## Operator commands

```bash
ultracortex status
ultracortex snapshot
ultracortex quarantine list
ultracortex gap list
ultracortex audit verify
ultracortex congruence audit
ultracortex contract list
ultracortex curator status
ultracortex cross-check tail
ultracortex adjudicator stats
ultracortex metrics
ultracortex shutdown
```

`curator status` is the collusion dashboard. The Librarian/Warden
agreement band should stay in the documented range, `rationale_access_denied`
should remain non-zero, and `probe_missed` should stay at zero.

## Repository layout

```text
src/core        ids, errors, ULID, canonical CBOR, glob, TOML subset, crypto
src/obs         metrics, structured log, hash-chained audit log
src/persist     sharded WAL, CAS blobs, CoW snapshots, KMS T0-T2, view cache
src/cells       memory, index, and coordination cells
src/trinity     governance cells and pre-validation chain
src/curator     Librarian, Warden, Adjudicator, guardrails, cross-check ledger
src/router      capability tokens, envelopes, views, events, dispatch
src/proto       wire framing, listeners, blocking client
src/bootstrap   operator boot, recovery, self-test, admin plane
tests/          conformance coverage for trinity and curator behavior
docs/           22-document v1.0 specification corpus
```
