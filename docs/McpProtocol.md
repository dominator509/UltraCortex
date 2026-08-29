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

### §0.1 — Current Checkout Wire Contract

The concrete v1 implementation uses canonical CBOR maps. The pseudo-Rust
shapes in later sections describe logical result content; they are not
additional wire envelopes.

Request envelopes contain:

- `proto_version` (u64, exactly 1), `request_id` (ULID string), and
  `agent_id` (string);
- `capability`, mandatory `work_budget` (`task_id` string plus `units` u64),
  `intent`, `payload`, optional `spec_anchor`, `severity`, optional `gap_ref`,
  and `tier`;
- `seed` (u64) and `flags` (`semantic_check`, `continuation`).

The response envelope always contains `request_id`, `ok`, `result`, optional
`err_code`, `err_message`, and `quarantine_id`, plus `tokens_emitted`,
`next_tier_hint`, `logical_at`, and the request `seed`. A response mirrors the
request seed even when the result is an error.

The handshake is a `hello` map followed by `hello_ack`. The current hello
requires `type`, `proto_version`, and `agent_id`; the acknowledgement reports
`node_id`, `proto_version`, and capability bits. Subscription delivery uses
authenticated `events` and `events_ack` pull messages, not unsolicited push
frames.

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
- `continuation` is available for truncated request flows. Subscription
  delivery is the authenticated `events` / `events_ack` pull protocol in §4.4
  and §9.

---

## §3 — Envelope Format

```rust
struct Envelope {
    proto_version: u64,           // = 1 in v1.0
    request_id:    Ulid,
    agent_id:      String,
    capability:    CapToken,
    work_budget:   WorkBudget,    // MANDATORY (P12)
    intent:        Intent,        // Recall | Hydrate | Write | Subscribe | View | Supersede
    payload:       Payload,
    spec_anchor:   Option<String>, // "Doc.md§Section"
    severity:      Severity,      // P0 | P1 | P2
    gap_ref:       Option<GapId>,
    tier:          Tier,
    seed:          u64,
    flags:         EnvelopeFlags,
}

struct WorkBudget {
    task_id: String,
    units:    u64,
}

struct EnvelopeFlags {
    semantic_check: bool,
    continuation:  bool,
}
```

**Invariants:**
- **E1** — `proto_version == 1` is required.
- **E2** — `work_budget` is required on every envelope; Router enforces
  available units and there is no free work.
- **E3** — `intent` must be known and state-changing intents must carry a
  non-null payload. The request `seed` is mirrored by the response.
- **E4** — `request_id` must be a valid ULID.

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
    pattern: String,
    since:   u64,
}
```

The Router persists the subscription registration before acknowledging it.
The current transport provides authenticated pull delivery:

- `events` drains pending events for the subscribed agent; an optional
  `since` cursor replays retained events with a later sequence.
- `events_ack` acknowledges the delivered sequence/cursor.
- Each returned event contains its sequence, event name, logical time, and
  payload.

There is no unsolicited server push channel in the current v1 transport.
The `continuation` envelope flag is for truncated request flows, not an
implicit stream.

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

## §5 — Response Envelope and Prefix Stability

Every request receives one canonical CBOR response envelope. Per-verb result
content is carried in `result`; the wrapper is:

```rust
struct ResponseEnvelope {
    request_id:       Ulid,
    ok:               bool,
    result:           CanonicalCbor,
    err_code:         Option<ErrCode>,
    err_message:      Option<String>,
    quarantine_id:    Option<String>,
    tokens_emitted:   u64,
    next_tier_hint:   Option<Tier>,
    logical_at:       u64,
    seed:             u64,
}
```

The response `seed` equals the request `seed`. Successful
recall, hydrate, and view result maps retain canonical CBOR ordering and the
Router's documented handle/skeleton/body ordering. The response wrapper uses
canonical map encoding rather than the blueprint's separate receipt/wal
fields.

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

## §7 — Handshake and Schema Negotiation

The current handshake is intentionally small and capability-based:

```rust
struct Hello {
    type:          "hello",
    proto_version: u64,
    agent_id:      String,
}

struct HelloAck {
    type:             "hello_ack",
    node_id:          String,
    proto_version:    u64,
    capability_bits:  Map<String, BoolOrArray>,
}
```

The acknowledgement advertises semantic checking, supported tiers, built-in
views, the CrossCheck ledger, authenticated event pull, and the requirement
for an operator capability on admin messages. Contract migrations remain
tracked through the ContractCell admin surface:
`contract plan-migration`, `contract verify-migration`, and
`contract apply-migration`.

Mismatches produce a structured `ContractViolation`; there is no
unversioned downgrade that changes the v1 field contract.

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

## §9 — Subscription Pull Semantics

For `subscribe`:

- Registration is persisted in the SubscriptionCell WAL before success is
  returned.
- `events` returns pending events for the authenticated agent.
- A `since` cursor replays retained ring events with a later sequence
  and respects the subscription's activation cursor.
- `events_ack` acknowledges the delivered cursor.
- The in-memory event ring retains up to 4096 events; pending queues provide
  bounded local backpressure.
- Reconnect by issuing `events` with the last acknowledged cursor.

The current transport does not open an asynchronous server-push stream. Event
names under `trinity.`, `curator.`, and `node.fatal` are
always delivered to registered operator/escalation recipients; other events
require a matching subscription pattern.

---

## §10 — Versioning and Handshake

- `proto_version` is a u64 field in every request and response
  negotiation. v1.0 uses value `1`.
- Capability bits are advertised in `hello_ack`; there is no separate
  minor-version field in the current handshake.
- Breaking changes require a new major version and a documented parallel
  support window.

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
    sig:               HmacSha256,
}
```

The current single-node implementation uses HMAC-SHA256 behind the `Signer` trait; an Ed25519 implementation remains a future multi-node seam. Revocation is checked against the `AgentRegistryCell` at the protocol boundary.

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
    continuation:  bool,    // continue a truncated request flow
}
```

When `true`, Router invokes `WardenCell.judge(env)` synchronously after the Trinity pre-validation chain completes. Auto-set `true` when `severity == P0`.

## §A.2 NEW Error Codes (Trinity/Curator-Related)

| Code | Trigger | Recovery |
|---|---|---|
| **`SemanticDrift`** | WardenCell flagged envelope as drifting from canonical substrate | Envelope → QuarantineCell with grounded counter-rationale. Caller receives `quarantine_id`. |
| **`HallucinationDetected`** | Warden detected hallucinated handle/fact in envelope | Same; QuarantineCell with `unknown_handles: Vec<Handle>` provenance. |
| **`AdjudicationPending`** | LibrarianCell and WardenCell disagreed; AdjudicatorCell resolving | Caller MAY poll via `request_id` or use authenticated `events` pull delivery. |

All three errors return either a structured response or a `quarantine_id` — **never silent drop** (consistent with Mission §1.3).

## §A.3 Capability Token: Negation Glob Support (NEW)

```rust
struct CapToken {
    facet_scope: FacetGlob,   // NOW supports negation:
                              //   "<include_glob>  [!<exclude_glob>]*"
                              // e.g., "librarian/output/*  !rationale.*"
}
```

This is what enforces **P19 (Asymmetric Visibility)** at the protocol level. The Warden literally cannot `hydrate` a Librarian rationale blob — the Router rejects the request and emits `curator.rationale_access_denied`. Negation-glob canonicalization is implemented and covered by local conformance/self-tests.

## §A.4 New Capability Bit on Handshake

```rust
struct Hello {
    capability_bits: BitSet,
    // NEW bits:
    //   semantic_check_supported  — client can request Warden sync gate
    //   curator_rationale_optout — client confirms it understands rationale fields are gated
}
```

## §A.5 Event Delivery (subscribe)

The current transport uses authenticated pull rather than an always-delivered
push stream. The EventBus always-deliver prefixes are `trinity.`, `curator.`,
and `node.fatal` for registered operator/escalation recipients. Events such
as `cross_check.record_appended` are delivered when the agent has a matching
subscription pattern; they are not silently persisted as an unsolicited
socket frame.

## §A.6 Protocol-Scoped GAPs

| ID | Description |
|---|---|
| GAP-CU-012 | Capability-token negation glob canonicalization | closed in current checkout; retain regression coverage |

## §A.7 Congruence Contract (Updated)

Congruent with: Architecture-UltraCortex.md (§9, §16–§18), **CURATOR_PAIR_PROTOCOL.md** (§3, §4 — protocol-level enforcement of P19), **LibrarianCell.md**, **WardenCell.md**, **AdjudicatorCell.md**, RouterScheduler.md (semantic_check gate + Adjudicator path), ObservabilityAudit.md (new audit events).

_End of UltraCortex v1.0 Delta._
