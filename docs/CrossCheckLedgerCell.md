# CrossCheckLedgerCell.md — **UltraCortex** Forensic Cross-Check Ledger

**Status:** v1.0 Normative | **Owner:** Dominic Sarria-Wiley | **Phase:** 1G

## §0 Conventions
RFC 2119. SPEC-DERIVED-§N.N. [GAP-CU-NNN]. Subject to Trinity governance per P20.

## §1 Mission
Append-only, WAL-backed, KMS-signed forensic record of **every cross-check between in-substrate LLMs**. The ledger is the substrate that makes collusion detectable as a **pattern**, even when no individual cross-check looks suspicious.

## §2 Schema

```rust
pub struct CrossCheckRecord {
    record_id:           Ulid,
    initiator:           CellId,          // who produced the original output
    auditor:             CellId,          // who reviewed it
    audit_kind:          AuditKind,       // RoutineAudit | ReverseCheck |
                                          // BlindReaudit | AdversarialProbe |
                                          // AdjudicatorResolution
    initiator_output:    BlobHandle,      // PUBLIC fields only
    auditor_judgment:    Judgment,        // Agree | DisagreeSpecific | Uncertain
    auditor_grounding:   Vec<Handle>,     // substrate handles independently cited
    independent_handle:  bool,            // CURATOR_PAIR_PROTOCOL.md §3.3
    outcome:             Outcome,         // Applied | Quarantined |
                                          // EscalatedToAdjudicator |
                                          // EscalatedToHuman
    adjudicator_id:      Option<CellId>,
    resolution_handle:   Option<DecisionId>,
    logical_at:          u64,
}
```

## §3 Invariants
- **L1** — Append-only. No in-place mutation.
- **L2** — WAL-backed; durable.
- **L3** — KMS-signed at encryption tier T2+ (Ed25519, every 256-record batch).
- **L4** — Records cite Decisions via `resolution_handle` rather than embedding decisions.
- **L5** — Sequence number monotonic per record; gaps detected on replay.

## §4 Indices
| Index | Use |
|---|---|
| `by_initiator: BTreeMap<CellId, BTreeSet<Ulid>>` | "show me everything Warden audited from Librarian" |
| `by_auditor: BTreeMap<CellId, BTreeSet<Ulid>>` | "show me all of Librarian's sanity-checks of Warden" |
| `by_outcome: BTreeMap<Outcome, BTreeSet<Ulid>>` | "all Quarantined this week" |
| `by_adjudicator: BTreeMap<CellId, BTreeSet<Ulid>>` | "all adjudications by Phi-3.5" |
| `by_audit_kind: BTreeMap<AuditKind, BTreeSet<Ulid>>` | "all BlindReaudits" |

## §5 Derived Metrics
- **Agreement rate** per (initiator, auditor) pair, rolling 7-day window from CrossCheckLedger reads.
- **Suspicious-agreement signal** — >99% agreement → `curator.suspicious_agreement` audit event + auto-inflated escalation rate.
- **Escalation rate** per operation type — anomalies surface miscalibration.
- **Calibration drift** — empirical accuracy per confidence band; tracked over rolling windows.
- **Blind-reaudit disagreement rate** — direct drift signal.

## §6 Query Patterns
```
recall(initiator=Librarian, audit_kind=RoutineAudit, since=T0)
recall(outcome=EscalatedToAdjudicator, last=24h)
recall(adjudicator_id=Phi-3.5, since=T0)
```

All queries are read-only; the ledger is append-only.

## §7 Retention Policy
Audit-grade. Months-to-years. KMS-signed batches make retention forensically robust. **[GAP-CU-009]** retention horizon.

## §8 Mandatory Disagreement Quota Detection
Per CURATOR_PAIR_PROTOCOL.md §5.5:
- Agreement rate >99% over rolling 7-day window → `curator.suspicious_agreement` event.
- Auto-inflated escalation rate (more cases → AdjudicatorCell LLM pool) until rate normalizes.
- Agreement rate <92% → `curator.miscalibration_suspected` event (one Cell likely drifted).

## §9 PreValidator Behavior
Append-only — no PreValidator. Pre-validated by ContractCell schema check + DecisionLedger conflict check only. Cannot quarantine an audit record (would defeat forensic purpose).

## §10 Performance Targets
| Op | p50 | p99 |
|---|---|---|
| Append record | <50 μs | <200 μs |
| Query by index | <10 μs | <100 μs |
| Rolling-window metric compute | <5 ms | <20 ms |
| WAL fsync (group-committed) | <500 μs | <2 ms |

## §11 GAPs
| ID | Description |
|---|---|
| GAP-CU-009 | Retention horizon |
| GAP-NT-012 | Audit signing key custody (inherited) |

## §12 Congruence Contract
Congruent with: Architecture.md (§18), CURATOR_PAIR_PROTOCOL.md (§6), LibrarianCell.md, WardenCell.md, AdjudicatorCell.md, PersistenceLayer.md (own WAL stream), ObservabilityAudit.md (metrics derivation).

_End of CrossCheckLedgerCell.md v1.0._
