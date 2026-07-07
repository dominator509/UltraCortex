# McpProtocol.md — **UltraCortex** MCP Protocol Surface Specification

**Status:** v1.0 — Normative L3 Specification
**Owner:** Dominic Sarria-Wiley
**Companion documents:** Architecture.md v1.0, RouterScheduler.md v1.0, CellTaxonomy.md v1.0, PersistenceLayer.md v1.0, NATIVE_TRINITY.md v1.0, DeepSeekOptimization.md v1.0.

---

## §0 — Document Conventions

- **MUST / SHOULD / MAY** follow RFC 2119.
- **SPEC-DERIVED-§N.N** markers reference Architecture.md.
- All bytes little-endian. Payloads are **canonical CBOR** (RFC 8949) — keys lex-sorted, no indefinite-length items, no duplicate keys.
- All times are logical clocks; wall-clock fields suffixed `*_wall`.

---

## §1 — Mission

The MCP protocol surface is the **product** of UltraCortex (Architecture.md P8). Storage tiers, sharding, even the Rust core may evolve; the protocol MUST remain stable.

Four non-negotiable properties:

1. **Budget-aware** — every request carries a `WorkBudget`; server enforces.
2. **Prefix-stable** — every response in canonical order for DeepSeek prefix-cache reuse.
3. **Quarantine-safe** — no silent drops; failures return structured errors or `quarantine_id`.
4. **Deterministic** — same WAL + same `seed` → byte-identical responses on replay.

---

## §2 — Transport

### §2.1 UDS (preferred)

- Default path: `/run/ultracortex/ultracortex.sock`.
- Persistent connection per agent; multiplexed via `request_id`.
- Permissions: `0600`.

### §2.2 TCP

- Default port: `7741`.
- Plaintext TCP is supported only on loopback (`127.0.0.1` / `::1`) for local tooling.
- Non-loopback listener addresses are rejected at config/load time and again before bind; this checkout does not expose LAN or multi-host TCP transport.
- UDS remains the preferred transport. A future multi-host transport may add TLS/mTLS, but it is not part of the enforced v1.0 default.

### §2.3 Framing

```
+----------------------+----------------------+
| u32 LE frame length  | CBOR-encoded payload |
+----------------------+----------------------+
```

- Max frame: 16 MiB (hard cap 256 MiB).
- One frame = one envelope or one response.
- Streams (subscribe) use continuation flag.

---

## §3 — Envelope Format

```rust
struct Envelope {
    proto_version: u8,            // = 1 in v1.0
    request_id:    Ulid,
    agent_id:      Ulid,
    capability:    CapToken,
    work_budget:   WorkBudget,    // MANDATORY (P12)
    spec_anchor:   Option<AnchorRef>,
    intent:        Intent,        // Recall | Hydrate | Write | Subscribe | View | Supersede
    payload:       Payload,
    severity:      Severity,      // P0 | P1 | P2
    gap_ref:       Option<GapId>,
    task_id:       TaskId,
    seed:          u64,
    logical_at:    u64,
    continuation:  bool,
}

struct WorkBudget {
    tokens_remaining: u32,
    deadline_logical: u64,
    retry_count:      u8,
    severity:         Severity,
}

struct AnchorRef { doc: SmallString, section: SmallString }
```

**Invariants:**
- **E1** — `work_budget.tokens_remaining > 0` REQUIRED unless `intent == Subscribe`.
- **E2** — `task_id` REQUIRED on Write/Supersede.
- **E3** — `seed` MUST propagate into response envelope.
- **E4** — `request_id` unique per `(agent_id, connection)`.

---

## §4 — Verbs

### §4.1 `recall`

```rust
struct RecallReq {
    cell_kind: CellKind,
    query:     QueryExpr,
    k:         u16,
    tier:      Tier,          // L0 | L1 | L2
    filters:   Vec<Filter>,
}

struct RecallResp {
    skeletons:      Vec<Skeleton>,   // ≤ k, lex-sorted by handle
    handles:        Vec<Handle>,     // when tier ≥ L1
    bodies:         Vec<Body>,       // when tier == L2
    truncated:      bool,
    tokens_used:    u32,
    next_tier_hint: Option<Tier>,
}
```

### §4.2 `hydrate`

```rust
struct HydrateReq  { handles: Vec<Handle> /* lex-sort */ }
struct HydrateResp { bodies: Vec<Body>, tokens_used: u32 }
```

### §4.3 `write`

```rust
struct WriteReq {
    target_cell: CellId,
    op:          WriteOp,
    payload:     CanonicalCbor,
    schema_id:   SchemaId,
    supersedes:  Option<NodeId>,
    decision:    Option<DecisionRef>,
}

struct WriteResp {
    receipt:        Receipt,
    wal_offset:     u64,
    logical_at:     u64,
    anchor_created: Option<AnchorRef>,
}
```

Pre-validation chain runs before `on_update`. Failure → `QuarantineCell.absorb()` and client receives `Quarantined { quarantine_id, cause }`, never a silent drop.

### §4.4 `subscribe`

```rust
struct SubscribeReq {
    patterns: Vec<EventPattern>,
    since:    Option<u64>,
}
```

Server streams `EventFrame { event_kind, payload, logical_at }`. Trinity events (`decision.conflict`, `anchor.orphaned`, `task.no_progress`, `task.budget.exceeded`, `task.quarantined`, `contract.deprecated`) ALWAYS delivered to escalation-list subscribers.

### §4.5 `view`

```rust
struct ViewReq {
    view_id:    ViewId,
    params:     CanonicalCbor,
    tier:       Tier,
    view_version: Option<Version>,
    allow_migrate: bool,
    formatting: Formatting,     // Default | DeepSeekFim | DeepSeekR1
}

struct ViewResp {
    view_bytes:     Bytes,       // prefix-stable per RouterScheduler.md §9
    view_version:   Version,
    migrated_from:  Option<Version>,
    view_key:       ViewKey,
    cache_hit:      bool,
    tokens_emitted: u32,
}
```

**Primary delivery vehicle for context-as-view (P11). DeepSeek prefix-cache benefits accrue here.**

Compatibility contract:
- missing `view_version` means "serve the current version";
- `view_version == current` serves normally;
- `view_version < current` rejects with `ContractViolation` unless `allow_migrate = true`, in which case the server serves the current view bytes and sets `migrated_from`;
- `view_version > current` always rejects with `ContractViolation`.

### §4.6 `supersede`

```rust
struct SupersedeReq {
    target:           NodeId,
    new:              NodeId,
    rationale_handle: BlobHandle,
}

struct SupersedeResp { receipt: Receipt, logical_at: u64 }
```

Only way to invalidate a Decision (DecisionLedgerCell invariant D2).

---

## §5 — Prefix-Stable Response Layout

For every response with visible bytes (`recall`, `hydrate`, `view`), the serializer MUST emit fields in this canonical order:

1. **header** — fixed order: `schema_id`, `view_version`, `namespace_id`, `params_canonical_hash`, `logical_at`.
2. **handles[]** — lex-sorted by handle string.
3. **skeletons[]** — lex-sorted by handle.
4. **bodies[]** — lex-sorted by handle (when present).
5. **footer** — `hydrate_endpoints`, `supersedes_handles`, `tokens_emitted`.

Inside any record, fields lex-sorted. CBOR map keys canonically ordered.

This is what allows DeepSeek's prefix cache to hit the entire shared prefix when two views differ only in newly-appended content.

---

## §6 — DeepSeek-Specific Extensions

### §6.1 FIM framing (DeepSeek-Coder)

When `Formatting::DeepSeekFim`, callers supply `prefix` / `suffix` in the request payload:

```
<|fim_begin|>{prefix}<|fim_hole|>{suffix}<|fim_end|>
```

Supported variants:
- `client_kind = "deepseek-coder"` → emit the real FIM tags above.
- `client_kind = "deepseek-v3"` or `client_kind = "deepseek-r1"` → downgrade to a plain `prefix + suffix` splice with no coder-only tokens.

### §6.2 R1 `<think>` strip-and-replay

When `Formatting::DeepSeekR1`:
- Prior reasoning skeletons emitted with `<think>` stripped.
- Agent receives only the conclusion.
- Original chain fetchable via `hydrate(handle.with_facet("reasoning"))`.
- `seed` forwarded → R1 replay determinism.
- `include_reasoning: true` opts back in.

**[GAP-DS-003]** canonical strip format.

### §6.3 Function-call grammar

DeepSeek's preferred format:
- lowercase verb names,
- single-level argument objects (no nested oneOf),
- explicit `required` arrays,
- canonical key ordering.

`tools_manifest` endpoint serves it on demand.

### §6.4 Temperature/seed propagation

Every `Envelope.seed` mirrored in `ResponseEnvelope.seed`. Agents SHOULD pass it back to the LLM call.

---

## §7 — Schema Negotiation (ContractCell)

Handshake `Hello` frame:

```rust
struct Hello {
    proto_version:     u8,
    client_kind:       String,    // "deepseek-v3" | "deepseek-r1" | "deepseek-coder" | ...
    capability_bits:   BitSet,    // {deepseek_coder, deepseek_r1_strip, fim, streaming, ...}
    requested_schemas: Vec<SchemaId>,
}
```

Server `HelloAck`:

```rust
struct HelloAck {
    proto_version: u8,
    accepted:      Vec<(SchemaId, Version)>,
    rejected:      Vec<(SchemaId, RejectReason)>,
    server_caps:   BitSet,
}
```

Mismatches → negotiated downgrade or connection-level `ContractViolation`. Contract migrations are now tracked through the ContractCell admin surface: `contract plan-migration`, `contract verify-migration`, and `contract apply-migration`.

---

## §8 — Error Codes

```rust
struct ErrorResp {
    code:                ErrCode,
    message:             SmallString,
    quarantine_id:       Option<QuarantineId>,
    cause_chain:         Vec<ErrCode>,
    retry_after_logical: Option<u64>,
    spec_anchor:         Option<AnchorRef>,
}
```

| Code | Trinity? | Meaning |
|------|----------|---------|
| `Quarantined`        | yes | message absorbed by QuarantineCell |
| `BudgetExceeded`     | yes | WorkBudget exhausted |
| `Fixation`           | yes | gap-aware loop detector tripped |
| `AnchorMissing`      | yes | SpecAnchor required but absent |
| `ContractViolation`  | yes | schema mismatch |
| `DecisionConflict`   | yes | conflicting active Decision |
| `CongruenceDelta`    | yes | unaccepted congruence delta |
| `PermissionDenied`   | no  | capability rejected |
| `RateLimited`        | no  | backpressure |
| `DeadlineExceeded`   | no  | past `deadline_logical` |
| `NotFound`           | no  | unknown handle |
| `Unimplemented`      | no  | feature behind a GAP |
| `Internal`           | no  | last resort; auditable |

**Critical:** no error silently drops. Every Trinity error returns a structured response or `quarantine_id`.

---

## §9 — Streaming Semantics

For `subscribe`:
- Each event frame independently CBOR-encoded.
- `logical_at` monotonically increasing.
- Backpressure: full agent buffer → Router buffers up to N=4096 frames to L0; beyond → `subscription.paused`.
- Resume: reconnect with `since: <last_logical_at>`.

---

## §10 — Versioning & Handshake

- Major version = first byte of every envelope (`proto_version`). v1.0 = `0x01`.
- Minor version inside `Hello`.
- Breaking changes require new major + ≥ 6 month parallel support window.

---

## §11 — Capability Token (Wire Format)

```rust
struct CapToken {
    version:           u8,
    issuer_ns:         NamespaceId,
    agent_id:          AgentId,
    cell_scope:        CellTypeSet,
    ops_allowed:       OpSet,
    facet_scope:       FacetGlob,     // NEW v1.0
    expiry:            u64,
    tokens_per_window: Option<u32>,
    caveats:           Vec<Caveat>,
    sig:               Ed25519Sig,
}
```

In-process verification (≤ 2 μs p99). Revocation lazily replicated from `AgentRegistryCell` to Router; SLO ≤ 100 ms.

---

## §12 — GAPs

| ID | Description |
|----|-------------|
| GAP-DS-003 | R1 `<think>` canonical strip format |

---

## §13 — Congruence Contract

Congruent with: Architecture.md (§9, §14, §15), CellTaxonomy.md (Trinity), RouterScheduler.md (envelope, hook chain), PersistenceLayer.md (PrefixCacheStore), NATIVE_TRINITY.md, DeepSeekOptimization.md.

_End of McpProtocol.md v1.0 (UltraCortex)._


---

# 🆙 UltraCortex v1.0 Delta — Envelope Flag, Error Codes, Negation Glob

The HyperCortex L3 wire format above remains normative. UltraCortex v1.0 adds one envelope field, three error codes, and capability-token negation glob support.

## §A.1 New Envelope Flag

```rust
struct EnvelopeFlags {
    semantic_check: bool,   // NEW v1.0 — invoke WardenCell sync gate
}
```

When `true`, Router invokes `WardenCell.judge(env)` synchronously after the Trinity pre-validation chain completes. Auto-set `true` when `severity == P0`.

## §A.2 NEW Error Codes (Trinity/Curator-Related)

| Code | Trigger | Recovery |
|---|---|---|
| **`SemanticDrift`** | WardenCell flagged envelope as drifting from canonical substrate | Envelope → QuarantineCell with grounded counter-rationale. Caller receives `quarantine_id`. |
| **`HallucinationDetected`** | Warden detected hallucinated handle/fact in envelope | Same; QuarantineCell with `unknown_handles: Vec<Handle>` provenance. |
| **`AdjudicationPending`** | LibrarianCell and WardenCell disagreed; AdjudicatorCell resolving | Caller MAY poll via `request_id` or receive async resolution event on subscribed stream. |

All three errors return either a structured response or a `quarantine_id` — **never silent drop** (consistent with Mission §1.3).

## §A.3 Capability Token: Negation Glob Support (NEW)

```rust
struct CapToken {
    facet_scope: FacetGlob,   // NOW supports negation:
                              //   "<include_glob>  [!<exclude_glob>]*"
                              // e.g., "librarian/output/*  !rationale.*"
}
```

This is what enforces **P19 (Asymmetric Visibility)** at the protocol level. The Warden literally cannot `hydrate` a Librarian rationale blob — the Router rejects the request and emits `curator.rationale_access_denied`. **[GAP-CU-012]** canonicalization.

## §A.4 New Capability Bit on Handshake

```rust
struct Hello {
    capability_bits: BitSet,
    // NEW bits:
    //   semantic_check_supported  — client can request Warden sync gate
    //   curator_rationale_optout — client confirms it understands rationale fields are gated
}
```

## §A.5 New Always-Delivered Trinity Events (subscribe)

In addition to the existing always-delivered Trinity events:

- `curator.suspicious_agreement` (>99% Librarian/Warden agreement → collusion signal)
- `curator.calibration_drift_detected`
- `curator.rationale_access_denied` (proves P19 active; MUST be non-zero)
- `adjudicator.invoked`
- `adjudicator.resolution`
- `cross_check.record_appended`

## §A.6 New GAPs (Protocol-scoped)

| ID | Description |
|---|---|
| GAP-CU-012 | Capability-token negation glob canonicalization |

## §A.7 Congruence Contract (Updated)

Congruent with: Architecture-UltraCortex.md (§9, §16–§18), **CURATOR_PAIR_PROTOCOL.md** (§3, §4 — protocol-level enforcement of P19), **LibrarianCell.md**, **WardenCell.md**, **AdjudicatorCell.md**, RouterScheduler.md (semantic_check gate + Adjudicator path), ObservabilityAudit.md (new audit events).

_End of UltraCortex v1.0 Delta._
