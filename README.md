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
- Local validation: `cargo test` passes (`158 passed`, 7 suites).
- Acceptance bench: [`tests/acceptance_bench.rs`](tests/acceptance_bench.rs) records `856` bytes p50, `1052` bytes p99, `84.21%` prefix-cache hit rate, and `0.25` hydrate/recall ratio; the measured artifact lives at [`docs/benchmarks/deepseek_acceptance_2026-07-07.json`](docs/benchmarks/deepseek_acceptance_2026-07-07.json).
- Snapshot bench: [`tests/acceptance_bench.rs`](tests/acceptance_bench.rs) records `392 µs` p50 and `562 µs` p99/max for the admin snapshot pause window on a seeded Router workload; the measured artifact lives at [`docs/benchmarks/snapshot_pause_2026-07-07.json`](docs/benchmarks/snapshot_pause_2026-07-07.json).

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

TCP is intentionally loopback-only in the current checkout. Non-loopback
`listen.tcp` addresses are rejected before bind; UDS remains the preferred
transport.

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
ultracortex kms status
ultracortex kms rotate [--emergency]
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
`audit verify` now checks the audit hash chain plus completed CrossCheck
batch signatures, and the `kms` verbs expose the local T3 custody /
rotation seam.

## Repository layout

```text
src/core        ids, errors, ULID, canonical CBOR, glob, TOML subset, crypto
src/obs         metrics, structured log, hash-chained audit log
src/persist     sharded WAL, CAS blobs, CoW snapshots, KMS T0-T3, view cache
src/cells       memory, index, and coordination cells
src/trinity     governance cells and pre-validation chain
src/curator     Librarian, Warden, Adjudicator, guardrails, cross-check ledger
src/router      capability tokens, envelopes, views, events, dispatch
src/proto       wire framing, listeners, blocking client
src/bootstrap   operator boot, recovery, self-test, admin plane
tests/          conformance coverage for trinity and curator behavior
docs/           22-document v1.0 specification corpus
```
