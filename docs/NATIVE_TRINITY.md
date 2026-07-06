# NATIVE_TRINITY.md — **UltraCortex** Anti-Drift, Anti-Freeze, Anti-Fixation Subsystem

**Status:** v1.0 — Normative L1+L2 Subsystem Specification (supersedes HyperCortex v1.0; NATIVE_TRINITY.md v0.1)
**Owner:** Dominic Sarria-Wiley
**Companion documents:** Architecture.md v1.0, CellTaxonomy.md v1.0, RouterScheduler.md v1.0, PersistenceLayer.md v1.0, McpProtocol.md v1.0, EmbeddingReranker.md v1.0, ObservabilityAudit.md v1.0, BootstrapOperator.md v1.0.

---

## §0 — Document Conventions

- **MUST / SHOULD / MAY** follow RFC 2119.
- **SPEC-DERIVED-§N.N** markers reference Architecture.md.
- Open items reference **[GAP-NT-NNN]**, distinct from the original GAP-NNN namespace.
- All timestamps inside Cells are logical clocks.
- All seven Trinity Cells implement the Cell trait (Architecture.md §5.1) **and** the `PreValidator` sub-trait (CellTaxonomy.md §15).

---

## §1 — Mission & Scope

The Native Trinity makes three classes of failure structurally impossible by absorbing them into the substrate as first-class Cells + scheduler policies:

1. **Drift** — code/decisions/contracts diverging from spec.
2. **Freeze** — strict gates blocking forward progress without escape valves.
3. **Fixation** — agents looping endlessly on unsolvable gaps.

> "don't drift, don't freeze, don't fixate." — the three-document trinity, native realization.

### §1.1 In Scope

- Native Cell implementations replacing all external CI scripts (anchor audit, congruence audit, decision audit).
- Router pre-validation hook chain.
- Scheduler severity-aware routing.
- Scheduler gap-aware loop detection.
- WAL-backed decision ledger.
- Live congruence matrix.
- Quarantine and budget enforcement.

### §1.2 Non-Goals

- Model-internal hallucination prevention.
- Cross-host federation of trinity state (deferred to Phase 2+, [GAP-014]).
- Replacement of human HALT approval (Trinity informs HALT; does not replace it).

### §1.3 Performance Thesis

The Trinity is a **performance multiplier**, not a tax: it eliminates wasted work (drifted code, frozen pipelines, fixated loops) **before tokens are spent**. Quantitative analysis in §13.

---

## §2 — The Three Failure Modes

### §2.1 Drift

Drift occurs when code, decisions, contracts, semantic conventions, or handoff state diverge from the spec source-of-truth. Trinity addresses all subtypes at **write time**.

### §2.2 Freeze

Freeze occurs when strict drift gates prevent forward progress (agents retrying failed writes indefinitely; HALT items accumulating). Trinity prevents freeze with escape valves: quarantine, severity tiering, budget enforcement.

### §2.3 Fixation

Fixation occurs when agents loop on an unsolvable gap. Trinity prevents fixation with the gap-aware loop detector (§9.3) — repeated dispatches against the same `Gap` ID without state transition escalate, never silently retry.

---

## §3 — Subsystem Overview

```
                   ┌─────────────────────────────────────┐
                   │            TRINITY SHARD            │
                   │                                     │
   write envelope →│  ContractCell  →  SpecAnchorCell  →│→ proceed to target shard
                   │       ↓               ↓             │
                   │  DecisionLedgerCell ← WorkBudgetCell│
                   │       ↓               ↓             │
                   │  CongruenceCell    QuarantineCell  │
                   │       ↓               ↑             │
                   │     GapCell ──────────┘             │
                   └─────────────────────────────────────┘
```

Every state-changing envelope flows top-to-bottom. Any failure routes to `QuarantineCell`. The chain is in-process on a single shard (Architecture.md §15.9 latency target: ≤ 25 μs p50, ≤ 100 μs p99).

---

## §4 — SpecAnchorCell

**Purpose:** bidirectional spec ↔ code/doc anchoring.

```rust
pub struct SpecAnchorState {
    anchors:     BTreeMap<AnchorId, AnchorNode>,
    by_section:  BTreeMap<DocSection, BTreeSet<AnchorId>>,
    by_artifact: BTreeMap<ArtifactPath, BTreeSet<AnchorId>>,
}

pub struct AnchorNode {
    anchor_id:     AnchorId,
    doc_section:   DocSection,
    artifact_path: ArtifactPath,
    artifact_kind: ArtifactKind, // Code | Doc | Test | Schema
    created_at:    u64,
    status:        AnchorStatus, // Active | Stale | Orphaned
}
```

**Invariants A1–A3:** see CellTaxonomy.md §16.

**`pre_validate` behavior:** rejects writes whose target has no anchor, unless the write itself creates the anchor.

**`[GAP-NT-002]`** anchor edge granularity (line vs section).
**`[GAP-NT-009]`** pre-validation chain ordering proof.

---

## §5 — DecisionLedgerCell

**Purpose:** append-only WAL-backed ledger of normative choices.

See CellTaxonomy.md §17 for state structure and invariants D1–D3.

**`pre_validate` behavior:** rejects scope-conflicting writes unless `supersedes:` is provided.

**Event emission:** `decision.applied`, `decision.conflict`, `decision.superseded` — all audited.

**`[GAP-NT-003]`** policy for `scope=*` (cross-cutting) decisions.

---

## §6 — CongruenceCell

**Purpose:** live congruence matrix across the five-doc SoT (Architecture, CellTaxonomy, RouterScheduler, PersistenceLayer, McpProtocol) + supporting docs.

See CellTaxonomy.md §18 for invariants C1–C2.

**`pre_validate` behavior:** previews the post-write symdiff; rejects unaccepted deltas.

**Event emission:** `congruence.delta`, `congruence.delta_accepted`.

**`[GAP-NT-004]`** delta acceptance UI.

---

## §7 — GapCell

**Purpose:** lifecycle tracker for GAP-NNN / GAP-NT-NNN / GAP-DS-NNN + fixation detection.

State: see CellTaxonomy.md §19.

**Fixation detector (loop detection):**

```
on dispatch_increment(gap_id):
    dispatch_counter[gap_id] += 1
    if dispatch_counter[gap_id] - last_transition_seq[gap_id] >= N:  // N default 8
        emit("task.no_progress", gap_id)
        return RequireEscalation
```

**Event emission:** `gap.transition`, `task.no_progress`.

**`[GAP-NT-005]`** counter window N.

---

## §8 — QuarantineCell

**Purpose:** holding pen for failed/blocked envelopes with full provenance.

See CellTaxonomy.md §20 for invariants Q1–Q2.

**Event emission:** `task.quarantined`.

**Admin path:** `ultracortex quarantine list` / `reinject` / `reject` (BootstrapOperator.md §5).

**`[GAP-NT-006]`** retention horizon.

---

## §9 — WorkBudgetCell

**Purpose:** per-task budget enforcement.

State: see CellTaxonomy.md §21.

### §9.1 Charging Model

- `charge_pre(task_id, est_tokens)` runs in the pre-validation chain.
- `charge_post(task_id, actual_tokens)` runs after the response is assembled.
- Discrepancies are logged; consistent under-estimation triggers an `estimator.recalibrate` event.

### §9.2 Budget Exhaustion

On exhaustion:

1. Emit `task.budget.exceeded`.
2. Snapshot current task state.
3. Escalate per severity table (P0 → immediate human; P1/P2 → backlog).
4. **NEVER loop**, **NEVER silently retry**.

### §9.3 Fixation Coupling

The WorkBudgetCell coordinates with GapCell: when budget approaches exhaustion AND `gap_dispatch_counter` is non-zero, the scheduler preemptively triggers `task.no_progress` rather than waiting for the explicit fixation threshold.

**`[GAP-NT-007]`** namespace defaults. **`[GAP-NT-010]`** acceptance bench.

---

## §10 — ContractCell

**Purpose:** registry of every interface/schema (Cell traits, View schemas, Envelope shape).

State: see CellTaxonomy.md §22.

### §10.1 Migration Plan

Breaking changes require:

1. a new `Version` registered alongside the old,
2. a `migration_plan_handle` (Blob) describing transitional steps,
3. a Decision record linking the contract version change to the plan,
4. a deprecation deadline (logical),
5. quarantined writes against the old version after the deadline.

**`pre_validate` behavior:** rejects schema-noncompliant messages (this is hook #1 in the chain).

**`[GAP-NT-008]`** migration tooling.

---

## §11 — Router / Scheduler Enhancements

Three policies extend the generic RouterScheduler:

### §11.1 Pre-Validation Hook Chain Ordering

In order (and rationale):

1. `ContractCell.validate_schema()` — cheapest first; without schema validity, nothing downstream is meaningful.
2. `SpecAnchorCell.validate_anchor()` — anchor existence is a structural prerequisite for valid writes.
3. `DecisionLedgerCell.check_conflicts()` — semantic conflict check; cheap BTree lookup.
4. `WorkBudgetCell.charge_pre()` — early budget gate; prevents waste from later steps.
5. `CongruenceCell.preview_delta()` — most expensive (incremental symdiff); runs last to avoid wasting compute on writes the earlier hooks reject.

Failures route to `QuarantineCell.absorb()`. Each step short-circuits the chain.

**Proof-of-correctness sketch (informal):** Each hook is monotone with respect to the substrate invariants it guards; ordering by ascending cost minimizes wasted work; routing failures to Quarantine guarantees no envelope is silently lost. Formal proof: **[GAP-NT-009]**.

### §11.2 Severity-Aware Routing

P0 / P1 / P2 routing per RouterScheduler.md §6. Severity propagates to spawned cross-Cell calls. **[GAP-NT-011]**.

### §11.3 Gap-Aware Loop Detection

Per §7 above and RouterScheduler.md §7.

---

## §12 — Integration With Existing Subsystems

| Subsystem | Trinity integration |
|-----------|---------------------|
| Router    | pre-validation hook chain (§11.1) |
| Scheduler | severity + gap-aware routing (§11.2, §11.3) |
| Persistence | Trinity Cell WAL persistence (PersistenceLayer.md §10) |
| MCP Protocol | WorkBudget envelope, Trinity error codes (McpProtocol.md §8) |
| Observability | Trinity events audited (ObservabilityAudit.md §5.2) |
| Bootstrap | Trinity shard provisioned first (BootstrapOperator.md §6) |
| Embedding/Reranker | offloaded reasoning preserves Trinity guarantees (no silent LLM rationales) |

---

## §13 — Performance Analysis

### §13.1 Direct Cost

Pre-validation chain latency: ≤ 25 μs p50, ≤ 100 μs p99 (Architecture.md §15.9).

### §13.2 Indirect Savings (the multiplier)

For a steady-state DeepSeek multi-step coding agent benchmark (target):

| Scenario | Without Trinity | With Trinity |
|----------|-----------------|--------------|
| Drifted code paths retried | ~12% of writes | < 0.1% (rejected at write time) |
| Fixated loops (envelope retries on stuck gap) | unbounded, terminated by timeout | terminated by gap detector in ≤ 8 dispatches |
| Budget overruns (silent prompt inflation) | typical 1.5–3× | hard-bounded by WorkBudgetCell |
| Token waste from quarantine-eligible work | full token cost, dropped silently | rejected pre-write, ~0 tokens spent |

Net effect: **tokens-injected-per-step reduction ≥ 40%** vs an equivalent Cortex-v0.1 path on the same workload. **[GAP-NT-010]** acceptance bench.

### §13.3 Failure Mode if Trinity Disabled

There is no supported configuration that disables Trinity. Removing it would invalidate the substrate guarantees (P16/P17/P18).

---

## §14 — Cross-Cell Matrix (Trinity Excerpt)

See CellTaxonomy.md §23 for the full 21×21 matrix. Trinity → non-Trinity write paths are confined to:

- `SpecAnchorCell` writes anchors derived from FactCell-stored doc nodes.
- `DecisionLedgerCell` writes to BlobCell (rationale storage).
- `CongruenceCell` reads spec nodes from FactCell, writes Subscription deliveries.
- `GapCell` writes to QuarantineCell and WorkBudgetCell on detector trips.
- `QuarantineCell` writes Subscription deliveries on absorption.

---

## §15 — Conformance Test Bundle

Every release MUST pass:

1. **Anchor coverage:** every `SPEC-DERIVED-§` in the corpus maps to a live AnchorNode.
2. **Decision conflict detection:** synthetic conflicting writes → second write returns `DecisionConflict`.
3. **Congruence delta detection:** synthetic spec mutation introducing an entity divergence → blocks the next HALT.
4. **Fixation detection:** synthetic loop of N+1 envelopes against one GapId → `task.no_progress` fires on the N+1th.
5. **Budget enforcement:** envelope with `tokens_remaining = 0` → `BudgetExceeded` without dispatch.
6. **Quarantine no-drop:** induce every failure mode → every failed envelope appears in QuarantineCell.
7. **Audit chain integrity:** every Trinity event lands in the hash-chained audit (ObservabilityAudit.md §5).
8. **Trinity boot ordering:** verify Trinity shard is live before the MCP surface opens.

---

## §16 — GAPs (Trinity)

| ID | Description |
|----|-------------|
| GAP-NT-001 | Trinity-shard topology (dedicated vs co-tenant) |
| GAP-NT-002 | Anchor edge granularity (line vs section) |
| GAP-NT-003 | Decision conflict policy for scope=* |
| GAP-NT-004 | Congruence delta acceptance UI |
| GAP-NT-005 | Gap dispatch counter window N |
| GAP-NT-006 | Quarantine retention horizon |
| GAP-NT-007 | WorkBudget defaults per namespace |
| GAP-NT-008 | ContractCell migration tooling |
| GAP-NT-009 | Pre-validation chain ordering proof |
| GAP-NT-010 | Token-injected-per-step acceptance bench |
| GAP-NT-011 | Severity-tag propagation across cross-cell calls |
| GAP-NT-012 | Audit signing key custody |
| GAP-NT-013 | SummarizerCell |
| GAP-NT-014 | PrefixCacheStore eviction policy |

---

## §17 — Congruence Contract

This document, Architecture.md §15, CellTaxonomy.md §§16–22, RouterScheduler.md §§5–11, PersistenceLayer.md §10, McpProtocol.md §8, ObservabilityAudit.md §5, and BootstrapOperator.md §6 form a **nine-way source-of-truth** for the Trinity. Any change to one MUST be reflected in the others within the same change set. Enforcement is live by `CongruenceCell`, not by external scripts.

_End of NATIVE_TRINITY.md v1.0 (UltraCortex)._


---

# 🆙 UltraCortex v1.0 Delta — Trinity Governs the Curator Pair

The HyperCortex Trinity content above remains normative. UltraCortex v1.0 extends the Trinity's anti-failure substrate to govern the in-substrate LLMs themselves, per **P20 (Substrate-Policed Semantic Layer)**.

## §A.1 Trinity Extension: Curator Pair Governance

The Curator Pair (`LibrarianCell`, `WardenCell`, `AdjudicatorCell`, `CrossCheckLedgerCell`) are **not exempt** from any Trinity mechanism. Specifically:

- **SpecAnchorCell** — Curator PUBLIC outputs MUST be anchored to a spec section (their own Cell spec or CURATOR_PAIR_PROTOCOL.md).
- **DecisionLedgerCell** — every Curator-pinning model swap is a Decision; every adjudication resolution is a Decision.
- **CongruenceCell** — Curator-pair docs are part of the 13-way SoT; deltas block HALT.
- **GapCell** — Curator Cells have their own dispatch counters; fixation detection applies (e.g., Librarian-fixated-on-supersede, Warden-fixated-on-flag).
- **QuarantineCell** — Curator failures route here with new causes: `SemanticDrift`, `HallucinationDetected`, `AdjudicationFailed`, `GroundingViolation`.
- **WorkBudgetCell** — Curator inferences charge against per-task budgets.
- **ContractCell** — Curator weight files pinned by SHA-256; system prompts pinned; schemas pinned.

**No privileged path. No exemption from quarantine. No exemption from fixation detection.**

## §A.2 Pre-Validation Hook Chain (Updated, Step 6 Optional)

```
1. ContractCell.validate_schema      [Trinity, ≤25 μs p99]
2. SpecAnchorCell.validate_anchor    [Trinity, ≤25 μs p99]
3. DecisionLedgerCell.check_conflicts [Trinity, ≤25 μs p99]
4. WorkBudgetCell.charge_pre          [Trinity, ≤25 μs p99]
5. CongruenceCell.preview_delta       [Trinity, ≤25 μs p99]
6. WardenCell.judge   [Curator, OPTIONAL, ~140 ms p50] — only if flags.semantic_check OR severity=P0
```

Steps 1–5 stay on the ≤100 μs p99 hot path. Step 6 is opt-in and decoupled.

## §A.3 New QuarantineCell Causes

```rust
enum QuarantineCause {
    // ... existing causes ...
    SemanticDrift,            // Warden detected drift from canonical substrate
    HallucinationDetected,    // Warden detected hallucinated handles/facts
    AdjudicationFailed,       // Adjudicator could not resolve disagreement
    GroundingViolation,       // Auditor cited a handle not in substrate at audit time
}
```

## §A.4 GapCell Fixation Detection on Curators

GapCell now tracks dispatch counters for Curator-specific patterns:

- `librarian.supersede_loop` — Librarian repeatedly proposing supersedes that get quarantined.
- `warden.flag_loop` — Warden repeatedly flagging the same envelope pattern.
- `adjudicator.escalation_loop` — Adjudicator repeatedly escalating to human.

Window N (default 8) applies — same as for any other Cell.

## §A.5 GAP-NT-013 CLOSURE

**GAP-NT-013 (SummarizerCell) is CLOSED in UltraCortex v1.0** — the proposed SummarizerCell is subsumed by LibrarianCell's `Skeleton` operation mode. The summarizer is no longer a separate Cell; it is a Curator function with full mutual-accountability oversight.

## §A.6 Performance Thesis (Updated)

The Trinity remains a performance multiplier, not a tax. UltraCortex extends this: **the Curator Pair adds semantic policing without entering the hot path** (Librarian async on node.written; Warden opt-in via flags.semantic_check). Net effect on tokens-injected-per-step: further reduction below the HyperCortex ≤1.5 KB p50 baseline, plausibly to **≤800 B p50**, because Librarian-generated skeletons are semantically richer than regex-extracted ones.

## §A.7 New Conformance Test (NORMATIVE)

> **Trinity-governs-Curator test:** Induce a Librarian fixation loop on a synthetic gap → `GapCell` fixation detector MUST fire on the (N+1)th dispatch AND the Librarian MUST be quarantined like any other Cell. No special-casing.

## §A.8 GAPs Updated

- **GAP-NT-013 CLOSED** (subsumed by LibrarianCell).
- New namespace **GAP-CU-001..014** (full list in HANDOFF.md).

## §A.9 Congruence Contract (Updated)

Congruent with: Architecture-UltraCortex.md (§15, §16, §17), **CURATOR_PAIR_PROTOCOL.md** (§5 — Trinity is the substrate that makes the Curator Pair safe), **LibrarianCell.md**, **WardenCell.md**, **AdjudicatorCell.md**, **CrossCheckLedgerCell.md**, CellTaxonomy.md (Cells 22–25 added).

_End of UltraCortex v1.0 Delta._
