# ObservabilityAudit.md — **UltraCortex** Observability & Audit Specification

**Status:** v1.0 — Normative Observability & Audit Specification
**Owner:** Dominic Sarria-Wiley
**Companion documents:** Architecture.md v1.0, CellTaxonomy.md v1.0, RouterScheduler.md v1.0, PersistenceLayer.md v1.0, McpProtocol.md v1.0, NATIVE_TRINITY.md v1.0, DeepSeekOptimization.md v1.0.

---

## §0 — Document Conventions

- **MUST / SHOULD / MAY** follow RFC 2119.
- **SPEC-DERIVED-§N.N** markers reference Architecture.md.
- Logical clocks authoritative for ordering; wall-clock fields display-only.
- All exported records UTF-8 canonical JSON (or OTLP binary).

---

## §1 — Mission

Four pillars:

| Pillar | Purpose | Retention | Drop policy |
|--------|---------|-----------|-------------|
| **Tracing** | per-request span trees for latency & causality | hours–days | best-effort |
| **Metrics** | aggregated counters/gauges/histograms | days–weeks | best-effort |
| **Structured logs** | discrete events with context | days–weeks | best-effort |
| **Audit** | tamper-evident, hash-chained record of state-changing & security events | months–years | **NEVER drop; ALWAYS detect tampering** |

First three: ops telemetry, disposable. Fourth — **Audit** — is the permanent forensic record. Rides on WAL + KMS audit log + admin/security events + **every Trinity event**, hash-chained and optionally signed.

---

## §2 — Component Overview

```
┌──────────────────────────────────────────────────────────────────┐
│                  OBSERVABILITY & AUDIT PLANE                     │
│                                                                  │
│  ┌──────────────────┐  ┌──────────────────┐  ┌─────────────────┐ │
│  │ TracingSubsystem │  │ MetricsSubsystem │  │ LogSubsystem    │ │
│  │  span tree       │  │  counters        │  │  JSON envelope  │ │
│  │  context prop    │  │  gauges          │  │  levels         │ │
│  │  sampling        │  │  histograms      │  │  sampling       │ │
│  └──────┬───────────┘  └──────┬───────────┘  └────────┬────────┘ │
│         │                     │                       │          │
│  ┌──────▼─────────────────────▼───────────────────────▼───────┐  │
│  │                  RedactionPipeline                         │  │
│  │  per-namespace rules · field allowlist · regex set         │  │
│  └────────────────────────────────────────────────────────────┘  │
│                                                                  │
│  ┌────────────────────────────────────────────────────────────┐  │
│  │              AuditSubsystem (HASH-CHAINED)                 │  │
│  │  Trinity events · WAL framing · KMS · admin/security       │  │
│  │  NEVER dropped, ALWAYS detected if tampered                │  │
│  └────────────────────────────────────────────────────────────┘  │
│                                                                  │
│  ┌────────────────────────────────────────────────────────────┐  │
│  │             ExporterMatrix (OTLP)                          │  │
│  │  traces → OTLP/gRPC · metrics → OTLP · logs → JSON sidecar │  │
│  └────────────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────────────┘
```

---

## §3 — Tracing

### §3.1 Span Model

OTel-compatible. Every Envelope opens a root span at the Router; child spans for:
- capability verification,
- pre-validation hook chain (one child per Trinity hook),
- shard dispatch,
- `on_query` / `on_update`,
- persistence flush,
- view assembly.

### §3.2 Context Propagation

`request_id`, `task_id`, `agent_id`, `gap_ref`, `severity` propagated as baggage.

### §3.3 Sampling

- Head-based default: 1% fully sampled.
- Always-sampled tags: severity == P0, any error, any Trinity event.
- Tail-based: any errored child → whole trace retained.

### §3.4 Export

OTLP/HTTP JSON to a configurable collector. The current checkout defaults to
`http://127.0.0.1:4318/v1/metrics`, `/v1/traces`, and `/v1/logs`, with
`[observability]`, `UC_OTLP_*`, and `--set observability.*` overrides. Export is
explicit and best-effort through `ultracortex metrics export`; the Router never
performs collector network I/O on a metric increment.

---

## §4 — Metrics

| Metric | Type | Labels |
|--------|------|--------|
| `ultracortex.requests.total` | counter | intent, severity, code |
| `ultracortex.request.duration_us` | histogram | intent, tier |
| `ultracortex.tokens.injected_per_step` | histogram | namespace |
| `ultracortex.prefix_cache.hit_rate` | gauge | namespace |
| `ultracortex.trinity.prevalidation_duration_us` | histogram | hook |
| `ultracortex.trinity.quarantine.depth` | gauge | cause |
| `ultracortex.trinity.gap.fixation_triggers` | counter | gap_id |
| `ultracortex.trinity.budget.exceeded` | counter | namespace |
| `ultracortex.wal.fsync_duration_us` | histogram | shard |
| `ultracortex.snapshot.pause_window_us` | histogram | cell_id |
| `ultracortex.rate_limited.total` | counter | namespace |
| `ultracortex.audit.chain_length` | gauge | — |

DeepSeek-specific:

| Metric | Type | Labels |
|--------|------|--------|
| `ultracortex.deepseek.fim_emitted.total` | counter | agent_id |
| `ultracortex.deepseek.r1_reasoning_stripped.total` | counter | agent_id |
| `ultracortex.deepseek.view_bytes` | histogram | view_id |

---

## §5 — Audit Subsystem (Hash-Chained, Tamper-Evident)

### §5.1 Audit Record

```rust
struct AuditRecord {
    seq:         u64,
    prev_hash:   [u8; 32],
    record_hash: [u8; 32],
    event_kind:  AuditEventKind,
    payload:     CanonicalCbor,
    logical_at:  u64,
    signing:     Option<Ed25519Sig>,    // KMS-signed (T2+)
}
```

### §5.2 Event Kinds (MUST be audited)

**State-change:**
- `wal.frame_committed`, `snapshot.taken`, `cas.blob_written`

**Trinity (always):**
- `decision.applied`, `decision.conflict`, `decision.superseded`
- `anchor.created`, `anchor.orphaned`
- `gap.transition`, `task.no_progress`
- `task.budget.exceeded`, `task.quarantined`
- `congruence.delta`, `congruence.delta_accepted`
- `contract.registered`, `contract.deprecated`

**Security/admin:**
- `cap_token.issued`, `cap_token.revoked`
- `kms.key_rotated`, `admin.config_changed`
- `bootstrap.recovery_complete`

### §5.3 Hash Chain Invariants

- **A1** — `record_hash[N+1].prev_hash == record_hash[N]`.
- **A2** — `seq` monotonic; gaps detected on replay.
- **A3** — Records MUST NOT be mutated post-write.
- **A4** — Records MUST NOT be dropped. Disk full / KMS down → Router applies **hard backpressure**; next state-changing request returns `Internal` and is quarantined.

### §5.4 Signing

Current checkout: T2/T3 CrossCheck batches carry key-id HMAC evidence, T3 custody state persists in `kms/keyring.cbor`, `ultracortex kms rotate` emits `kms.key_rotated`, and batch-signature metadata persists in `wal/cross_check/batch-signatures.cbor`.

### §5.5 Replay Verification

On boot, AuditSubsystem replays chain from known-good root and verifies:
1. all hashes link,
2. every completed CrossCheck batch signature still matches its persisted key id and digest,
3. `seq` unbroken.

Failure → `bootstrap.audit_chain_invalid` → node refuses MCP open until human review.

---

## §6 — Redaction Pipeline

Before any export to tracing/metrics/logs (Audit NEVER redacted — forensic source of truth):

- per-namespace **field allowlist**,
- per-namespace **regex set** → `<redacted:%name%>`,
- **secret detection** (API keys, JWTs) unconditionally redacted.

---

## §7 — Structured Logs

JSON envelope, one event per line, stdout (+ optional rotating file).

```json
{
  "ts": "2026-06-10T18:00:00Z",
  "logical_at": 184739283,
  "level": "info",
  "event": "task.budget.exceeded",
  "request_id": "01H...",
  "task_id": "T...",
  "agent_id": "A...",
  "gap_ref": "GAP-NT-005",
  "namespace": "default",
  "details": { ... }
}
```

Levels: `error`, `warn`, `info`, `debug`, `trace`.

---

## §8 — SLOs

| SLO | Target | Window |
|-----|--------|--------|
| Audit record write latency | p99 ≤ 1 ms | 5 min |
| Audit chain integrity verifications passed | 100% | rolling |
| Audit drop rate | 0 | rolling |
| Trace export success | ≥ 99% | 5 min |
| Metric scrape latency | p99 ≤ 10 ms | 5 min |

---

## §9 — Conformance Tests

Every release MUST pass:

1. **Audit chain integrity** under 1M synthetic events.
2. **Tamper detection** — flip one bit → boot refuses MCP open.
3. **No-drop guarantee** — disk-full → backpressure, not drop.
4. **Trinity coverage** — every Trinity event kind has audited test path.

---

## §10 — GAPs

| ID | Description |
|----|-------------|
| GAP-013    | OTel exporter default endpoints — closed by `OtlpConfig`/`OtlpExporter` and the loopback smoke test |

---

## §11 — Congruence Contract

Congruent with: Architecture.md (§11, §15), PersistenceLayer.md (WAL framing, KMS), RouterScheduler.md (backpressure on audit failure), NATIVE_TRINITY.md (event taxonomy), DeepSeekOptimization.md (DeepSeek-specific metrics).

_End of ObservabilityAudit.md v1.0 (UltraCortex)._


---

# 🆙 UltraCortex v1.0 Delta — Curator Metrics, Audit Events, Rationale-Denied Invariant

The HyperCortex four-pillar observability content above remains normative. UltraCortex v1.0 adds Curator-pair metrics, audit events, and a critical invariant.

## §A.1 New Curator Metrics

| Metric | Type | Labels | Source |
|---|---|---|---|
| `ultracortex.curator.agreement_rate` | gauge | pair=(librarian,warden) | rolling 7-day from CrossCheckLedger |
| `ultracortex.curator.suspicious_agreement` | counter | pair | >99% agreement event |
| `ultracortex.curator.calibration_drift_detected` | counter | cell, band | CURATOR_PAIR_PROTOCOL.md §5.4 |
| `ultracortex.curator.probe_pass_rate` | gauge | cell | adversarial probe pass-rate |
| `ultracortex.curator.probe_failed` | counter | cell, probe_kind | failed probes |
| `ultracortex.curator.rationale_access_denied` | counter | requester, target | **MUST be non-zero** (P19 active) |
| `ultracortex.curator.blind_reaudit_disagreement` | counter | cell | blind re-audit found disagreement |
| `ultracortex.adjudicator.invoked` | counter | resolution=det\|llm\|human | adjudication events |
| `ultracortex.adjudicator.deterministic_resolved_pct` | gauge | — | target ≥ 70% |
| `ultracortex.curator.degraded_mode` | gauge | cell | 1 if degraded, 0 normal |
| `ultracortex.librarian.inference_duration_ms` | histogram | — | per-call latency |
| `ultracortex.warden.judgment_duration_ms` | histogram | — | per-call latency |

## §A.2 New Audit Events (MUST be hash-chained, never dropped)

- `librarian.output_emitted`
- `warden.judgment_emitted`
- `warden.audit_disagreement` (with Librarian)
- `librarian.sanity_check_disagreement` (with Warden)
- `adjudicator.invoked`
- `adjudicator.resolution`
- `curator.suspicious_agreement`
- `curator.calibration_drift_detected`
- `curator.probe_failed`
- `curator.rationale_access_denied`
- `cross_check.record_appended`

All Trinity audit events remain present. Hash chain replays deterministically with WAL.

## §A.3 CRITICAL INVARIANT — Rationale-Access-Denied Non-Zero

> **`ultracortex.curator.rationale_access_denied` MUST be non-zero during normal operation.**

This metric counts legitimate cross-check attempts where the auditor's capability token correctly excluded the producer's rationale (P19 enforcement). If this metric drops to zero over a rolling window, it means either:
1. No audits are running (no cross-checks at all), OR
2. The negation-glob enforcement has been disabled (silent guardrail failure).

Either case → **HALT FAILS** until investigated. This is the live "is the guardrail actually on?" check.

## §A.4 Redaction Pipeline (Updated)

Curator PRIVATE blobs (rationale_handle, considered_alts, reasoning_trace, confidence_precise) are **automatically excluded from tracing/metrics/logs exports** to prevent accidental leakage through observability channels. Audit records, however, remain unredacted as the forensic source of truth.

## §A.5 New SLOs

| SLO | Target | Window |
|---|---|---|
| `curator.suspicious_agreement` → operator alert latency | ≤ 5 min wall | per event |
| `curator.rationale_access_denied` = 0 | **NEVER** | rolling 1 h |
| `adjudicator.deterministic_resolved_pct` | ≥ 70% | rolling 7-day |

## §A.6 New Conformance Tests

1. **Curator metric emission** — every Curator inference produces a histogram entry.
2. **Rationale-denied non-zero under load** — 1k synthetic envelope load → metric fires ≥1× per audited write.
3. **Audit chain covers Curator events** — every Curator event kind has at least one audited test path.
4. **Suspicious-agreement detection** — synthetic 100/100 Librarian-Warden agreement → `curator.suspicious_agreement` audit event fires + escalation rate inflates.

## §A.7 New GAPs (Observability-scoped)

| ID | Description |
|---|---|
| GAP-CU-004 | Disagreement quota bounds (default 92–97%) |

## §A.8 Congruence Contract (Updated)

Congruent with: Architecture-UltraCortex.md (§11, §16–§18), **CURATOR_PAIR_PROTOCOL.md** (§7 metrics, §8 conformance tests), **LibrarianCell.md**, **WardenCell.md**, **AdjudicatorCell.md**, **CrossCheckLedgerCell.md** (metric derivation), NATIVE_TRINITY.md (event taxonomy).

_End of UltraCortex v1.0 Delta._
