# RouterScheduler.md — **UltraCortex** L2 Router & Scheduler Specification

**Status:** v1.0 — Normative L2 Specification
**Owner:** Dominic Sarria-Wiley
**Companion documents:** Architecture.md v1.0, CellTaxonomy.md v1.0, PersistenceLayer.md v1.0, McpProtocol.md v1.0, NATIVE_TRINITY.md v1.0, DeepSeekOptimization.md v1.0.

---

## §0 — Document Conventions

- **MUST / SHOULD / MAY** follow RFC 2119.
- **SPEC-DERIVED-§N.N** markers reference Architecture.md.
- **[GAP-NNN]** / **[GAP-NT-NNN]** / **[GAP-DS-NNN]** track open items.
- All times are logical clocks unless suffixed `*_wall`.

---

## §1 — Mission

The Router is the L2 ingress. It authenticates, looks up destination shard, verifies the WorkBudget envelope, runs the pre-validation hook chain (Architecture.md §15.9), enqueues on the destination shard's inbox, applies backpressure, and observes per-task forward-progress signals (`task.no_progress`, P18).

The Scheduler is the per-shard dispatch policy engine honoring:

- token-budget-aware routing (§5),
- severity-aware routing (§6),
- gap-aware loop detection (§7),
- deadline + priority class WRR (§8),
- prefix-cache-aware view assembly (§9, DeepSeek optimization).

L2 holds **no domain logic**.

---

## §2 — Topology

```
                ┌──────────────────────────┐
       MCP ───▶│        ROUTER             │── pre-validation ──▶ TRINITY SHARD
                │  auth · lookup · budget   │     hook chain        ┌────────────┐
                │  intent → shard           │                       │ Contract   │
                └────────────┬─────────────-┘                       │ SpecAnchor │
                             │                                      │ Decision   │
                             ▼                                      │ WorkBudget │
                ┌──────────────────────────┐                        │ Congruence │
                │  PER-SHARD SCHEDULER     │                        │ Gap        │
                │  3 priority queues       │                        │ Quarantine │
                │  deadline heap           │                        └────────────┘
                │  gap dispatch counter    │
                │  WRR weights             │
                └────────────┬─────────────┘
                             │
                             ▼
                       Cell `on_query` / `on_update`
```

---

## §3 — Capability Tokens

```rust
struct CapToken {
    issuer:            NamespaceId,
    agent_id:          AgentId,
    cell_scope:        CellTypeSet,
    ops_allowed:       OpSet,           // {query, update, subscribe, supersede}
    facet_scope:       FacetGlob,       // NEW v1.0 — e.g., "facets.path: src/router/**"
    expiry:            u64,             // logical
    tokens_per_window: Option<u32>,
    macaroon_caveats:  Vec<Caveat>,
    sig:               Ed25519Sig,
}
```

- **F1** — facet-glob scoping is enforced **before** dispatch. Mismatch → `PermissionDenied`. **SPEC-DERIVED-§12.**
- **F2** — verification is in-process; no network calls; target ≤ 2 μs p99.

---

## §4 — Envelope & Intent Routing

```rust
struct Envelope {
    request_id:  Ulid,
    agent_id:    AgentId,
    capability:  CapToken,
    work_budget: WorkBudget,
    spec_anchor: Option<AnchorRef>,
    intent:      Intent,
    payload:     Payload,
    severity:    Severity,
    gap_ref:     Option<GapId>,
    task_id:     TaskId,
    seed:        u64,
    logical_at:  u64,
}

enum Intent { Recall, Hydrate, Write, Subscribe, View, Supersede }
```

Lookup: `intent + payload.cell_kind → candidate Cells (Catalog read) → stable hash of (payload.key, namespace_id) → exactly one Cell → cell_id → shard_id`. Two BTree reads = O(log n), ≤ 1 μs p99.

---

## §5 — Token-Budget-Aware Routing (P12)

UltraCortex's competitive differentiator. Enforced:

- **R1** — `recall` MUST NOT return payload > `tokens_remaining`.
- **R2** — `view` MUST NOT assemble bundle > `tokens_remaining`.
- **R3** — Tier escalation (L0 → L1 → L2 → L3) only when lower tier fails the agent's success predicate.
- **R4** — Repeated calls in one `task_id` share the budget envelope.

Tier policy (default):

| Tier | Content | Token cost |
|------|---------|------------|
| L0 | skeletons only (≤ 80 tok each) | ≤ 500 tok |
| L1 | skeletons + symbolic handles | ≤ 1.5 KB |
| L2 | skeletons + top-3 hydrated bodies | ≤ 4 KB |
| L3 | full hydration (escape) | unbounded; emits `view.budget_override` |

Scheduler MAY return `BudgetInsufficient` with recommended tier; agent MAY re-issue with explicit override (audited).

Namespace default grants are `admin=250_000`, `bootstrap=1_000_000`, `curator=10_000_000`, with fallback `100_000`, surfaced through `budget defaults`. The checked-in acceptance bench (`tests/acceptance_bench.rs`) records `856` bytes p50 and `1052` bytes p99 on the representative DeepSeek workload.

---

## §6 — Severity-Aware Routing (P0/P1/P2)

| Severity | Meaning | Scheduler Behavior |
|----------|---------|--------------------|
| **P0**   | Must fix before forward progress | Bypass priority queue; head-of-line; preempts P1/P2 |
| **P1**   | Should fix soon                  | Background queue; logged; non-blocking |
| **P2**   | Best-effort                       | Bulk class; preemptable; coalescable |

- **S1** — P0 envelope bypasses priority queue.
- **S2** — cross-Cell spawns preserve the parent severity by default (`spawn_severity = parent_severity` unless explicitly demoted with Decision).
- **S3** — P2 envelopes MAY be coalesced (same `(cell_id, op_kind)` within 1 ms).

---

## §7 — Gap-Aware Loop Detection (Anti-Fixation, P18)

Tracked state:
- `envelope.gap_ref` — GapId being worked on.
- `gap_dispatch_counter[gap_id]` — count of envelopes dispatched against this gap.
- `gap_last_transition[gap_id]` — logical timestamp of last status change.

```
on dispatch(env):
    if env.gap_ref is None: pass
    else:
        gap_dispatch_counter[env.gap_ref] += 1
        cnt = gap_dispatch_counter[env.gap_ref]
        if cnt - last_tx_seq[env.gap_ref] >= N:    // N default 8
            emit("task.no_progress", env.gap_ref, env.task_id)
            quarantine(env, cause = NoForwardProgress)
            return Quarantined
    dispatch_normally(env)
```

On `task.no_progress`:
1. Scheduler does **not** retry.
2. Envelope → `QuarantineCell`.
3. `WorkBudgetCell` snapshots envelope.
4. Originating agent receives `Fixation`.
5. `decision.required` emitted to escalation chain.

**Substrate-level realization of "never silently loop."**

---

## §8 — Priority Classes & Deadlines

| Class       | Weight (default) | Use case |
|-------------|------------------|----------|
| Interactive | 8 | Hot-path queries |
| Background  | 2 | Indexing, congruence recompute |
| Bulk        | 1 | Snapshots, GC, exports |

- Deadline heap checked on every dequeue. Expired envelopes → `QuarantineCell` cause=`DeadlineExceeded`. **Never silent.**
- Per-shard inbox bounded (default 4096). Overflow → `RateLimited` to caller.
- Per-namespace token bucket (default 10k req/s; configurable).

---

## §9 — Prefix-Cache-Aware View Assembly (DeepSeek)

On every `view` request:

1. Compute `view_key = (view_id, namespace_id, view_version, params_canonical_hash)`.
2. Query `PrefixCacheStore` (PersistenceLayer.md §9). Hit → return cached prefix-stable bytes (≤ 5 μs).
3. Miss → assemble canonically (Architecture.md §14.3), persist to PrefixCacheStore, return.

**Canonical view layout:**

```
view_header (fixed): schema_id, view_version, namespace_id, params_canonical_hash, logical_at
handles_section (lex-sorted by handle)
skeletons_section (lex-sorted by handle)
bodies_section (only if tier ≥ L2; lex-sorted)
footer: hydrate_endpoints, supersedes_handles, tokens_emitted
```

Lex-sorting at every level guarantees a long shared prefix on re-emission.

**Targets:**
- Prefix-cache hit rate **≥ 80%**.
- View assembly latency: ≤ 100 μs p99 cache miss; ≤ 5 μs p99 cache hit.

The checked-in acceptance bench records an `84.21%` prefix-cache hit rate on the representative workload. **[GAP-NT-014]** eviction policy.

---

## §10 — Pre-Validation Hook Chain Dispatch

Every state-changing envelope (Write, Supersede) passes:

```
ContractCell.validate_schema(env)
    ↓ (ok)
SpecAnchorCell.validate_anchor(env)
    ↓ (ok)
DecisionLedgerCell.check_conflicts(env)
    ↓ (ok)
WorkBudgetCell.charge_pre(env, est_tokens)
    ↓ (ok)
CongruenceCell.preview_delta(env)
    ↓ (ok)
→ enqueue on target shard
```

Any failure → `QuarantineCell.absorb(env, cause)` — never silent reject.

**Latency target:** ≤ 25 μs p50, ≤ 100 μs p99 (co-located on Trinity shard).

---

## §11 — Trinity Shard Co-Location

All 7 Trinity Cells default to a single dedicated shard. The only supported small-deployment override is `co-tenant-shard-0`, surfaced by bootstrap as `trinity_topology`. Rationale: every state-changing envelope touches all 7; co-location → same-shard mmap reads, not network hops.

Saturation → backpressure + `trinity.saturated` event. Remedy: per-namespace Trinity sharding (Phase 2).

For Curator placement, small deployments follow the same policy surface: bootstrap defaults to dedicated placement and accepts `co-tenant-shard-0` as the supported override via `curator.topology`.

---

## §12 — Quarantine Integration

Replaces the generic DLQ from Cortex v0.1. Every poisoned envelope, every chain failure, every deadline overrun, every fixation halt → `QuarantineCell` with cause tagged. **No envelope is ever silently lost.**

---

## §13 — Subscription Fan-Out

Router maintains a fan-out table keyed by event pattern. On every `node.written` / `node.superseded` / `decision.applied` / `task.budget.exceeded`, walks the trie in `SubscriptionCell` and pushes onto each matching subscriber's stream.

- Best-effort but durable: full queue → buffered to L0 with TTL.
- Trinity events (`decision.conflict`, `anchor.orphaned`, `task.no_progress`) ALWAYS delivered to escalation list.

---

## §14 — Determinism

The Scheduler MUST be deterministic given identical WAL + envelope order + `seed`:

- no wall-clock reads in dispatch loop,
- no hash randomization (pinned `ahash` seed in `ultracortex.toml`),
- no thread-local PRNG (all randomness from `envelope.seed`).

---

## §15 — Performance Targets

| Operation | p50 | p99 |
|-----------|-----|-----|
| auth + lookup | 1.5 μs | 4 μs |
| pre-validation chain | 25 μs | 100 μs |
| envelope enqueue | 0.5 μs | 2 μs |
| view assembly (cache hit) | 5 μs | 20 μs |
| view assembly (cache miss, L1) | 100 μs | 500 μs |
| recall (L0 skeletons, k=10) | 30 μs | 150 μs |
| write (durable, group-committed) | 100 μs | 1 ms |

---

## §16 — GAPs

| ID | Description |
|----|-------------|

---

## §17 — Congruence Contract

Congruent with: Architecture.md (§4, §7, §14, §15), CellTaxonomy.md (Trinity Cells), McpProtocol.md (envelope), PersistenceLayer.md (PrefixCacheStore), NATIVE_TRINITY.md (hook chain), DeepSeekOptimization.md (view layout).

_End of RouterScheduler.md v1.0 (UltraCortex)._


---

# 🆙 UltraCortex v1.0 Delta — Semantic-Check Gate, Adjudicator Path, Negation Glob

The HyperCortex content above remains normative for the base Router/Scheduler. UltraCortex v1.0 adds three integration points for the Curator Pair (P19, P20).

## §A.1 New Envelope Field: `flags.semantic_check`

```rust
struct EnvelopeFlags {
    semantic_check: bool,   // NEW v1.0 — invokes WardenCell sync gate after Trinity chain
}
```

- `false` (default) → standard hot path. Latency unchanged.
- `true` → after Trinity pre-validation succeeds, Router invokes `WardenCell.judge(env)` sync (~140 ms p50, ~350 ms p99).
- Auto-set `true` when `severity == P0`.

## §A.2 New Pre-Validation Hook (Step 6, Optional)

Per UltraCortex Architecture §15, the chain now extends:

```
1. ContractCell.validate_schema      (≤25 μs p99)
2. SpecAnchorCell.validate_anchor    (≤25 μs p99)
3. DecisionLedgerCell.check_conflicts (≤25 μs p99)
4. WorkBudgetCell.charge_pre          (≤25 μs p99)
5. CongruenceCell.preview_delta       (≤25 μs p99)
6. WardenCell.judge   [OPTIONAL]      (~140 ms p50, ~350 ms p99) — only if flags.semantic_check OR severity=P0
```

Steps 1–5: hot-path Trinity chain, unchanged.
Step 6: gated semantic check; LibrarianCell reverse-flow runs if Warden flags.

## §A.3 New Adjudicator Escalation Path

When LibrarianCell and WardenCell produce conflicting outputs on the same envelope:
1. Router invokes `AdjudicatorCell.adjudicate(librarian_pub, warden_pub, substrate_slice)`.
2. Deterministic resolution first (~70–80% of cases, ≤200 μs p99).
3. Rotating LLM pool for unresolved cases (~20–30%, ~300 ms p50, ~700 ms p99).
4. Human escalation (~1–2%).
5. Resolution recorded as Decision in DecisionLedgerCell + CrossCheckRecord in CrossCheckLedgerCell.

## §A.4 Async Fan-Out to LibrarianCell

On every `node.written` event from Memory/Payload Cells (FactCell, TimelineCell, BlobCell), Router enqueues an async curation job to the Librarian shard. **Latency-decoupled from the writer.** Output lands as PENDING; Warden audits before Active.

## §A.5 Capability-Token Negation Glob (P19 enforcement)

```
facet_scope: "librarian/output/*  !rationale.*  !considered_alts.*  !reasoning_trace.*  !confidence_precise"
```

Router rejects `hydrate` requests matching exclude globs → emits `curator.rationale_access_denied`. **This metric MUST be non-zero in production** — zero = guardrail silently disabled → next HALT fails. **[GAP-CU-012]** canonicalization.

## §A.6 New Trinity Events (always delivered to escalation list)

`curator.suspicious_agreement`, `curator.calibration_drift_detected`, `curator.rationale_access_denied`, `adjudicator.invoked`, `adjudicator.resolution`.

## §A.7 New GAPs (Router-scoped subset)

| ID | Description |
|---|---|
| GAP-CU-008 | Adjudicator rotation policy |
| GAP-CU-012 | Negation glob canonicalization |

## §A.8 Congruence Contract (Updated)

Congruent with: Architecture-UltraCortex.md (§§7, 16–18), **CURATOR_PAIR_PROTOCOL.md**, **LibrarianCell.md**, **WardenCell.md**, **AdjudicatorCell.md**, NATIVE_TRINITY.md (Trinity governs Curators), McpProtocol.md (new error codes + envelope flags).

_End of UltraCortex v1.0 Delta._
