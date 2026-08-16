# GAP_CLOSURE_CHECKLIST.md — UltraCortex v1.0 Remaining Gap Closure Checklist

**Status date:** 2026-08-13
**Scope:** Every `open`, `deferred`, or `in_progress` row in `docs/HANDOFF.md`.

This document translates the remaining gap register into the minimum evidence needed to move a gap to `closed`.

Second-pass audit note (2026-08-13):
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

### GAP-013 — OTel exporter default endpoints (closed)
Audit evidence (2026-08-13): `OtlpConfig` and `OtlpExporter` implement dependency-free OTLP/HTTP JSON export with conventional localhost defaults, config/env/CLI overrides, and `metrics export` operator wiring; the evidence artifact is `docs/benchmarks/curator_observability_defaults_2026-08-13.json`.
- [x] Decision: default to `http://127.0.0.1:4318/v1/{metrics,traces,logs}` with explicit override and best-effort export semantics.
- [x] Code: implement the exporter path in `src/obs.rs` and wire it through bootstrap, config, and the admin CLI.
- [x] Proof: `otlp_metrics_smoke_posts_json_to_loopback_collector` emits and receives a real HTTP request on a loopback collector.
- [x] Docs: update `ObservabilityAudit.md`, `README.md`, and `HANDOFF.md`.

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

### GAP-CU-001 — Librarian default model (closed)
Audit evidence (2026-08-13): production config selects `gemma-2-2b-it-q4_k_m`, verifies the pinned SHA-256 weight file, and wires the GGUF backend through `LibrarianCell`; development mode is explicit. The evidence artifact is `docs/benchmarks/curator_observability_defaults_2026-08-13.json`.
- [x] Decision: ratify Gemma 2 2B Instruct Q4_K_M as the production Librarian default.
- [x] Code: make the pinned default runtime real in bootstrap/config; missing runner or weights fails closed instead of silently selecting deterministic mode.
- [x] Proof: `production_curator_defaults_are_pinned_and_family_distinct` plus bootstrap self-test model-selection validation cover the pin and runtime mode.
- [x] Docs: update `LibrarianCell.md`, `BootstrapOperator.md`, and `HANDOFF.md`.

### GAP-CU-002 — Warden default model (closed)
Audit evidence (2026-08-13): production config selects the distinct `qwen2.5-coder-1.5b-q4_k_m` family, verifies its pinned SHA-256 weight file, and injects the backend into `WardenCell`; the bootstrap self-test rejects a non-GGUF production backend. The evidence artifact is `docs/benchmarks/curator_observability_defaults_2026-08-13.json`.
- [x] Decision: ratify Qwen 2.5 Coder 1.5B Q4_K_M as the production Warden default.
- [x] Code: make the pinned default runtime real in bootstrap/config and use the configured backend for the semantic Librarian audit pass.
- [x] Proof: `production_curator_defaults_are_pinned_and_family_distinct`, `WardenCell` backend wiring, and bootstrap self-test model-selection validation cover the family distinction and runtime mode.
- [x] Docs: update `WardenCell.md`, `BootstrapOperator.md`, and `HANDOFF.md`.

_End of GAP_CLOSURE_CHECKLIST.md._
