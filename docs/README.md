# UltraCortex

**Self-policing shared-memory substrate for multi-agent AI. Single Rust binary. Microsecond recall. Deterministic replay. Structurally impossible drift, freeze, fixation — AND structurally impossible semantic drift and hallucination, including by in-substrate LLMs.**

---

## What it is

UltraCortex is the v1.0 evolution of HyperCortex. It collapses what used to be **six** glued-together systems — vector DB, KV store, blob store, audit log, agent registry, AND drift-detection CI — into **one Rust binary** delivering:

- microsecond recall
- native-speed writes
- deterministic replay
- first-class DeepSeek optimization (≥80% prefix-cache hit, ≤1.5 KB tokens/step at p50, plausibly ≤800 B with Curator)
- a uniform MCP-style protocol any agent runtime can consume
- **Native Trinity** anti-failure substrate
- **(NEW v1.0)** **Curator Pair + Adjudicator** — a mutually-accountable semantic layer using small open-source LLMs (Gemma 2 2B + Qwen 2.5 Coder 1.5B + rotating Phi/Llama/SmolLM adjudicator pool)

## What's new in UltraCortex vs HyperCortex

| Capability | HyperCortex v1.0 | **UltraCortex v1.0** |
|---|---|---|
| Cells | 21 | **25** (+ 4 Curator Cells) |
| Anti-drift | substrate (write-time) | substrate + **semantic** (Warden judges) |
| Anti-hallucination | not addressed | **substrate** (Warden flags hallucinated handles) |
| In-substrate LLMs | none | **Librarian (Gemma 2 2B) + Warden (Qwen 2.5 Coder 1.5B)** |
| LLM collusion risk | n/a | **structurally blocked** (P19 Asymmetric Visibility + 9 anti-collusion guardrails) |
| LLM oversight | n/a | **mutual accountability** (Librarian audits Warden, Warden audits Librarian, Adjudicator tie-breaks) |
| GAP-NT-013 (SummarizerCell) | open | **CLOSED** (subsumed by LibrarianCell) |
| Tokens injected per step | ≤1.5 KB p50 | ≤1.5 KB p50 (plausibly **≤800 B** with Curator) |

## Who it's for

Anyone building production multi-agent systems who is tired of:
- vector DBs that serialize on every access
- memory layers that inject the same 7 KB pack on every call
- CI scripts trying to enforce spec/code congruence after the fact
- agents stuck in silent retry loops
- LLMs hallucinating handles, supersessions, or facts with no oversight

## Quick start (after v1.0 ship)

```sh
$ ultracortex --config ultracortex.toml
ready node_id=node-01 proto_version=1 cells=25
```

```sh
$ ultracortex curator status       # agreement rate, calibration, degraded mode
$ ultracortex cross-check tail     # recent CrossCheckRecords
$ ultracortex adjudicator stats    # det vs llm vs human split
$ ultracortex congruence audit     # check the 13 docs in sync
```

## Document map

| Tier | Document | What it specifies |
|---|---|---|
| **SoT-13** | Architecture.md | System structure (§1–§22). Start here. |
| **SoT-13** | CellTaxonomy.md | All 25 Cell types. |
| **SoT-13** | RouterScheduler.md | L2 dispatch + budget + severity + fixation + **semantic_check gate**. |
| **SoT-13** | PersistenceLayer.md | L0 WAL/snapshots/CAS/Manifest/KMS/PrefixCacheStore + **Curator weight pinning**. |
| **SoT-13** | McpProtocol.md | L3 wire format + **new error codes**. |
| **SoT-13** | NATIVE_TRINITY.md | Anti-drift/freeze/fixation substrate + **governs Curators**. |
| **SoT-13** | EmbeddingReranker.md | Embedding + reranker + hybrid retrieval. |
| **SoT-13** | ObservabilityAudit.md | Four pillars + **Curator metrics**. |
| **SoT-13** | BootstrapOperator.md | Single-binary lifecycle + **Curator self-test**. |
| **SoT-13** | DeepSeekOptimization.md | All DeepSeek-specific tuning. |
| **SoT-13** | **CURATOR_PAIR_PROTOCOL.md** | **(NEW)** Mutual-accountability protocol. |
| **SoT-13** | **LibrarianCell.md** | **(NEW)** Write-side curator. |
| **SoT-13** | **WardenCell.md** | **(NEW)** Read-side judge. |
| Normative | AdjudicatorCell.md | Tie-breaker. |
| Normative | CrossCheckLedgerCell.md | Forensic ledger. |
| Support | Roadmap.md | Phase 0 → Phase 1G. |
| Support | HANDOFF.md | Live GAP register (now incl. GAP-CU). |
| Support | RECONCILE.md | Mapping from prior designs. |
| Support | CONGRUENCE.md | The 13-way source-of-truth contract. |
| Support | SYSTEM_REQUIREMENTS.md | Hardware specs (3 tiers). |
| Support | MANIFEST.md | Bundle index. |

## Status

v1.0 specification complete. The current checkout compiles and passes `cargo test` locally, and the second-pass in-scope gap checklist is closed. The audited register lives in `HANDOFF.md`; GAP-001 through GAP-005 and GAP-014 remain intentionally deferred product-scope work. Production Curator boot still requires the operator-owned `llama-cli` runner and the two pinned GGUF files, with no software credential required.

## License

TBD.

---

_UltraCortex — built so the memory layer never gets in the way, and the LLMs inside it keep each other honest._
