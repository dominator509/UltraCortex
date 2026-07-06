# RECONCILE.md — UltraCortex v1.0 Reconciliation With Prior Designs

**Status:** v1.0 Normative mapping document

## §0 Purpose
Map every external improvement, convention, or prior CI script to its native realization in UltraCortex. Outcome: **zero external scripts, all enforcement native**, including the new semantic layer.

## §1 DRIFT_CONTROL → Native Trinity Cells

| Layer | External | Native |
|---|---|---|
| Spec drift | `anchor_audit.py` | **SpecAnchorCell** |
| Decision drift | `decision_audit.py` | **DecisionLedgerCell** |
| Congruence audit | `congruence_audit.py` | **CongruenceCell** |
| Semantic convention drift | naming lint | **ContractCell** |
| Handoff drift | manual HANDOFF.md | **GapCell** + HANDOFF.md living register |
| Bounded agent authority | scoped tokens | capability tokens + **facet-glob scoping** |

## §2 FLOW_CONTROL → Quarantine + WorkBudget + Severity

| Valve | External | Native |
|---|---|---|
| Quarantine | DLQ + manual review | **QuarantineCell** |
| Token/time budgets | per-task envelope | **WorkBudgetCell** |
| P0/P1/P2 tiering | manual triage | **Severity-aware routing** |
| Forward-progress guarantee | timeout | **Gap-aware loop detection** |

## §3 GAP_PROTOCOL → GapCell
Gap lifecycle states + dispatch counter + fixation detection → native Cell.

## §4 Mem0 Competition Analysis → DeepSeek Optimization Layer

| Lever | Mem0 | UltraCortex |
|---|---|---|
| Context-as-view | retrieved bundle | **`view` verb + canonical prefix-stable layout** |
| Reasoning scaffolding | LLM ranks/summarizes | **Worker offloading** |
| Causal state refs | re-injects text | **Symbolic pointer compression** |
| Token budget | retrieval-side | **Protocol-level WorkBudget** |

## §5 HyperCortex Curator-Pair Proposal → Native Realization (NEW v1.0)

| Original proposal | UltraCortex realization |
|---|---|
| Small LLM as "Memory Archive Librarian" | **LibrarianCell** (Gemma 2 2B, async, write-side) |
| Small LLM as "Drift/Hallucination Warden" | **WardenCell** (Qwen 2.5 Coder 1.5B, opt-in sync, read-side) |
| Different models for each role | Different model families (Gemma vs Qwen-Coder) per CURATOR_PAIR_PROTOCOL.md §5.2 |
| Tie-breaker mechanism | **AdjudicatorCell** (deterministic + rotating pool + human escalation) |
| Mutual accountability | Cross-check protocol + CrossCheckLedgerCell + Asymmetric Visibility (P19) |
| Anti-collusion | Nine structural guardrails (CURATOR_PAIR_PROTOCOL.md §5) |
| Trinity governance over Curators | P20 + §11 hook chain + GapCell fixation + ContractCell pinning |

## §6 Asymmetric Visibility Discovery (NEW)
User insight during design: "What about forbidding shared substrate slices?" → analyzed to: **shared evidence, separate reasoning**. Codified as P19 + Asymmetric-Visibility Invariant. Capability-token negation glob enforces at protocol level.

## §7 Rejected External Proposals
- External `congruence_audit.py` script: rejected (CongruenceCell runs continuously, O(delta)).
- Per-request LLM-driven validation in default chain: rejected (latency tax + nondeterminism).
- **Blanket slice-segregation between Librarian and Warden**: rejected (would destroy accountability — V1 outputs MUST stay visible).
- **Shared rationales for "context"**: rejected (V3 = collusion attack surface; PRIVATE-only).

## §8 Outcome

| Class | Before UltraCortex | UltraCortex v1.0 |
|---|---|---|
| External CI scripts | 4 | **0** |
| Convention-based safeguards | many | **all native** |
| LLM rationale-anchoring collusion risk | unmitigated | **structurally blocked** (P19) |
| Drift detection latency | minutes (CI) | **microseconds (write-time)** + semantic check on-demand |
| Loop prevention | timeout | **substrate-level GapCell** (covers Curator Cells too) |

UltraCortex is the realization of "the protocol is the product" applied to **safety, performance, AND semantic honesty**.

_End of RECONCILE.md v1.0 (UltraCortex)._
