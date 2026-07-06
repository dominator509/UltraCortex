# UltraCortex v1.0 — Bundle Manifest

**Generated:** 2026-06-10
**Project:** UltraCortex — Self-Policing Shared-Memory Substrate for Multi-Agent AI
**Version:** v1.0
**Owner:** Dominic Sarria-Wiley
**Supersedes:** HyperCortex v1.0

This bundle contains the complete v1.0 specification for UltraCortex. Every file is a normative spec (RFC 2119) with SPEC-DERIVED traceability, GAP tracking, and live congruence enforcement via `CongruenceCell`.

## Thirteen-Way Source-of-Truth (SoT-13)

| File | Purpose |
|---|---|
| `Architecture.md` | System structure, principles, GAPs (P19/P20, §16–§18, 25 cells, GAP-CU register) |
| `CellTaxonomy.md` | All 25 Cell types + interaction matrix (+ Curator quartet §§22–25) |
| `RouterScheduler.md` | L2 dispatch + budget + severity + fixation + semantic_check gate |
| `PersistenceLayer.md` | L0 WAL/Snapshots/CAS/Manifest/KMS + PrefixCacheStore + Curator weight pinning |
| `McpProtocol.md` | L3 wire format + DeepSeek extensions + new Trinity/Curator error codes |
| `NATIVE_TRINITY.md` | Anti-drift/freeze/fixation substrate + Curator governance |
| `EmbeddingReranker.md` | Embedding + reranker + hybrid retrieval (GAP-NT-013 closed) |
| `ObservabilityAudit.md` | Four pillars + hash-chained Trinity audit + Curator metrics |
| `BootstrapOperator.md` | Single-binary lifecycle + Curator self-test |
| `DeepSeekOptimization.md` | Prefix-stable views, FIM, R1, function-call grammar (parallel to Curator) |
| `CURATOR_PAIR_PROTOCOL.md` | **NEW** — Three collusion vectors, Asymmetric Visibility, 9 anti-collusion guardrails |
| `LibrarianCell.md` | **NEW** — Memory Archive Librarian (Gemma 2 2B, async, write-side) |
| `WardenCell.md` | **NEW** — Drift/Hallucination Warden (Qwen 2.5 Coder 1.5B, opt-in sync, read-side) |

## Normative Companions

| File | Purpose |
|---|---|
| `AdjudicatorCell.md` | Curator pair tie-breaker (deterministic + rotating LLM pool + human escalation) |
| `CrossCheckLedgerCell.md` | Append-only forensic ledger of every cross-check |

## Supporting Documents

| File | Purpose |
|---|---|
| `Roadmap.md` | Sequenced build plan, Phase 0 → Phase 1G |
| `HANDOFF.md` | Live GAP register (carryover + Trinity + DeepSeek + Curator) |
| `RECONCILE.md` | Mapping prior designs → native realization |
| `CONGRUENCE.md` | The 13-way source-of-truth contract |
| `README.md` | Project overview + comparison table |
| `SYSTEM_REQUIREMENTS.md` | Three deployment tiers with Curator deltas |

## Key Differentiators vs HyperCortex v1.0

| Capability | HyperCortex v1.0 | UltraCortex v1.0 |
|---|---|---|
| Cell types | 21 | **25** (Librarian, Warden, Adjudicator, CrossCheckLedger) |
| Anti-drift | substrate (write-time, structural) | substrate + **semantic** (Warden judges intent) |
| Anti-hallucination | n/a | **substrate-level** (Warden flags hallucinated handles) |
| In-substrate LLMs | none | **2 specialists + 1 rotating adjudicator pool** |
| LLM oversight | n/a | **mutual accountability + Trinity governance** |
| LLM collusion risk | n/a | **structurally blocked** (P19 + 9 guardrails) |
| Principles | 18 (P1–P18) | **20** (+ P19 Asymmetric Visibility, P20 Substrate-Policed Semantic Layer) |
| Tokens/step | ≤1.5 KB p50 | ≤1.5 KB p50 (plausibly **≤800 B** with Curator) |
| GAP-NT-013 (SummarizerCell) | open | **CLOSED** (LibrarianCell) |
| New GAP namespace | n/a | **GAP-CU-NNN** (14 entries) |

## Bundle Composition (22 files)

This complete bundle = the 10 inherited HyperCortex specs (patched with UltraCortex v1.0 deltas) + the 6 novel Curator-pair specs + 6 supporting docs.

_End of MANIFEST.md_
