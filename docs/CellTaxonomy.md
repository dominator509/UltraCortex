# CellTaxonomy.md — **UltraCortex** Cell Taxonomy Specification

**Status:** v1.0 — Normative Cell-Type Specification (supersedes HyperCortex v1.0; CellTaxonomy.md v0.1 + CELL_TAXONOMY_PATCH)
**Owner:** Dominic Sarria-Wiley
**Companion documents:** Architecture.md v1.0, Roadmap.md v1.0, RouterScheduler.md v1.0, PersistenceLayer.md v1.0, McpProtocol.md v1.0, NATIVE_TRINITY.md v1.0, EmbeddingReranker.md v1.0, ObservabilityAudit.md v1.0, BootstrapOperator.md v1.0, DeepSeekOptimization.md v1.0.

---

## §0 — Document Conventions

- **MUST / SHOULD / MAY** follow RFC 2119.
- Every Cell section carries **SPEC-DERIVED-§N.N** markers.
- GAP IDs reference Architecture.md §19.
- State sketches are *illustrative Rust*. The contract is the trait surface + invariants.
- All timestamps are logical clocks (Architecture.md §4). Wall-clock reads in `on_update` are forbidden.
- All Cells implement the `Cell` trait (Architecture.md §5.1).
- Trinity Cells additionally implement the `PreValidator` sub-trait (§15).

---

## §1 — Taxonomy Overview

### §1.1 The Twenty-Five Built-In Cell Types

| #  | Cell Type | Memory Role | Persistence | Primary Index | Phase |
|----|-----------|-------------|-------------|---------------|-------|
| 1  | **CatalogCell**         | Meta              | strict     | HashMap                  | 1A |
| 2  | **FactCell**            | Semantic          | lazy       | SlabMap + BTree          | 1A |
| 3  | **TimelineCell**        | Episodic          | lazy       | Ring + Bloom             | 1A |
| 4  | **PlaybookCell**        | Procedural        | strict     | BTreeMap                 | 1D |
| 5  | **ScratchpadCell**      | Working           | ephemeral  | DashMap + expiry heap    | 1D |
| 6  | **VectorCell**          | Semantic index    | lazy       | HNSW dual-index (768/1536) | 1B |
| 7  | **GraphCell**           | Relational index  | lazy       | CSR adjacency            | 1B |
| 8  | **BM25Cell**            | Lexical index     | lazy       | Tantivy segments         | 1B |
| 9  | **BlobCell**            | Payload storage   | lazy       | Content-addressed FS     | 1A |
| 10 | **CacheCell**           | Retrieval cache   | ephemeral  | LRU + ANN                | 1B |
| 11 | **AgentRegistryCell**   | Identity          | strict     | HashMap                  | 1D |
| 12 | **ProposalCell**        | Coordination      | strict     | Append log + sig set     | 1D |
| 13 | **SubscriptionCell**    | Coordination      | lazy       | Pattern trie             | 1D |
| 14 | **RerankerCell**        | Inference service | ephemeral  | Model weights mmap       | 1B |
| 15 | **SpecAnchorCell**      | Trinity (anchor)  | strict     | (anchor_id, doc_section) BTree | **1A** |
| 16 | **DecisionLedgerCell**  | Trinity (decision)| strict     | Append log + scope BTree | **1A** |
| 17 | **CongruenceCell**      | Trinity (audit)   | lazy       | Live entity symdiff matrix | **1A** |
| 18 | **GapCell**             | Trinity (gap)     | strict     | HashMap + dispatch counter | **1A** |
| 19 | **QuarantineCell**      | Trinity (escape)  | strict     | Append log + cause index | 1B |
| 20 | **WorkBudgetCell**      | Trinity (budget)  | lazy       | Per-task envelope map    | 1B |
| 21 | **ContractCell**        | Trinity (schema)  | strict     | (contract_id, version) BTree | **1A** |

### §1.2 Cell Categories

| Category | Cells | R/W Ratio | Hot Path? |
|----------|-------|-----------|-----------|
| **Memory Stores** | Fact, Timeline, Playbook, Scratchpad | mixed | yes |
| **Indexes**       | Vector, BM25, Graph | read-heavy | yes |
| **Payload**       | Blob | write-once-read-many | warm |
| **Coordination**  | Subscription, Proposal, AgentRegistry | mixed, low-volume | yes for Sub |
| **Services**      | Reranker, Cache, Catalog | read-heavy | yes |
| **Trinity**       | SpecAnchor, DecisionLedger, Congruence, Gap, Quarantine, WorkBudget, Contract | every write touches | **yes (pre-validation chain)** |

### §1.3 Cell Lifecycle (Common)

1. **Provisioned** — registered with `CatalogCell`.
2. **Booting** — load snapshot, replay WAL.
3. **Active** — accepts queries/updates.
4. **Quiescing** — drain inbox, flush.
5. **Snapshotted** — CoW checkpoint complete.
6. **Migrated** — offline-only Phase 1; see **[GAP-001]**.
7. **Upgraded** — `migrate(old) -> new`; see **[GAP-003]**.
8. **Retired** — soft-deleted; preserved until retention horizon.

### §1.4 Common Invariants (MUST)

- **I1** — Single-threaded internally.
- **I2** — Share-nothing.
- **I3** — Orthogonal persistence.
- **I4** — Deterministic given identical inputs.
- **I5** — No wall-clock reads in `on_update`.
- **I6** — Pre-validation hook chain runs *before* `on_update` for every state-changing message (Architecture.md §15.9).
- **I7** — Every Cell registers schemas with `ContractCell` at provision time.

---

## §2 — CatalogCell

**SPEC-DERIVED-§5, §6.** Meta-registry. Every Cell instance registers with `cell_id`, `cell_type`, `shard_id`, `schema_id`, `provisioned_at`, lifecycle `state`.

```rust
pub struct CatalogState {
    cells: HashMap<CellId, CellEntry>,
    by_type: BTreeMap<CellType, BTreeSet<CellId>>,
    schema_index: BTreeMap<SchemaId, BTreeSet<CellId>>,
}
```

- **Invariants:** unique `cell_id`; every entry references a valid `SchemaId`.
- **Hot ops:** `lookup(cell_id) -> shard_id` (≤ 1 μs p50).

---

## §3 — FactCell

Semantic store of canonical facts. Append-only; supersession explicit.

```rust
pub struct FactState {
    facts: SlabMap<FactId, FactNode>,
    by_subject: BTreeMap<SubjectId, BTreeSet<FactId>>,
    superseded: HashMap<FactId, FactId>,
}
```

`FactNode { subject, predicate, object_handle, confidence, provenance_handle, logical_at }`. Facet-path capability scoping per Architecture.md §12.

---

## §4 — TimelineCell

Episodic events on a per-actor stream. Ring buffer up to retention; overflow → BlobCell.

```rust
pub struct TimelineState {
    streams: HashMap<StreamId, Ring<EventNode>>,
    bloom_per_stream: HashMap<StreamId, BloomFilter>,
}
```

`recall_recent(stream_id, k)` is O(1).

---

## §5 — PlaybookCell

Procedural memory keyed by trigger conditions. A `Play` is a serialized DAG of MCP verbs.

```rust
pub struct PlaybookState { plays: BTreeMap<TriggerKey, Play> }
```

---

## §6 — ScratchpadCell

Ephemeral working memory with TTL. Never persisted.

```rust
pub struct ScratchpadState { pad: DashMap<Key, Scrap>, expiry: BinaryHeap<Expiry> }
```

---

## §7 — VectorCell

Dual-index HNSW (768-d + 1536-d), DeepSeek-aligned.

```rust
pub struct VectorState {
    index_768:  Hnsw768,
    index_1536: Hnsw1536,
    id_map:     SlabMap<NodeId, EmbeddingMeta>,
}
```

Chunking: 512/1024 token windows, 64-token overlap (DeepSeekOptimization.md §7). Per-shard share-nothing.

---

## §8 — GraphCell

CSR adjacency over typed edges.

```rust
pub struct GraphState { adj: CsrGraph<NodeId, EdgeType>, by_type: BTreeMap<EdgeType, EdgeBitset> }
```

---

## §9 — BM25Cell

Tantivy-backed lexical index over text payloads.

---

## §10 — BlobCell

Payload bodies. SHA-256 content-addressed. Refcounted. Mark-and-sweep GC.

---

## §11 — CacheCell

LRU + small ANN over recent retrieval. Token-budget-aware: returns cached prefix-stable Views when fresh assembly would exceed budget.

---

## §12 — AgentRegistryCell

Identity + capability-token issuance/revocation.

---

## §13 — ProposalCell

Multi-agent proposal/quorum log. **[GAP-011]** quorum defaults.

---

## §14 — SubscriptionCell

Pattern-trie of subscription filters. Server-stream fan-out. **Hot path.**

---

## §15 — RerankerCell

Mmapped cross-encoder weights; deterministic per-shard inference. Hot-swap via `migrate()`.

---

# Native Trinity Cells (§16–§22)

Foundational. Five ship in Phase 1A. All seven implement `PreValidator`:

```rust
pub trait PreValidator: Cell {
    fn pre_validate(&self, env: &MessageEnv) -> Result<(), ValidationError>;
}
```

---

## §16 — SpecAnchorCell

**SPEC-DERIVED-§15.2.** **GAPs:** GAP-NT-002, GAP-NT-009.

```rust
pub struct SpecAnchorState {
    anchors:     BTreeMap<AnchorId, AnchorNode>,
    by_section:  BTreeMap<DocSection, BTreeSet<AnchorId>>,
    by_artifact: BTreeMap<ArtifactPath, BTreeSet<AnchorId>>,
}

pub struct AnchorNode {
    anchor_id:     AnchorId,
    doc_section:   DocSection,     // "Architecture.md§15.2"
    artifact_path: ArtifactPath,   // "crates/ultracortex-trinity/src/spec_anchor.rs:42"
    artifact_kind: ArtifactKind,   // Code | Doc | Test | Schema
    created_at:    u64,
    status:        AnchorStatus,   // Active | Stale | Orphaned
}
```

**Invariants:**
- **A1** — every `SPEC-DERIVED-§N.N` marker has a corresponding `AnchorNode`.
- **A2** — every SoT-ten doc section has ≥1 anchor (or explicit `no_implementation_required` facet).
- **A3** — orphaned anchors emit `anchor.orphaned` and block HALT.

**`pre_validate`:** rejects writes whose target lacks an anchor (unless the write creates it).

---

## §17 — DecisionLedgerCell

**SPEC-DERIVED-§15.3.** **GAPs:** GAP-NT-003.

```rust
pub struct DecisionLedgerState {
    decisions:  SlabMap<DecisionId, DecisionRecord>,
    by_scope:   BTreeMap<Scope, BTreeSet<DecisionId>>,
    superseded: HashMap<DecisionId, DecisionId>,
}

pub struct DecisionRecord {
    decision_id:      DecisionId,
    agent_id:         AgentId,
    scope:            Scope,
    rationale_handle: BlobHandle,
    superseded_by:    Option<DecisionId>,
    wal_offset:       u64,
    logical_at:       u64,
    severity:         Severity,
}
```

**Invariants:**
- **D1** — append-only; no in-place mutation.
- **D2** — `supersede(old, new)` is the only invalidation path.
- **D3** — `by_scope` lookup O(log n); conflict detection O(1) amortized.

**`pre_validate`:** rejects scope-conflicting writes unless `supersedes:` is provided.

---

## §18 — CongruenceCell

**SPEC-DERIVED-§15.4.** **GAPs:** GAP-NT-004.

```rust
pub struct CongruenceState {
    docs:             BTreeMap<DocId, DocEntityIndex>,
    matrix:           HashMap<(DocId, DocId), EntitySymDiff>,
    accepted_deltas:  HashMap<(DocId, DocId), AcceptedDeltaSet>,
}
```

**Invariants:**
- **C1** — `matrix` recomputed incrementally on each `node.written` / `node.superseded` for spec-typed nodes (O(delta)).
- **C2** — non-empty `EntitySymDiff` minus corresponding `accepted_deltas` blocks the next HALT.

**`pre_validate`:** previews post-write symdiff; rejects unaccepted deltas.

---

## §19 — GapCell

**SPEC-DERIVED-§15.5.** **GAPs:** GAP-NT-005.

```rust
pub struct GapState {
    gaps:               HashMap<GapId, GapRecord>,
    by_status:          BTreeMap<GapStatus, BTreeSet<GapId>>,
    dispatch_counter:   HashMap<GapId, u32>,
    last_transition:    HashMap<GapId, u64>,
}

pub enum GapStatus { Open, InProgress, Blocked, Resolved, Quarantined }
```

**Invariants:**
- **G1** — every GAP-NNN/GAP-NT-NNN/GAP-DS-NNN in the corpus has a `GapRecord`.
- **G2** — `dispatch_counter` increments on every envelope whose `gap_ref` references it.
- **G3** — when counter advances by N (default 8) without a `last_transition` change → emit `task.no_progress`.

**`pre_validate`:** rejects writes that re-introduce a `Resolved` gap.

---

## §20 — QuarantineCell

**SPEC-DERIVED-§15.6.** **GAPs:** GAP-NT-006.

```rust
pub struct QuarantineState {
    quarantined:                SlabMap<QuarantineId, QuarantineRecord>,
    by_cause:                   BTreeMap<CauseKind, BTreeSet<QuarantineId>>,
    retention_horizon_logical:  u64,
}

pub struct QuarantineRecord {
    envelope:        MessageEnv,
    cause:           CauseKind,  // ContractFail | AnchorMissing | BudgetExceeded | ...
    quarantined_at:  u64,
    reviewer:        Option<AgentId>,
    disposition:     Disposition, // Pending | Reinjected | Rejected | Expired
}
```

**Invariants:**
- **Q1** — no message failing pre-validation is silently dropped; MUST land here.
- **Q2** — `Reinjected` re-dispatches the original envelope with an audit link.

---

## §21 — WorkBudgetCell

**SPEC-DERIVED-§15.7.** **GAPs:** GAP-NT-007, GAP-NT-010.

```rust
pub struct WorkBudgetState {
    envelopes: HashMap<TaskId, BudgetEnvelope>,
}

pub struct BudgetEnvelope {
    task_id:           TaskId,
    tokens_allowed:    u32,
    tokens_spent:      u32,
    deadline_logical:  u64,
    retries_allowed:   u8,
    retries_used:      u8,
    severity:          Severity,
    parent_task:       Option<TaskId>,
}
```

**Invariants:**
- **W1** — envelope provisioned at task creation; lookups O(1).
- **W2** — `charge_pre(task_id, est)` rejects on over-budget; `charge_post(task_id, actual)` reconciles.
- **W3** — exhaustion emits `task.budget.exceeded`, snapshots state, escalates per Severity. **Never loops.**

**`pre_validate`:** rejects writes exceeding `tokens_remaining`.

---

## §22 — ContractCell

**SPEC-DERIVED-§15.8.** **GAPs:** GAP-NT-008.

```rust
pub struct ContractState {
    contracts:  BTreeMap<ContractId, BTreeMap<Version, ContractSpec>>,
    active:     HashMap<ContractId, Version>,
    deprecated: HashMap<ContractId, Vec<Version>>,
}
```

**Invariants:**
- **K1** — breaking changes require a `migration_plan_handle` referencing a Decision record.
- **K2** — writes against deprecated contracts are quarantined.

**`pre_validate`:** rejects schema-noncompliant messages (hook #1 in the chain).

---

## §23 — Cross-Cell Interaction Matrix

R = read via message; W = write via message; — = no direct interaction.

| From \ To | Cat | Fact | Time | Play | Scr | Vec | Grph | BM25 | Blob | Cch | Agt | Prop | Sub | Rer | SpA | DcL | Cng | Gap | Qur | Bud | Ctr |
|-----------|-----|------|------|------|-----|-----|------|------|------|-----|-----|------|-----|-----|-----|-----|-----|-----|-----|-----|-----|
| Catalog   | —   | R    | R    | R    | R   | R   | R    | R    | —    | —   | R   | R    | R   | R   | R   | R   | R   | R   | R   | R   | R   |
| Fact      | R   | —    | W    | R    | —   | W   | W    | W    | W    | R   | R   | —    | W   | —   | W   | W   | W   | R   | —   | R   | R   |
| Timeline  | R   | —    | —    | —    | —   | —   | —    | —    | W    | R   | R   | —    | W   | —   | R   | R   | R   | R   | —   | R   | R   |
| Playbook  | R   | R    | R    | —    | —   | —   | —    | —    | —    | R   | R   | W    | W   | —   | R   | W   | R   | R   | —   | R   | R   |
| Scratch   | —   | —    | —    | —    | —   | —   | —    | —    | —    | —   | —   | —    | W   | —   | —   | —   | —   | —   | —   | R   | R   |
| Vector    | R   | R    | —    | —    | —   | —   | —    | R    | R    | R   | —   | —    | W   | R   | R   | R   | R   | R   | —   | R   | R   |
| Graph     | R   | R    | R    | —    | —   | —   | —    | —    | —    | R   | —   | —    | W   | —   | R   | R   | R   | R   | —   | R   | R   |
| BM25      | R   | R    | —    | —    | —   | —   | —    | —    | R    | R   | —   | —    | W   | R   | R   | R   | R   | R   | —   | R   | R   |
| Blob      | R   | —    | —    | —    | —   | —   | —    | —    | —    | —   | —   | —    | W   | —   | R   | R   | R   | R   | R   | R   | R   |
| Cache     | R   | R    | R    | R    | —   | R   | R    | R    | R    | —   | —   | —    | W   | R   | R   | R   | R   | R   | —   | R   | R   |
| AgentReg  | R   | —    | —    | —    | —   | —   | —    | —    | —    | —   | —   | R    | W   | —   | R   | W   | R   | R   | R   | R   | R   |
| Proposal  | R   | —    | W    | R    | —   | —   | —    | —    | W    | R   | R   | —    | W   | —   | R   | W   | R   | R   | R   | R   | R   |
| Subscrip  | R   | R    | R    | R    | R   | R   | R    | R    | R    | R   | R   | R    | —   | —   | R   | R   | R   | R   | R   | R   | R   |
| Reranker  | R   | —    | —    | —    | —   | R   | —    | R    | R    | R   | —   | —    | W   | —   | R   | R   | R   | R   | —   | R   | R   |
| **SpecA** | R   | W    | R    | R    | —   | —   | —    | —    | R    | —   | R   | R    | W   | —   | —   | R   | W   | W   | W   | R   | R   |
| **DecL**  | R   | W    | W    | R    | —   | —   | —    | —    | W    | —   | R   | W    | W   | —   | R   | —   | W   | W   | W   | W   | R   |
| **Cong**  | R   | R    | R    | R    | —   | —   | —    | —    | R    | —   | R   | R    | W   | —   | R   | R   | —   | W   | W   | R   | R   |
| **Gap**   | R   | R    | R    | R    | —   | —   | —    | —    | R    | —   | R   | R    | W   | —   | R   | R   | R   | —   | W   | W   | R   |
| **Quar**  | R   | R    | R    | R    | R   | R   | R    | R    | R    | R   | R   | R    | W   | —   | R   | R   | R   | R   | —   | R   | R   |
| **Budg**  | R   | —    | —    | —    | —   | —   | —    | —    | —    | —   | R   | R    | W   | —   | R   | R   | R   | R   | R   | —   | R   |
| **Ctr**   | R   | —    | —    | —    | —   | —   | —    | —    | —    | —   | R   | R    | W   | —   | R   | R   | R   | R   | R   | R   | —   |

---

## §24 — Open GAPs (Cell-level)

| ID         | Owning Cell      | Description                                  |
|------------|------------------|----------------------------------------------|
| GAP-002    | all              | Final allocator selection                    |
| GAP-004    | Vec/BM25/Graph   | Hybrid retrieval ranking weights             |
| GAP-NT-002 | SpecAnchor       | Anchor edge granularity                      |
| GAP-NT-003 | DecisionLedger   | Decision conflict policy for scope=*         |
| GAP-NT-004 | Congruence       | Delta acceptance UI                          |
| GAP-NT-005 | Gap              | Dispatch counter window N                    |
| GAP-NT-006 | Quarantine       | Retention horizon                            |
| GAP-NT-007 | WorkBudget       | Per-namespace defaults                       |
| GAP-NT-008 | Contract         | Schema migration tooling                     |
| GAP-NT-009 | all Trinity      | Pre-validation chain ordering proof          |
| GAP-NT-013 | (proposed)       | SummarizerCell                               |
| GAP-NT-014 | Cache + L0       | PrefixCacheStore eviction policy             |
| GAP-DS-004 | Contract         | View-schema versioning for prefix stability  |

---

## §25 — Congruence Contract

Congruent with: Architecture.md (Cell trait §5, hook chain §15.9, GAP register §19), RouterScheduler.md (severity, gap-aware, budget), PersistenceLayer.md (per-cell snapshot, schema-id persistence), McpProtocol.md (envelope shape, work-budget), NATIVE_TRINITY.md, DeepSeekOptimization.md (prefix-stable view emission). Live enforcement by `CongruenceCell`.

_End of CellTaxonomy.md v1.0 (UltraCortex)._


---

# 🆙 UltraCortex v1.0 Delta — 4 New Curator Cells (§§22–25)

This document's HyperCortex content above remains normative for Cells 1–21. UltraCortex v1.0 adds four Curator Cells. They implement `Cell` + `PreValidator` and split outputs PUBLIC/PRIVATE per **P19** (Asymmetric Visibility).

## §22 LibrarianCell

**Category:** Curator · **Persistence:** mmap weights + RAM-only KV · **Phase:** 1G · **GAPs:** GAP-CU-001, GAP-CU-003, GAP-CU-005, GAP-CU-006, GAP-CU-010, GAP-CU-013

Memory Archive Librarian. Default model: **Gemma 2 2B Q4_K_M**, pinned by SHA-256 in `ContractCell`. Async-only on `node.written`. Operations: `Skeleton | SupersedeProposal | ArchiveTag`. PUBLIC fields: operation, target_handle, grounded_in, confidence_band, schema_id, spec_anchor, logical_at. PRIVATE fields: rationale_handle, considered_alts, confidence_precise, reasoning_trace, private_seed. **Closes GAP-NT-013.**

Full spec: `LibrarianCell.md`.

## §23 WardenCell

**Category:** Curator · **Persistence:** mmap weights + RAM-only KV · **Phase:** 1G · **GAPs:** GAP-CU-002, GAP-CU-004, GAP-CU-007, GAP-CU-008, GAP-CU-011, GAP-CU-012

Drift/Hallucination Warden. Default model: **Qwen 2.5 Coder 1.5B Q4_K_M** — **MUST be a different model family than Librarian**. Pinned by SHA-256. Opt-in sync via `flags.semantic_check`; auto-sync on `severity=P0`; async audit of every Librarian output. Output variants: `Pass | FlagDrift | FlagHallucination`. PUBLIC + PRIVATE split per P19.

Full spec: `WardenCell.md`.

## §24 AdjudicatorCell

**Category:** Curator · **Persistence:** mmap pool + policy table · **Phase:** 1G · **GAPs:** GAP-CU-007, GAP-CU-008, GAP-CU-014

Tie-breaker. Two-stage policy:
1. **Deterministic adjudication** (~70–80%): Rust policy table, no LLM call, ≤200 μs p99.
2. **Rotating LLM pool** (~20–30%): Phi-3.5 Mini / Llama 3.2 3B / SmolLM-2, rotation seeded by `envelope.seed`.
3. **Human escalation** (~1–2%) via AgentRegistry list.

MUST NOT see CrossCheckLedger prior decisions (prevents rationale anchoring).

Full spec: `AdjudicatorCell.md`.

## §25 CrossCheckLedgerCell

**Category:** Curator · **Persistence:** strict WAL + KMS-signed at T2+ · **Phase:** 1G · **GAPs:** GAP-CU-009, GAP-NT-012

Append-only forensic ledger of every cross-check + adjudication. Schema: `CrossCheckRecord { record_id, initiator, auditor, audit_kind, initiator_output (PUBLIC only), auditor_judgment, auditor_grounding, independent_handle, outcome, adjudicator_id, resolution_handle, logical_at }`. Derived metrics: agreement rate, suspicious-agreement signal (>99%), escalation rate, calibration drift.

Full spec: `CrossCheckLedgerCell.md`.

## §26 New Common Invariant

- **I8 (NEW v1.0):** Curator Cells MUST split outputs into PUBLIC and PRIVATE fields per **P19**.

## §27 GAP Updates

- **GAP-NT-013 (SummarizerCell): CLOSED** — subsumed by LibrarianCell.
- New namespace **GAP-CU-001..014** (full table in HANDOFF.md).

## §28 Congruence Contract (Updated)

Congruent with: Architecture-UltraCortex.md (§§16–18, §22.3), **CURATOR_PAIR_PROTOCOL.md**, **LibrarianCell.md**, **WardenCell.md**, **AdjudicatorCell.md**, **CrossCheckLedgerCell.md**, RouterScheduler.md (`flags.semantic_check`, Adjudicator path), NATIVE_TRINITY.md (Curator governance), ObservabilityAudit.md (curator.* metrics + events).

_End of UltraCortex v1.0 Delta._
