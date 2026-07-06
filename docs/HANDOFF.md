# HANDOFF.md — UltraCortex v1.0 Live GAP Register

**Status:** v1.0 | Live tracking | Update cadence: every HALT

## §0 Conventions
Four GAP namespaces: `GAP-NNN` (carryover), `GAP-NT-NNN` (Trinity), `GAP-DS-NNN` (DeepSeek), `GAP-CU-NNN` (Curator pair — NEW v1.0). Status: open / in_progress / blocked / resolved / quarantined / deferred / closed.

## §1 Carryover GAPs
GAP-001 (rebalancing, deferred 2+) · GAP-002 (allocator, open 0) · GAP-003 (WASM, deferred 2+) · GAP-004 (retrieval weights, open 1B) · GAP-005 (cross-shard tx, deferred) · GAP-006 (TLS defaults, open 1C) · GAP-009 (T3 rotation, open 1E) · GAP-010 (token-eff bench, open 1B/1F) · GAP-011 (quorum, open 1D) · GAP-012 (snapshot pause, open 1E) · GAP-013 (OTel endpoints, open 1F) · GAP-014 (federation, deferred 2+)

## §2 Native Trinity GAPs
GAP-NT-001..014 unchanged from HyperCortex v1.0 **EXCEPT GAP-NT-013 CLOSED** (subsumed by LibrarianCell in UltraCortex v1.0).

## §3 DeepSeek GAPs
GAP-DS-001..004 unchanged from HyperCortex v1.0.

## §4 Curator Pair GAPs (NEW v1.0)

| ID | Description | Phase | Status |
|---|---|---|---|
| GAP-CU-001 | Librarian default model (Gemma 2 2B vs Gemma 3) | 1G | open |
| GAP-CU-002 | Warden default model (Qwen 2.5 Coder 1.5B vs alternates) | 1G | open |
| GAP-CU-003 | Confidence-band threshold defaults | 1G | open |
| GAP-CU-004 | Disagreement quota bounds (default 92–97%) | 1G | open |
| GAP-CU-005 | Adversarial probe schedule + corpus | 1G | open |
| GAP-CU-006 | Calibration drift detection window size | 1G | open |
| GAP-CU-007 | Adjudicator LLM pool composition (3 vs 5 models) | 1G | open |
| GAP-CU-008 | Adjudicator rotation policy details | 1G | open |
| GAP-CU-009 | Cross-check ledger retention horizon | 1G | open |
| GAP-CU-010 | Per-Cell KV cache size budget | 1G | open |
| GAP-CU-011 | Blind re-audit sample rate (default 1%) | 1G | open |
| GAP-CU-012 | Capability-token negation glob canonicalization | 1G | open |
| GAP-CU-013 | Curator shard topology for small deployments | 1G | open |
| GAP-CU-014 | Human escalation routing policy | 1G | open |

## §5 Inter-Agent Handoff Protocol
Every write carries `agent_id`. Every Decision references issuing agent. Cross-agent contention detected by DecisionLedgerCell. Resolution via explicit `supersede`. Human-in-the-loop is the only path to resolve true contention.

## §6 SPEC-DERIVED Coverage
After Phase 1A.2 (SpecAnchorCell live): every `SPEC-DERIVED-§N.N` marker in the v1.0 corpus has a live AnchorNode. Coverage auto-reported by `ultracortex contract list --anchors`.

## §7 Congruence Audit Summary
After Phase 1A.3 (CongruenceCell live): every pair (doc_i, doc_j) in the SoT-13 has a matrix entry. Unaccepted deltas empty at HALT-gate evaluation.

## §8 Next Actions
- Assign owners to all `Owner: TBD` rows.
- Specify the multi-step coding agent benchmark for DeepSeek + Curator acceptance.
- Decide Trinity-shard + Curator-shard topology defaults.

_End of HANDOFF.md v1.0 (UltraCortex)._
