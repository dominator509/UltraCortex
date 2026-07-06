# Architecture.md — **UltraCortex** Self-Policing Shared-Memory Substrate for Multi-Agent AI

**Status:** v1.0 Source-of-Truth | **Owner:** Dominic Sarria-Wiley | **Supersedes:** HyperCortex v1.0
**Language:** Rust (full stack) | **In-substrate LLMs:** Gemma 2 2B (Librarian) · Qwen 2.5 Coder 1.5B (Warden) · Phi-3.5 / Llama 3.2 / SmolLM-2 (Adjudicator pool)

## §0 Document Conventions
RFC 2119. SPEC-DERIVED-§N.N markers. GAP namespaces: GAP-NNN (carryover), GAP-NT-NNN (Trinity), GAP-DS-NNN (DeepSeek), GAP-CU-NNN (Curator, NEW). Live congruence via `CongruenceCell`.

## §1 Mission
UltraCortex = HyperCortex + a **self-policing semantic layer**. Where HyperCortex made drift/freeze/fixation structurally impossible, UltraCortex extends the same logic upward: **semantic drift and hallucination, including by in-substrate LLMs themselves, are detected at write time by a mutually-accountable Curator Pair governed by the same Trinity that governs every other Cell.**

One Rust binary delivering: microsecond recall, native-speed writes, deterministic replay, structurally impossible drift/freeze/fixation (Trinity §15), structurally impossible semantic drift/hallucination (Curator Pair + Adjudicator §16–§18), DeepSeek-optimized prefix-stable views (§14), uniform MCP-style protocol.

## §2 Design Principles
P1–P18 inherited from HyperCortex v1.0. **NEW:**

- **P19 — Asymmetric Visibility**: A Cell auditing another Cell MUST see substrate effects but MUST NOT see the producing Cell's rationale, chain-of-thought, considered alternatives, or precise confidence. (§16, §17)
- **P20 — Substrate-Policed Semantic Layer**: In-substrate LLMs (Librarian, Warden, Adjudicator pool) are subject to the same Trinity governance as every other Cell. No privileged path, no quarantine exemption, no fixation-detection exemption. (§16, §17)

## §3 Vocabulary (additions)
- **Curator Pair**: `LibrarianCell` (write-side async) + `WardenCell` (read-side opt-in judge).
- **Adjudicator**: `AdjudicatorCell` + rotating LLM pool — tie-breaker.
- **Cross-Check Record**: forensic record of every cross-check between in-substrate LLMs.
- **PUBLIC / PRIVATE output**: every Curator output split into PUBLIC (visible to auditors) and PRIVATE (rationale, CoT, alternatives, precise confidence — visible only to producer).

## §4 System Topology

```
L4 — Agent Fabric (DeepSeek V3/R1/Coder, GPT, Claude, local)
              │ MCP-over-uds / MCP-over-tcp
L3 — Protocol Surface (recall · hydrate · write · subscribe · view · supersede)
              │
L2 — Router/Scheduler (auth · budget · pre-validation chain · semantic_check gate)
              │
   ┌──────────┼──────────────┬─────────────────┐
   │          │              │                 │
L1 Memory  L1 Index       Trinity          CURATOR SHARDS (NEW)
shards     shards         Shard            Librarian (Gemma 2 2B)
                          7 cells          Warden (Qwen 2.5 1.5B)
                                           Adjudicator (pool)
                                           CrossCheckLedger
                                           ── NO SHARED STATE ──
                          │
L0 — Persistence (WAL · Snapshots · CAS · Manifest · KMS · PrefixCacheStore)
```

N shard threads = physical cores − 4 (reserves: Router, flusher, Librarian, Warden, Adjudicator shards). Each Curator Cell has its own mmap'd weights + own KV cache. Cross-shard communication via lock-free SPSC queues.

## §5 The Cell
Cell trait unchanged. **NEW invariant**: Curator Cells MUST split outputs into PUBLIC/PRIVATE per P19. Curator weight files pinned by SHA-256 in `ContractCell` — swapping a model is a Decision, never a silent merge.

### §5.3 Latency targets
| Op | p50 | p99 |
|---|---|---|
| Query | <10 μs | <100 μs |
| Update | <100 μs | <1 ms |
| Pre-validate (Trinity) | <5 μs | <25 μs |
| Curator inference (CPU Q4_K_M) | ~180 ms | ~400 ms |
| Adjudicator deterministic | <50 μs | <200 μs |
| Adjudicator LLM | ~300 ms | ~700 ms |

**Curator latency decoupled from hot path** — Librarian async on `node.written`; Warden sync only on `flags.semantic_check` or `severity=P0`.

## §6 Cell Taxonomy (25 Cells)
21 inherited from HyperCortex + 4 NEW Curator Cells:

| # | Cell | Category | Phase |
|---|---|---|---|
| 1–14 | Catalog, Fact, Timeline, Playbook, Scratchpad, Vector, Graph, BM25, Blob, Cache, AgentRegistry, Proposal, Subscription, Reranker | Memory/Index/Payload/Coord/Service | 1A–1D |
| 15–21 | SpecAnchor, DecisionLedger, Congruence, Gap, Quarantine, WorkBudget, Contract | Trinity | 1A–1B |
| **22** | **LibrarianCell** | **Curator** | **1G** |
| **23** | **WardenCell** | **Curator** | **1G** |
| **24** | **AdjudicatorCell** | **Curator** | **1G** |
| **25** | **CrossCheckLedgerCell** | **Curator** | **1G** |

Full table in CellTaxonomy.md. **Phase 1G is new in UltraCortex.**

## §7 Router / Scheduler
Full spec in RouterScheduler.md. NEW: `flags.semantic_check` envelope field → invokes WardenCell synchronously after Trinity pre-validation. Auto-sync on `severity=P0`. Async fan-out to LibrarianCell on `node.written`. Adjudicator escalation when Librarian/Warden disagree.

## §8 Persistence
Full spec in PersistenceLayer.md. NEW: Curator weight files in `weights/<model_id>/<sha256>.gguf`, pinned by ContractCell. CrossCheckLedger has its own WAL stream. Curator KV caches are RAM-only — NEVER persisted (prevents rationale leakage on restart).

## §9 MCP Protocol
Full spec in McpProtocol.md. NEW envelope field `flags.semantic_check: bool`. NEW error codes: `SemanticDrift`, `HallucinationDetected`, `AdjudicationPending`.

## §10 Embedding & Reranking
Full spec in EmbeddingReranker.md. Unchanged from HyperCortex v1.0.

## §11 Observability & Audit
Full spec in ObservabilityAudit.md. NEW audit events: `librarian.output_emitted`, `warden.judgment_emitted`, `warden.audit_disagreement`, `librarian.sanity_check_disagreement`, `adjudicator.invoked`, `adjudicator.resolution`, `curator.suspicious_agreement` (>99%), `curator.calibration_drift_detected`, `curator.probe_failed`, `curator.rationale_access_denied` (proves P19 active — MUST be non-zero).

## §12 Security
Capability tokens NOW support **rationale-exclusion negation glob**:
```
facet_scope: "librarian/output/*  !rationale.*  !considered_alts.*  !reasoning_trace.*"
```
The Warden literally cannot `hydrate` a Librarian rationale blob.

## §13 Federation
Phase 2+. [GAP-014].

## §14 DeepSeek Optimization Layer
Unchanged from HyperCortex v1.0. Full spec in DeepSeekOptimization.md. §14.1–§14.7 preserved verbatim. Curator Pair operates in parallel — Librarian's better skeletons further reduce tokens-injected-per-step below ≤1.5 KB p50, plausibly to ≤800 bytes p50.

## §15 Native Trinity Subsystem
Full spec in NATIVE_TRINITY.md. Unchanged EXCEPT:
- §15.9 pre-validation chain has an optional **post-chain WardenCell gate** triggered by `flags.semantic_check` or `severity=P0`.
- §15.7 GapCell tracks Curator dispatch counters — Librarian/Warden subject to fixation detection like any other Cell.
- §15.6 QuarantineCell has new causes: `SemanticDrift`, `HallucinationDetected`, `AdjudicationFailed`.

## §16 Curator Pair Subsystem (NEW)
Full normative spec in CURATOR_PAIR_PROTOCOL.md, LibrarianCell.md, WardenCell.md.

### §16.1 Mission
Two specialized in-substrate LLMs with **focused, segregated roles**:
| Role | Cell | Model (default) |
|---|---|---|
| Memory Archive Librarian | `LibrarianCell` | Gemma 2 2B Q4_K_M |
| Drift/Hallucination Warden | `WardenCell` | Qwen 2.5 Coder 1.5B Q4_K_M |

Different model families, different training corpora, different alignment regimes. Non-negotiable per P20.

### §16.2 Mutual Accountability
1. **Warden audits Librarian** — every Librarian output lands PENDING; Warden audits before Active.
2. **Librarian sanity-checks Warden** — every Warden flag sanity-checked by Librarian before real quarantine. Librarian gets **escalation power, not veto power**.
3. **AdjudicatorCell tie-breaks** — ~70-80% resolved deterministically, remainder via rotating LLM pool.

### §16.3 Asymmetric Visibility (P19, NORMATIVE)
```rust
struct CuratorOutput {
    // PUBLIC — visible to all auditors
    operation:         Operation,
    target_handle:     Handle,
    grounded_in:       Vec<Handle>,
    confidence_band:   ConfidenceBand,   // Low | Med | High (coarse only)

    // PRIVATE — capability-scoped !rationale.*
    rationale_handle:  BlobHandle,
    considered_alts:   BlobHandle,
    confidence_precise: f32,
    reasoning_trace:   BlobHandle,
}
```

### §16.4 Nine Anti-Collusion Guardrails
1. Asymmetric visibility (§16.3).
2. Different training lineages (Gemma vs Qwen-Coder).
3. Periodic adversarial probes.
4. Confidence calibration drift detection.
5. Mandatory disagreement quota (>99% → `curator.suspicious_agreement`).
6. No shared KV cache, no shared embeddings, no shared shards.
7. No reciprocal awareness of timing.
8. Substrate-only grounding requirement.
9. Periodic blind re-audit (1% sample, clean KV).

### §16.5 Governance by Trinity
Curator Cells subject to: pre-validation chain, QuarantineCell, GapCell fixation, ContractCell weight pinning, SpecAnchorCell, DecisionLedgerCell, CrossCheckLedgerCell, CongruenceCell. **No privileged path. No exemptions.**

## §17 Adjudication Layer (NEW)
Full spec in AdjudicatorCell.md.

### §17.1 Deterministic adjudication (~70-80%)
Policy table — no LLM call. Latency ≤50 μs p50, ≤200 μs p99.

### §17.2 Rotating LLM pool (~20-30%)
| Model | Params | Q4 RAM |
|---|---|---|
| Phi-3.5 Mini | 3.8B | ~2.4 GiB |
| Llama 3.2 3B | 3B | ~2.0 GiB |
| SmolLM-2 1.7B | 1.7B | ~1.3 GiB |

Rotation seeded by `envelope.seed` — deterministic. Adjudicator sees envelope + both PUBLIC outputs + substrate slice. Does NOT see CrossCheckLedger prior decisions (prevents rationale anchoring).

### §17.3 Human escalation
~1-2% via AgentRegistry escalation list.

### §17.4 Why rotation matters
A single fixed third model would itself become a drift attractor. Three-model collusion across different training lineages is structurally implausible.

## §18 Cross-Check Ledger (NEW)
Full spec in CrossCheckLedgerCell.md. Append-only, WAL-backed, KMS-signed at T2+. Derived metrics: agreement rate, suspicious-agreement signal, escalation rate, calibration drift.

## §19 Bootstrap Operator
Full spec in BootstrapOperator.md. NEW: Phase B3 provisions Curator shards after Trinity, before MCP open. Phase B5 self-test includes: Librarian skeleton round-trip, Warden flag round-trip, Adjudicator disagreement round-trip, PUBLIC/PRIVATE boundary test.

## §20 Reconciliation
UltraCortex closes **GAP-NT-013** (SummarizerCell subsumed by LibrarianCell). Addresses Mem0/LangGraph drift-detection failure mode via Curator Pair.

## §21 Cross-Document Congruence (13-way SoT)
1. Architecture.md (this)
2. CellTaxonomy.md
3. RouterScheduler.md
4. PersistenceLayer.md
5. McpProtocol.md
6. NATIVE_TRINITY.md
7. EmbeddingReranker.md
8. ObservabilityAudit.md
9. BootstrapOperator.md
10. DeepSeekOptimization.md
11. **CURATOR_PAIR_PROTOCOL.md** (NEW)
12. **LibrarianCell.md** (NEW)
13. **WardenCell.md** (NEW)

Normative companions: AdjudicatorCell.md, CrossCheckLedgerCell.md. Supporting: Roadmap.md, HANDOFF.md, RECONCILE.md, CONGRUENCE.md, README.md, SYSTEM_REQUIREMENTS.md.

## §22 GAP Register

### §22.1 Carryover (HyperCortex)
GAP-001 … GAP-014 unchanged. **GAP-NT-013 CLOSED** (subsumed by LibrarianCell).

### §22.2 Trinity / DeepSeek
Unchanged from HyperCortex v1.0.

### §22.3 Curator Pair (NEW)
| ID | Description | Phase |
|---|---|---|
| GAP-CU-001 | Librarian default model | 1G |
| GAP-CU-002 | Warden default model | 1G |
| GAP-CU-003 | Confidence-band threshold defaults | 1G |
| GAP-CU-004 | Disagreement quota bounds (92–97%) | 1G |
| GAP-CU-005 | Adversarial probe schedule + corpus | 1G |
| GAP-CU-006 | Calibration drift detection window | 1G |
| GAP-CU-007 | Adjudicator LLM pool composition | 1G |
| GAP-CU-008 | Adjudicator rotation policy | 1G |
| GAP-CU-009 | Cross-check ledger retention horizon | 1G |
| GAP-CU-010 | Per-Cell KV cache size budget | 1G |
| GAP-CU-011 | Blind re-audit sample rate (default 1%) | 1G |
| GAP-CU-012 | Negation glob canonicalization | 1G |
| GAP-CU-013 | Curator shard topology for small deployments | 1G |
| GAP-CU-014 | Human escalation routing policy | 1G |

_End of Architecture.md v1.0 (UltraCortex)._
