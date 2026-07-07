# Roadmap.md — **UltraCortex** Sequenced Build Plan

**Status:** v1.0 — Sequenced Build Plan (supersedes HyperCortex v1.0; Roadmap.md v0.1)
**Owner:** Dominic Sarria-Wiley
**Companion documents:** Architecture.md v1.0 (source of truth for *what*), HANDOFF.md v1.0 (live gap state), NATIVE_TRINITY.md v1.0.
**Build style:** Deterministic-build sprint. HALT-batched. Human approval gates between batches. SPEC-DERIVED traceability end-to-end.

---

## §0 — Document Conventions

- **HALT batches** are atomic units of work. Each batch ends with a hard HALT requiring explicit human approval before the next batch begins.
- Every deliverable is tagged with **SPEC-DERIVED-§N.N** markers.
- Every batch declares which **GAPs** it closes, advances, or formally defers.
- A batch is not "done" until: (1) all deliverables merged, (2) all exit criteria pass, (3) **CongruenceCell** reports zero unaccepted deltas, (4) human approves the HALT.
- **MUST / SHOULD / MAY** follow RFC 2119.
- Estimates are **calendar-day ranges for one focused engineer + AI pair**.

---

## §1 — Build Philosophy

1. **Architecture first, code second.**
2. **Trinity first, applications second.** Phase 1A includes four foundational Trinity Cells alongside the basic memory Cells. No code is written that the substrate cannot anchor and audit.
3. **Conformance before optimization.**
4. **One binary, every phase.**
5. **Determinism is non-negotiable.**
6. **HALT means halt.**
7. **DeepSeek prefix-cache hit-rate is a Phase 1B exit criterion.** No exception.

---

## §2 — Phase Map (Top-Level)

| Phase | Theme | Batches | Closes GAPs | Target Window |
|-------|-------|---------|-------------|---------------|
| **0**  | Scaffolding & Cell primitive | B0.1 – B0.4 | partial GAP-002 | Days 1–10 |
| **1A** | Core Cells + **4 foundational Trinity Cells** | B1A.1 – B1A.5 | GAP-NT-002, GAP-NT-008, GAP-NT-009 | Days 11–28 |
| **1B** | Index Cells + Reranker + Quarantine + WorkBudget + **DeepSeek prefix-cache bench** | B1B.1 – B1B.6 | GAP-004, GAP-NT-005, GAP-NT-007, GAP-NT-010, GAP-NT-014, GAP-DS-001 | Days 29–56 |
| **1C** | Protocol surface + MCP transport + **DeepSeek FIM/R1 extensions** | B1C.1 – B1C.4 | GAP-006, GAP-DS-002, GAP-DS-003, GAP-DS-004 | Days 57–72 |
| **1D** | Multi-agent: Subscription / AgentRegistry / Proposal / Playbook / Scratchpad | B1D.1 – B1D.4 | GAP-011 | Days 73–90 |
| **1E** | Persistence hardening: encryption tiers, snapshots, recovery, audit signing | B1E.1 – B1E.4 | GAP-009, GAP-012, GAP-NT-006, GAP-NT-012 | Days 91–108 |
| **1F** | Observability, full token-efficiency acceptance bench, DeepSeek hit-rate gate | B1F.1 – B1F.4 | GAP-010, GAP-013, GAP-NT-004, GAP-NT-011 | Days 109–126 |
| **2+** | Federation, WASM-hosted Cells, runtime rebalancing | deferred | GAP-001, GAP-003, GAP-014 | Out of scope |

---

## §3 — Phase 0: Scaffolding & Cell Primitive

### B0.1 — Repository & Workspace Bootstrap
**SPEC-DERIVED:** §0, §1.
**Deliverables:**
- Cargo workspace: `ultracortex-core`, `ultracortex-cell`, `ultracortex-wal`, `ultracortex-router`, `ultracortex-proto`, `ultracortex-trinity`, `ultracortex-cli`, `ultracortex-test`.
- CI: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`, `cargo deny`.
- All v1.0 docs checked in at repo root.
- `CONGRUENCE.md` present.

**Exit:** `cargo build` clean; CI green; CongruenceCell (test stub) confirms doc presence.

**HALT — B0.1 → B0.2**

### B0.2 — The Cell Trait + In-Memory Test Cell
**SPEC-DERIVED:** §5.1–§5.3.
**Deliverables:** `Cell` trait, `PreValidator` sub-trait, an in-memory `EchoCell`, determinism harness.
**Exit:** EchoCell passes determinism replay test (10k messages, byte-identical output).

**HALT — B0.2 → B0.3**

### B0.3 — Per-Shard WAL + Group Commit
**SPEC-DERIVED:** §4.2, PersistenceLayer.md §3.
**Deliverables:** WAL writer with group-committed fsync; frame format per PersistenceLayer.md §3.2; replay path.
**Exit:** crash-replay test; group-commit fan-in ≥ 16 frames p50 under synthetic load.

**HALT — B0.3 → B0.4**

### B0.4 — Shard Topology + Lock-Free Inboxes
**SPEC-DERIVED:** §4.2, RouterScheduler.md §2.
**Deliverables:** core-pinned shard threads; SPSC inboxes; backpressure plumbing.
**Exit:** synthetic throughput bench ≥ 100k msg/s/shard; no `Mutex` on hot path.

**HALT — Phase 0 → Phase 1A**

---

## §4 — Phase 1A: Core Cells + Foundational Trinity

### B1A.1 — ContractCell
**SPEC-DERIVED:** CellTaxonomy.md §22, NATIVE_TRINITY.md §10.
**Deliverables:** ContractCell impl; schema registry; `pre_validate(schema)`.
**Exit:** all bootstrap schemas register; non-conforming writes are quarantined.
Local status: the live checkout now also includes ContractCell plan/apply/verify migration tooling with Decision linkage and deadline-gated deprecation for breaking schema upgrades.

**HALT — B1A.1 → B1A.2**

### B1A.2 — SpecAnchorCell + DecisionLedgerCell
**SPEC-DERIVED:** NATIVE_TRINITY.md §4, §5.
**Deliverables:** both Cells; anchor extraction from doc corpus; append-only ledger; `pre_validate` hooks.
**Exit:** every `SPEC-DERIVED-§` in the v1.0 corpus has an anchor; a synthetic decision conflict is detected.

**HALT — B1A.2 → B1A.3**

### B1A.3 — CongruenceCell + GapCell
**SPEC-DERIVED:** NATIVE_TRINITY.md §6, §7.
**Deliverables:** CongruenceCell live matrix; GapCell with dispatch counter; fixation detector.
**Exit:** synthetic congruence delta blocks HALT; synthetic loop of N+1 envelopes triggers `task.no_progress`.

**HALT — B1A.3 → B1A.4**

### B1A.4 — CatalogCell + FactCell + TimelineCell + BlobCell
**SPEC-DERIVED:** CellTaxonomy.md §§2,3,4,10.
**Deliverables:** four memory/payload Cells; all register schemas with ContractCell; all writes pass the hook chain.
**Exit:** end-to-end write → recall round-trip; full hook chain ≤ 100 μs p99.

**HALT — B1A.4 → B1A.5**

### B1A.5 — Router + Pre-Validation Hook Chain
**SPEC-DERIVED:** RouterScheduler.md §10, NATIVE_TRINITY.md §11.1.
**Deliverables:** Router with capability check, intent routing, and full pre-validation chain; QuarantineCell stub.
**Exit:** every Trinity event audited; chain ordering matches spec; chain latency ≤ 100 μs p99.

**HALT — Phase 1A → Phase 1B**

---

## §5 — Phase 1B: Indexes + Quarantine + Budget + DeepSeek Bench

### B1B.1 — QuarantineCell + WorkBudgetCell (Trinity completion)
**SPEC-DERIVED:** NATIVE_TRINITY.md §8, §9.
**Exit:** budget exhaustion never loops; every failed envelope lands in QuarantineCell.

**HALT — B1B.1 → B1B.2**

### B1B.2 — VectorCell (DeepSeek-aligned 768/1536-d)
**SPEC-DERIVED:** EmbeddingReranker.md §2.
**Exit:** dual-index HNSW; chunking 512/1024 + 64 overlap; ANN k=10 p99 ≤ 1 ms.

**HALT — B1B.2 → B1B.3**

### B1B.3 — BM25Cell + GraphCell + RerankerCell + Hybrid Retrieval
**SPEC-DERIVED:** EmbeddingReranker.md §3, §4.
**Exit:** RRF fusion; reranker determinism; hybrid recall@10 ≥ 0.85 on the benchmark.

**HALT — B1B.3 → B1B.4**

### B1B.4 — CacheCell + PrefixCacheStore (L0)
**SPEC-DERIVED:** PersistenceLayer.md §9, RouterScheduler.md §9.
**Exit:** view assembly cache-hit ≤ 20 μs p99; cache-miss ≤ 500 μs p99.

**HALT — B1B.4 → B1B.5**

### B1B.5 — SummarizerCell (closed in UltraCortex v1.0; **[GAP-NT-013]**)
**SPEC-DERIVED:** EmbeddingReranker.md §6.
**Exit:** skeleton-from-body deterministic; conformance test passes.

**HALT — B1B.5 → B1B.6**

### B1B.6 — **DeepSeek Acceptance Bench** (the gate)
**SPEC-DERIVED:** Architecture.md §14.7, DeepSeekOptimization.md §11.
**Deliverables:**
- multi-step coding agent benchmark harness,
- token-injected-per-step measurement (target ≤ 1.5 KB p50, ≤ 4 KB p99),
- prefix-cache hit-rate measurement (target ≥ 80%),
- hydration ratio measurement (target ≤ 0.25 hydrate per recall).

**Exit:** all three targets met or formally deferred with a recorded Decision.

Local status: `tests/acceptance_bench.rs` now satisfies this gate in the current checkout; see `docs/benchmarks/deepseek_acceptance_2026-07-07.json`.

**HALT — Phase 1B → Phase 1C**

---

## §6 — Phase 1C: Protocol Surface + DeepSeek Extensions

### B1C.1 — MCP-over-UDS + envelope format
**SPEC-DERIVED:** McpProtocol.md §2, §3.
**Exit:** all six verbs round-trip; envelope determinism replay passes.

### B1C.2 — MCP-over-TCP + mTLS
**SPEC-DERIVED:** McpProtocol.md §2.2.
**Closes:** GAP-006.
Local status: closed in the current checkout by a fail-closed transport policy. Plaintext TCP is allowed only on loopback, and bootstrap/proto tests now reject non-loopback listener addresses before the node serves.

### B1C.3 — DeepSeek FIM framing + function-call grammar
**SPEC-DERIVED:** McpProtocol.md §6.1, §6.3.
**Closes:** GAP-DS-002, GAP-DS-004.

### B1C.4 — R1 think-strip + seed propagation
**SPEC-DERIVED:** McpProtocol.md §6.2, §6.4.
**Closes:** GAP-DS-003.

**HALT — Phase 1C → Phase 1D**

---

## §7 — Phase 1D: Multi-Agent

### B1D.1 — SubscriptionCell + fan-out
### B1D.2 — AgentRegistryCell + capability token issuance
### B1D.3 — ProposalCell + quorum
**Closes:** GAP-011.
### B1D.4 — PlaybookCell + ScratchpadCell

**HALT — Phase 1D → Phase 1E**

---

## §8 — Phase 1E: Persistence Hardening

### B1E.1 — Encryption tiers T0–T3
**Closes:** GAP-009.
Local status: closed in the current checkout. T3 now opens with a persisted local keyring, surfaces audited `kms status` / `kms rotate` admin verbs, and the rotation drill proves pre/post-roll seal/unseal continuity plus retained-key verification.
### B1E.2 — Snapshot pause-window bound
**Closes:** GAP-012.
Local status: closed in the current checkout. `ultracortex snapshot` now reports the checkpoint pause window plus the slower post-pause phases separately and raises `snapshot.pause_target_exceeded` above `50_000 µs`; the representative bench artifact `docs/benchmarks/snapshot_pause_2026-07-07.json` recorded `562 µs` max.
### B1E.3 — Quarantine retention horizon
**Closes:** GAP-NT-006.
### B1E.4 — Audit signing key custody
**Closes:** GAP-NT-012.
Local status: closed in the current checkout. CrossCheck batch signatures now persist key ids plus sidecar metadata, recovery verifies every completed batch before MCP open, and `audit verify` reports the expected/verified batch counts plus signature integrity.

**HALT — Phase 1E → Phase 1F**

---

## §9 — Phase 1F: Observability + Final Acceptance

### B1F.1 — OTel exporter defaults
**Closes:** GAP-013.
Local status: docs name OTLP export, but `src/obs.rs` still stops at in-process Metrics/Logger/AuditChain, so this gap remains open.
### B1F.2 — Congruence delta acceptance UI
**Closes:** GAP-NT-004.
### B1F.3 — Severity tag propagation across cross-cell calls
**Closes:** GAP-NT-011.
### B1F.4 — Final token-efficiency acceptance + DeepSeek hit-rate gate
**SPEC-DERIVED:** DeepSeekOptimization.md §11, Architecture.md §14.7.
**Closes:** GAP-010.

Local status: the same acceptance bench now clears the final v1.0 token-efficiency gate in this checkout; see `docs/benchmarks/deepseek_acceptance_2026-07-07.json`.

**HALT — v1.0 RELEASE**

---

## §10 — Cross-Document Congruence

Every batch's "done" criterion includes a green CongruenceCell report. The CongruenceCell is itself delivered in B1A.3, so from Phase 1A onward, the system enforces its own congruence — no external script needed.

_End of Roadmap.md v1.0 (UltraCortex)._


---

# 🆙 UltraCortex v1.0 Delta — Phase 1G: Curator Pair + Adjudication

The HyperCortex roadmap above (Phases 0 → 1F) remains normative for UltraCortex v1.0. UltraCortex adds one new phase after Phase 1F.

## §A.1 Phase 1G — Curator Pair + Adjudication Layer (NEW)

**Goal:** Ship the four Curator Cells (LibrarianCell, WardenCell, AdjudicatorCell, CrossCheckLedgerCell) and meet the v1.0 acceptance gate.

**Pre-requisites:**
- Phase 1F complete (Observability + final acceptance gates green).
- All Trinity Cells live and audited.
- DeepSeek prefix-cache hit rate ≥ 80% confirmed.

**Target window:** Days 127–168 (~6 weeks).

### B1G.1 — ContractCell Weight-Pinning Extension
**SPEC-DERIVED:** PersistenceLayer §A.1, BootstrapOperator §A.2.
**Deliverables:** Extend ContractCell to pin model SHA-256s; verify-on-load; require Decision record for swaps.
**Exit:** Synthetic model swap without Decision → quarantine.

### B1G.2 — Capability-Token Negation Glob
**SPEC-DERIVED:** McpProtocol §A.3, CURATOR_PAIR_PROTOCOL §4.
**Deliverables:** Extend FacetGlob with `!exclude_glob` syntax; Router rejects matching hydrate; emits `curator.rationale_access_denied`.
**Closes:** GAP-CU-012.
**Exit:** Conformance test #1 in CURATOR_PAIR_PROTOCOL §8 passes.

### B1G.3 — CrossCheckLedgerCell
**SPEC-DERIVED:** CrossCheckLedgerCell.md, PersistenceLayer §A.2.
**Deliverables:** Dedicated WAL stream; KMS signing at T2+; five indices (by_initiator, by_auditor, by_outcome, by_adjudicator, by_audit_kind); derived metrics.
**Closes:** GAP-CU-009 (retention).
**Exit:** 1M synthetic records audit-chain-verifiable.

### B1G.4 — LibrarianCell (Gemma 2 2B)
**SPEC-DERIVED:** LibrarianCell.md.
**Deliverables:** Gemma 2 2B Q4_K_M mmap + RAM KV cache; async on `node.written`; PUBLIC/PRIVATE output split; subject to Trinity governance.
**Closes:** **GAP-NT-013** (SummarizerCell subsumed), GAP-CU-001.
Local status: Librarian behavior closes GAP-NT-013 locally, but the production Gemma default remains open because this checkout still uses `DeterministicBackend` unless operators configure and pin the optional GGUF seam.
**Exit:** Skeleton generation deterministic under WAL replay; ~180 ms p50 CPU.

### B1G.5 — WardenCell (Qwen 2.5 Coder 1.5B)
**SPEC-DERIVED:** WardenCell.md.
**Deliverables:** Qwen 2.5 Coder 1.5B Q4_K_M mmap + RAM KV cache (different family verified); opt-in sync via `flags.semantic_check`; auto-sync on `severity=P0`; async audit of Librarian outputs.
**Closes:** GAP-CU-002.
Local status: Warden behavior exists locally, but the production Qwen default remains open because the live Warden path is still deterministic evidence checks and no Qwen/GGUF backend is wired into `WardenCell` yet.
**Exit:** Audit-the-Librarian and sanity-check-the-Warden flows pass conformance tests.

### B1G.6 — AdjudicatorCell + Rotating LLM Pool
**SPEC-DERIVED:** AdjudicatorCell.md.
**Deliverables:** Deterministic policy table (Rust); rotating LLM pool (Phi-3.5, Llama 3.2, SmolLM-2); rotation seeded by `envelope.seed`; human-escalation path.
**Closes:** GAP-CU-007, GAP-CU-008.
**Exit:** ≥70% deterministic resolution rate on synthetic disagreement corpus.

### B1G.7 — Curator Pair Acceptance Bench (v1.0 RELEASE GATE)
**SPEC-DERIVED:** CURATOR_PAIR_PROTOCOL.md §8, ObservabilityAudit §A.6.

**The nine release-gate tests:**
1. PUBLIC/PRIVATE separation enforced.
2. `curator.rationale_access_denied` non-zero under load.
3. Independent grounding required from auditors.
4. Adversarial probe pass-rate ≥ 95%.
5. Mandatory disagreement quota detection (suspicious-agreement fires at >99%).
6. Blind re-audit byte-deterministic.
7. Calibration drift trip → degraded mode.
8. Adjudicator cannot read CrossCheckLedger prior decisions.
9. Trinity-governs-Curator: synthetic Librarian fixation → GapCell catches it.

**Exit:** All nine tests green. Disagreement quota observed at 92–97% over benchmark.

**HALT — UltraCortex v1.0 RELEASE.**

## §A.2 Phase Map (Updated)

| Phase | Theme | Window | Status |
|---|---|---|---|
| 0–1F | HyperCortex v1.0 phases | Days 1–126 | inherited |
| **1G** | **Curator Pair + Adjudication (NEW)** | **Days 127–168** | **NEW v1.0** |
| 2+ | Federation, WASM, runtime rebalancing | deferred | — |

## §A.3 New GAPs Closed in Phase 1G

GAP-NT-013 · GAP-CU-007 · GAP-CU-008 · GAP-CU-009 · GAP-CU-012

## §A.4 Congruence Contract (Updated)

Congruent with: Architecture-UltraCortex.md (all §s), **CURATOR_PAIR_PROTOCOL.md**, **LibrarianCell.md**, **WardenCell.md**, **AdjudicatorCell.md**, **CrossCheckLedgerCell.md**, HANDOFF.md (full GAP-CU register).

_End of UltraCortex v1.0 Delta._
