# CONGRUENCE.md — UltraCortex v1.0 Congruence Contract

**Status:** v1.0 Normative

## §0 Purpose
Declare the **thirteen-way source-of-truth** for UltraCortex v1.0 and the mechanism by which it is enforced.

## §1 The Thirteen Documents

1. Architecture.md — system structure, principles, GAPs (UltraCortex baseline)
2. CellTaxonomy.md — all 25 Cell types
3. RouterScheduler.md — L2 dispatch + semantic_check gate + Adjudicator path
4. PersistenceLayer.md — L0 byte layout + Curator weight-pinning
5. McpProtocol.md — L3 protocol surface + new error codes
6. NATIVE_TRINITY.md — Trinity Cells + scheduler hooks + Curator governance
7. EmbeddingReranker.md — retrieval workers (GAP-NT-013 closed)
8. ObservabilityAudit.md — observability + audit + Curator metrics
9. BootstrapOperator.md — single-binary lifecycle + Curator self-test
10. DeepSeekOptimization.md — DeepSeek alignment (parallel to Curator pair)
11. **CURATOR_PAIR_PROTOCOL.md (NEW)** — mutual-accountability protocol
12. **LibrarianCell.md (NEW)** — write-side curator
13. **WardenCell.md (NEW)** — read-side judge

Normative companions: AdjudicatorCell.md, CrossCheckLedgerCell.md.

Supporting: Roadmap.md, HANDOFF.md, RECONCILE.md, CONGRUENCE.md (this), README.md, SYSTEM_REQUIREMENTS.md.

## §2 Enforcement Mechanism
Congruence is **live**, not external:
1. **SpecAnchorCell** holds every `SPEC-DERIVED-§N.N` marker.
2. **CongruenceCell** holds a live matrix of entity sets across the thirteen docs. Symdiff deltas without `accepted_deltas` block the next HALT.
3. **ContractCell** holds every interface/schema (including Curator weight-file SHA-256 pins).
4. **DecisionLedgerCell** records every accepted delta and migration.
5. **(NEW)** **CrossCheckLedgerCell** records every Curator cross-check for forensic congruence.

A HALT cannot pass unless all five Cells report green.

## §3 Process
For any change touching the thirteen docs:
1. Author identifies affected sections.
2. Author updates all affected docs in the same change set.
3. Runs `ultracortex congruence audit`.
4. Unaccepted deltas → revise OR record a Decision accepting.
5. HALT proceeds only when audit clean.

## §4 Failure Modes

| Failure | Detection | Remedy |
|---|---|---|
| Doc edit without code/test update | `anchor.stale` | update artifact or remove anchor |
| Code/test edit without doc update | `anchor.orphaned` | update doc or remove artifact |
| Entity drift across docs | `congruence.delta` | revise or accept |
| Schema change without migration | `contract.deprecated` without plan | author plan + Decision |
| Decision drift | `decision.conflict` | supersede explicitly |
| **(NEW)** Curator model swap without Decision | ContractCell SHA-256 mismatch | author Decision recording the swap |
| **(NEW)** Curator collusion signal | `curator.suspicious_agreement` | auto-inflated escalation; reviewer required |

## §5 Versioning
v1.0 in force. Changes require new CONGRUENCE.md version + Decision linking versions + accepted_deltas covering affected docs.

_End of CONGRUENCE.md v1.0 (UltraCortex)._
