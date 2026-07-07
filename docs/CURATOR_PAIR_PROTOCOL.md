# CURATOR_PAIR_PROTOCOL.md — **UltraCortex** Curator Pair Mutual-Accountability Protocol

**Status:** v1.0 Normative Protocol Binding LibrarianCell, WardenCell, AdjudicatorCell
**Owner:** Dominic Sarria-Wiley

## §0 Conventions
RFC 2119. SPEC-DERIVED-§N.N. [GAP-CU-NNN] track open items. All times are logical clocks. All in-substrate LLMs subject to Trinity governance (P20).

## §1 Mission
Bind three Cells — `LibrarianCell`, `WardenCell`, `AdjudicatorCell` — into a self-policing semantic layer where:
1. No in-substrate LLM has a privileged path.
2. No in-substrate LLM is trusted unilaterally.
3. No collusion between LLMs is structurally possible.
4. Every cross-check is forensic (CrossCheckLedger).

**Non-optional.** No supported configuration disables it.

## §2 The Three Collusion Vectors

| Vector | Shared | Risk | Response |
|---|---|---|---|
| **V1 — Outputs** | Skeletons, supersession proposals, flags, judgments, grounding handles, confidence bands | **Low** — substrate facts, observable | **MUST remain mutually visible.** Blocking V1 destroys accountability. |
| **V2 — Working state** | KV caches, activations, scratchpad reasoning | **Medium** — could leak bias | **MUST be strictly segregated.** Enforced structurally by share-nothing (P4). |
| **V3 — Rationales** | Chain-of-thought, considered alternatives, precise confidence, narrative justification | **HIGH** — rationale-anchoring collapses cross-checks into rubber-stamping | **MUST be private to producer.** Enforced by Asymmetric-Visibility Invariant (§3) + capability-token negation glob (§4). |

**Central design move:** keep V1 fully visible, block V3 entirely, rely on share-nothing for V2.

## §3 The Asymmetric-Visibility Invariant (NORMATIVE)

> A Cell auditing another Cell MUST see substrate effects but MUST NOT see the producing Cell's rationale, chain-of-thought, considered alternatives, or precise confidence.

### §3.1 PUBLIC vs PRIVATE schema

```rust
struct CuratorOutput {
    // ───── PUBLIC ─────
    operation:         Operation,
    target_handle:     Handle,
    grounded_in:       Vec<Handle>,      // MUST exist in FactCell/DecisionLedger
                                         // BEFORE the audit
    confidence_band:   ConfidenceBand,   // Low | Med | High (coarse)
    schema_id:         SchemaId,
    spec_anchor:       AnchorRef,
    logical_at:        u64,

    // ───── PRIVATE ─────
    rationale_handle:  BlobHandle,
    considered_alts:   BlobHandle,
    confidence_precise: f32,
    reasoning_trace:   BlobHandle,
    private_seed:      u64,
}
```

### §3.2 Why the split
PUBLIC fields = everything an auditor needs (what, where in substrate, cited grounding, coarse confidence). PRIVATE fields = collusion bandwidth (persuasive rationale, precise confidence as covert signal, CoT that frames auditor's reasoning).

### §3.3 Auditor Independence
When auditor A reviews producer B:
- A reads B's PUBLIC + substrate slice.
- A independently derives `grounded_in` — MUST NOT copy B's grounding.
- A's judgment MUST cite ≥1 substrate handle B did not cite (**independent-handle invariant**) OR explicitly set `independent_grounding_matched: true` with content-hash proof.

Rubber-stamping becomes detectable as a pattern.

## §4 Capability-Token Negation Glob

```
facet_scope: <include_glob> [!<exclude_glob>]*
```

Examples:
```
# Warden sees Librarian outputs but no rationales
"librarian/output/*  !librarian/output/rationale.*  !librarian/output/considered_alts.*  !librarian/output/reasoning_trace.*  !librarian/output/confidence_precise"

# Adjudicator sees both PUBLIC outputs but neither's rationale, AND no CrossCheckLedger prior decisions
"librarian/output/*  warden/judgment/*  !*/rationale.*  !*/reasoning_trace.*  !cross_check_ledger/*"
```

Router rejects `hydrate` requests matching exclude globs → emits `curator.rationale_access_denied`. **This event MUST fire at the rate of legitimate audits.** Dropping to zero = guardrail silently disabled → HALT fails. **[GAP-CU-012]** canonicalization.

## §5 Nine Anti-Collusion Guardrails

### §5.1 Asymmetric Visibility (P19)
Per §3. The foundation.

### §5.2 Different Training Lineages
| Role | Default | Provenance | Emphasis |
|---|---|---|---|
| Librarian | Gemma 2 2B | Google DeepMind | General + summarization |
| Warden | Qwen 2.5 Coder 1.5B | Alibaba | Code + reasoning |
| Adjudicator pool MUST include third provenance | Phi (Microsoft), Llama (Meta), SmolLM (HF) | varied | varied |

Correlated blind spots track training corpus. Orthogonal lineages → consistent-wrong-consensus structurally implausible.

### §5.3 Periodic Adversarial Probes
Substrate injects known-wrong synthetic envelopes on deterministic schedule (seeded by logical clock). v1.0 fixes the schedule at a base probe rate of `0.001`, boosted `x10` when the rolling agreement window enters suspicious-agreement state. The v1.0 probe corpus is the fabricated-handle existence probe: a well-formed but nonexistent `fact/<ulid>` citation that the Warden MUST flag. Probe pass-rate = `ultracortex.curator.probe_pass_rate`. Below threshold → auto-quarantine of Cell + alert.

### §5.4 Confidence Calibration Drift Detection
Track empirical accuracy per band over rolling windows. v1.0 fixes the calibration window at `50` outcomes per band, with degraded mode triggered when High-band accuracy drops below `0.85` or Medium-band accuracy drops below `0.60` once a band has at least 10 observations. Drift beyond tolerance → Cell enters **degraded mode**: sync path disabled, all outputs auto-escalated to Adjudicator. Exits only on fresh re-validation Decision.

### §5.5 Mandatory Disagreement Quota
Healthy = 92–97% agreement. **>99% agreement = collusion signal**, not health.
- >99% → `curator.suspicious_agreement` + auto-inflated escalation rate.
- <92% → also flagged; miscalibration likely.

**[GAP-CU-004]** bounds.

### §5.6 No Shared KV-Cache, No Shared Embeddings, No Shared Shards
Separate mmap'd weights, separate shards, separate KV caches. Tokenizer + embedding caches NOT shared. Only inter-Cell channel = substrate.
Supported v1.0 KV-budget profiles are selected by `[curator].kv_budget_profile` and surfaced by `curator status`:
- `small` = Librarian `256 MiB`, Warden `256 MiB`, Adjudicator `128 MiB`
- `reference` = Librarian `384 MiB`, Warden `384 MiB`, Adjudicator `256 MiB` (default)
- `heavy` = Librarian `768 MiB`, Warden `768 MiB`, Adjudicator `512 MiB`

### §5.7 No Reciprocal Awareness of Timing
Warden MUST NOT see when Librarian wrote outputs or in what order. Capability scope excludes `logical_at` ordering reads on Librarian-produced nodes. Outputs as set, not sequence.

### §5.8 Substrate-Only Grounding
Any handle in auditor's `grounded_in` MUST exist in FactCell/DecisionLedger/Contract **at audit time** AND MUST NOT have been produced by the audited Cell in the same envelope. Enforced by SpecAnchorCell → violations route to QuarantineCell cause=`GroundingViolation`.

### §5.9 Periodic Blind Re-Audit
Deterministic schedule (1% sample, seeded by logical clock). Fresh Warden instance, empty KV. Re-audits past Curator output with substrate-as-of-original-write-time. Disagreement with original = drift caught. `audit_kind: BlindReaudit` in CrossCheckLedger. **[GAP-CU-011]**.

## §6 CrossCheckLedger Schema (Summary)
Full spec in CrossCheckLedgerCell.md.

```rust
struct CrossCheckRecord {
    record_id:           Ulid,
    initiator:           CellId,
    auditor:             CellId,
    audit_kind:          AuditKind,    // RoutineAudit | ReverseCheck |
                                       // BlindReaudit | AdversarialProbe
    initiator_output:    BlobHandle,   // PUBLIC fields only
    auditor_judgment:    Judgment,     // Agree | DisagreeSpecific | Uncertain
    auditor_grounding:   Vec<Handle>,
    independent_handle:  bool,         // §3.3 invariant
    outcome:             Outcome,      // Applied | Quarantined |
                                       // EscalatedToAdjudicator |
                                       // EscalatedToHuman
    adjudicator_id:      Option<CellId>,
    resolution_handle:   Option<DecisionId>,
    logical_at:          u64,
}
```

## §7 Audit Metrics

| Metric | Type |
|---|---|
| `ultracortex.curator.agreement_rate` | gauge |
| `ultracortex.curator.suspicious_agreement` | counter |
| `ultracortex.curator.calibration_drift_detected` | counter |
| `ultracortex.curator.probe_pass_rate` | gauge |
| `ultracortex.curator.probe_failed` | counter |
| `ultracortex.curator.rationale_access_denied` | counter (MUST be non-zero) |
| `ultracortex.curator.blind_reaudit_disagreement` | counter |
| `ultracortex.adjudicator.invoked` | counter |
| `ultracortex.adjudicator.deterministic_resolved_pct` | gauge (target ≥70%) |
| `ultracortex.curator.degraded_mode` | gauge |

## §8 Conformance Tests
Every release MUST pass:
1. **PUBLIC/PRIVATE separation** — Warden cap-token hydrating Librarian rationale → `PermissionDenied` + audit event.
2. **Rationale-access-denied non-zero** — 1k-envelope synthetic load → metric fires ≥1× per audited write.
3. **Independent grounding** — identical handle sets → auditor sets `independent_handle: false` and proves match via hash OR escalates.
4. **Probe pass-rate** — ≥95% on v1.0 probe corpus.
5. **Disagreement quota** — synthetic 100/100 agreement → `curator.suspicious_agreement` fires + escalation rate inflates.
6. **Blind re-audit determinism** — byte-identical judgment under temp=0 + pinned seed.
7. **Calibration drift** — synthetic accuracy drop → drift event + degraded mode.
8. **Adjudicator no-prior-leakage** — Adjudicator cap-token reading CrossCheckLedger → `PermissionDenied`.
9. **Trinity governance** — induce Librarian fixation loop → `GapCell` detector fires + Librarian quarantined.

## §9 GAP-CU Register
GAP-CU-001 … GAP-CU-014 (full list in Architecture.md §22.3).

## §10 Congruence Contract
Congruent with: Architecture.md (P19/P20, §16–§18, §22.4), CellTaxonomy.md (Cells 22–25), RouterScheduler.md (`flags.semantic_check`, Adjudicator path), ObservabilityAudit.md (curator.* metrics), McpProtocol.md (new error codes), NATIVE_TRINITY.md (Trinity governs Curators), LibrarianCell.md, WardenCell.md, AdjudicatorCell.md, CrossCheckLedgerCell.md. Live enforcement by CongruenceCell.

_End of CURATOR_PAIR_PROTOCOL.md v1.0._
