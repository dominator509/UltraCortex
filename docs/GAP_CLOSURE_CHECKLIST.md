# GAP_CLOSURE_CHECKLIST.md — UltraCortex v1.0 Remaining Gap Closure Checklist

**Status date:** 2026-07-07
**Scope:** Every `open`, `deferred`, or `in_progress` row in `docs/HANDOFF.md`.

This document translates the remaining gap register into the minimum evidence needed to move a gap to `closed`.

Second-pass audit note (2026-07-07):
- The DeepSeek section is intentionally empty because every `GAP-DS-*` row is now closed in `docs/HANDOFF.md`.
- The remaining entries below were re-checked against the current checkout; they are real open gaps, not stale notes.
- One stale wording issue was corrected during this pass: the current checkout records T2+ CrossCheck batch HMACs, not externally custodied Ed25519 audit signatures.

Closure legend:
- `Decision` = a policy/default/phase choice that must be ratified explicitly.
- `Code` = missing or incomplete implementation work.
- `Proof` = the test, benchmark, recovery drill, or audit artifact required to support closure.
- `Docs` = source-of-truth updates required in the same change set.

Deferred gaps are still real gaps. Their checklists describe what would be needed to close them when the project moves beyond the current v1.0 scope.

## Carryover Gaps

### GAP-001 — Rebalancing (deferred)
Audit evidence (2026-07-07): no rebalancing path exists in `src/`; the checkout is still single-binary, static-shard only.
- [ ] Decision: choose the Phase 2 rebalancing model and rollback semantics.
- [ ] Code: implement shard/cell migration with catalog updates, WAL continuity, and reinjection safety.
- [ ] Proof: add a multi-shard rebalance test proving deterministic replay and no lost writes before/after movement.
- [ ] Docs: update `Architecture.md`, `Roadmap.md`, and `HANDOFF.md` with the rebalancing contract and operator path.

### GAP-002 — Final allocator selection (open)
Audit evidence (2026-07-07): `Cargo.toml` and `src/` still contain no allocator hook such as `#[global_allocator]`, jemalloc, mimalloc, or build-time allocator switch.
- [ ] Decision: ratify the production allocator choice and whether it is unconditional or tier/config dependent.
- [ ] Code: wire the allocator selection into startup/build configuration.
- [ ] Proof: record allocator comparison data for throughput, latency, and memory footprint on representative workloads.
- [ ] Docs: update `Architecture.md`, `PersistenceLayer.md`, and `HANDOFF.md` with the chosen default and rationale.

### GAP-003 — WASM-hosted Cells (deferred)
Audit evidence (2026-07-07): no WASM host/runtime boundary exists in `src/`; the feature remains roadmap-only.
- [ ] Decision: choose the WASM host ABI, capability limits, and determinism contract.
- [ ] Code: implement a WASM cell runtime boundary and lifecycle integration.
- [ ] Proof: add conformance coverage for host/guest determinism, schema enforcement, and quarantine on guest failure.
- [ ] Docs: update `Architecture.md`, `CellTaxonomy.md`, `Roadmap.md`, and `HANDOFF.md`.

### GAP-004 — Hybrid retrieval ranking weights (open)
Audit evidence (2026-07-07): `handle_recall` still unions BM25 + Vector candidates and reranks them; GraphCell and the documented `(vector, bm25, graph)` defaults are not wired into the live Router path.
- [ ] Decision: ratify the default `(vector, bm25, graph)` weights and the override policy.
- [ ] Code: ensure the chosen defaults and override surface are exposed as the supported configuration seam.
- [ ] Proof: commit the acceptance bench showing the chosen weights satisfy the target retrieval metric on the benchmark corpus.
- [ ] Docs: update `EmbeddingReranker.md` and `HANDOFF.md` with the accepted weights and benchmark reference.

### GAP-005 — Cross-shard transactions (deferred)
Audit evidence (2026-07-07): writes route to exactly one shard by handle hash (`Node::wal_for`), and there is no transaction coordinator or cross-shard replay/rollback contract.
- [ ] Decision: choose the cross-shard consistency model and failure semantics.
- [ ] Code: implement the transaction protocol, replay rules, and rollback/compensation behavior.
- [ ] Proof: add crash/recovery tests covering commit, partial failure, and replay ordering across shards.
- [ ] Docs: update `Architecture.md`, `PersistenceLayer.md`, `Roadmap.md`, and `HANDOFF.md`.

### GAP-009 — T3 key-rotation cadence (closed)
Audit evidence (2026-07-07): T3 now opens locally, persists custody state in `kms/keyring.cbor`, exposes audited `kms status` / `kms rotate [--emergency]` admin verbs, and the recorded drill artifact `docs/benchmarks/kms_rotation_drill_2026-07-07.json` proves seal/unseal continuity plus auditable key-roll events.
- [x] Decision: define the T3 rotation schedule, trigger conditions, and emergency-rotation path (`T3_ROTATION_INTERVAL_OPS = 1_000_000` plus `--emergency` override).
- [x] Code: integrate the T3/KMS path needed to exercise the cadence.
- [x] Proof: add a rotation drill showing seal/unseal continuity and auditable key-roll events.
- [x] Docs: update `PersistenceLayer.md`, `ObservabilityAudit.md`, `Roadmap.md`, `BootstrapOperator.md`, and `HANDOFF.md`.

### GAP-013 — OTel exporter default endpoints (open)
Audit evidence (2026-07-07): `src/obs.rs` implements only in-process Metrics/Logger/AuditChain; there is no OTLP exporter or default collector seam in the runtime.
- [ ] Decision: choose the supported default collector/exporter endpoints and override behavior.
- [ ] Code: implement the OTel exporter path rather than documentation-only references.
- [ ] Proof: add a smoke/integration test that emits telemetry to the default collector configuration.
- [ ] Docs: update `ObservabilityAudit.md`, `README.md`, and `HANDOFF.md`.

### GAP-014 — Federation (deferred)
Audit evidence (2026-07-07): no multi-host state exchange or federation transport exists in `src/`; the feature remains deferred in the roadmap only.
- [ ] Decision: define the multi-host federation model, trust boundary, and conflict semantics.
- [ ] Code: implement cross-host state exchange and governance boundaries.
- [ ] Proof: add a multi-node conformance suite covering replay, quarantine, and decision consistency across hosts.
- [ ] Docs: update `Architecture.md`, `NATIVE_TRINITY.md`, `Roadmap.md`, and `HANDOFF.md`.

## Native Trinity Gaps

### GAP-NT-012 — Audit signing key custody (closed)
Audit evidence (2026-07-07): CrossCheck batch signatures now persist key ids and HMAC sidecar state, recovery reloads and verifies every completed batch before serving, and the recorded drill artifact `docs/benchmarks/kms_rotation_drill_2026-07-07.json` plus `audit verify` report the expected/verified batch counts and signature integrity.
- [x] Decision: define who owns audit-signing keys, where custody lives, and how rotation/recovery work (local persisted T3 keyring with audited rotation).
- [x] Code: integrate the custody model where required by the chosen tier.
- [x] Proof: run a custody/rotation drill and document the auditable evidence chain.
- [x] Docs: update `PersistenceLayer.md`, `ObservabilityAudit.md`, `CrossCheckLedgerCell.md`, `Roadmap.md`, and `HANDOFF.md`.

## DeepSeek Gaps

## Curator Pair Gaps

### GAP-CU-001 — Librarian default model (open)
Audit evidence (2026-07-07): `Node::new` uses `DeterministicBackend` unless operators configure both `[curator].external_cmd` and `pinned.librarian`.
- [ ] Decision: ratify the production default Librarian model family/size.
- [ ] Code: make the pinned default runtime real in bootstrap/config rather than falling back to deterministic-only default behavior.
- [ ] Proof: add startup/self-test coverage that verifies the default weight pin and model-family assumptions.
- [ ] Docs: update `LibrarianCell.md`, `BootstrapOperator.md`, and `HANDOFF.md`.

### GAP-CU-002 — Warden default model (open)
Audit evidence (2026-07-07): `WardenCell::new` takes no backend/model configuration, so the live Warden path is still deterministic evidence checks rather than a pinned Qwen runtime.
- [ ] Decision: ratify the production default Warden model family/size.
- [ ] Code: make the pinned default runtime real in bootstrap/config rather than deterministic fallback only.
- [ ] Proof: add startup/self-test coverage that verifies the Warden default and the model-family difference from the Librarian.
- [ ] Docs: update `WardenCell.md`, `BootstrapOperator.md`, and `HANDOFF.md`.

_End of GAP_CLOSURE_CHECKLIST.md._
