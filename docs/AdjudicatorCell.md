# AdjudicatorCell.md — **UltraCortex** Curator Pair Tie-Breaker

**Status:** v1.0 Normative | **Owner:** Dominic Sarria-Wiley | **Phase:** 1G

## §0 Conventions
RFC 2119. SPEC-DERIVED-§N.N. [GAP-CU-NNN]. Subject to Trinity governance per P20.

## §1 Mission
When LibrarianCell and WardenCell disagree, the AdjudicatorCell tie-breaks via a **two-stage policy**:
1. **Deterministic adjudication** first (~70-80% of cases, no LLM call).
2. **Rotating LLM pool** for genuinely ambiguous cases (~20-30%).
3. **Human escalation** as last resort (~1-2%).

The Adjudicator's goal is to **resolve disagreements with the smallest possible failure surface**, with deterministic substrate rules taking priority over any model judgment.

## §2 Deterministic Adjudication (default; ~70-80%)
Policy table — no LLM. Latency ≤50 μs p50, ≤200 μs p99.

| Disagreement type | Deterministic resolution |
|---|---|
| Librarian proposes supersede X→Y; Warden says X canonical | Check DecisionLedger for active Decision binding X. Active → reject supersede. None → escalate to LLM pool. |
| Warden flags envelope; Librarian says grounding insufficient | Check SpecAnchor coverage of Warden's cited grounding. ≥80% → uphold flag. <80% → escalate. |
| Both report confidence < threshold | Always escalate. |
| Identical PUBLIC outputs but conflicting verdicts | Quarantine envelope with both PUBLIC rationales attached; escalate to LLM pool. |
| One side reports `Uncertain` | Escalate to LLM pool. |
| Disagreement on a node lacking SpecAnchor coverage | Escalate to human (substrate cannot ground). |

## §3 Rotating LLM Pool (~20-30%)
For cases unresolvable deterministically.

| Model | Params | Q4 RAM | Provenance |
|---|---|---|---|
| Phi-3.5 Mini | 3.8B | ~2.4 GiB | Microsoft |
| Llama 3.2 3B | 3B | ~2.0 GiB | Meta |
| SmolLM-2 1.7B | 1.7B | ~1.3 GiB | HuggingFace |

**Rotation seeded by `envelope.seed`** → deterministic per case. The Adjudicator sees:
- ✅ Original envelope (PUBLIC).
- ✅ Librarian's PUBLIC output.
- ✅ Warden's PUBLIC judgment.
- ✅ Substrate slice both saw.
- ❌ **CrossCheckLedger prior decisions** (prevents rationale anchoring on past resolutions).
- ❌ Either Cell's PRIVATE fields.

Output recorded in CrossCheckLedger with `adjudicator_id` populated.

**[GAP-CU-007]** pool composition. **[GAP-CU-008]** rotation policy.

## §4 Human Escalation (~1-2%)
Survives both deterministic + LLM adjudication → AgentRegistry escalation list. Substrate filters firehose down to cases that genuinely need human attention. **[GAP-CU-014]**.

## §5 State

```rust
pub struct AdjudicatorState {
    deterministic_policy:  PolicyTable,           // compiled Rust rules
    llm_pool:              Vec<ModelHandle>,      // pinned by SHA-256 each
    rotation_index:        BTreeMap<Seed, usize>, // deterministic rotation
    pending:               HashMap<Ulid, AdjudicationJob>,
}
```

## §6 Invocation
Called by Router when:
- LibrarianCell's sanity-check disagrees with WardenCell.
- WardenCell's audit disagrees with LibrarianCell.
- Either Cell reports `Uncertain`.
- Confidence calibration drift triggers degraded mode auto-escalation.

## §7 Output

```rust
pub enum AdjudicationResolution {
    UpholdLibrarian,
    UpholdWarden,
    OverrideBoth { reason_anchor: AnchorRef },
    EscalateToHuman,
}
```

Resolution recorded as a Decision in DecisionLedgerCell + CrossCheckRecord in CrossCheckLedgerCell.

## §8 Determinism
- Deterministic policy: pure Rust, reproducible.
- LLM pool: `temperature = 0`, greedy decode, seed = `envelope.seed`, pinned weights.
- Rotation: deterministic via `envelope.seed`.

## §9 Anti-Collusion Protections
- **No prior-decision visibility** — Adjudicator capability scope excludes `cross_check_ledger/*` reads. Prevents rationale anchoring on past resolutions.
- **Rotation across providers** — three orthogonal training lineages prevent stable wrong-consensus.
- **No PRIVATE field visibility** — only Librarian + Warden PUBLIC outputs.
- **Subject to Trinity** — Adjudicator's own outputs pass through pre-validation chain, can be quarantined, can trigger GapCell fixation if it loops.

## §10 Conformance Tests
1. **Deterministic resolution rate ≥ 70%** on a synthetic mixed disagreement corpus.
2. **Rotation determinism** — same `envelope.seed` → same model picked across replays.
3. **No prior-decision leakage** — Adjudicator cap-token reading CrossCheckLedger → `PermissionDenied`.
4. **Trinity governance** — synthetic Adjudicator fixation → `GapCell` detector fires.

## §11 GAPs
| ID | Description |
|---|---|
| GAP-CU-007 | LLM pool composition (3 vs 5 models) |
| GAP-CU-008 | Rotation policy details |
| GAP-CU-014 | Human escalation routing |

## §12 Congruence Contract
Congruent with: Architecture.md (§17), CURATOR_PAIR_PROTOCOL.md, LibrarianCell.md, WardenCell.md, CrossCheckLedgerCell.md, RouterScheduler.md (escalation dispatch).

_End of AdjudicatorCell.md v1.0._
