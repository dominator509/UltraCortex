# EmbeddingReranker.md — **UltraCortex** Embedding & Reranker Workers Specification

**Status:** v1.0 — Normative Inference-Worker Specification
**Owner:** Dominic Sarria-Wiley
**Companion documents:** Architecture.md v1.0, CellTaxonomy.md v1.0, RouterScheduler.md v1.0, PersistenceLayer.md v1.0, McpProtocol.md v1.0, NATIVE_TRINITY.md v1.0, DeepSeekOptimization.md v1.0.

---

## §0 — Document Conventions

- **MUST / SHOULD / MAY** follow RFC 2119.
- **SPEC-DERIVED-§N.N** references Architecture.md.
- All Cells implement the `Cell` trait (Architecture.md §5.1).
- Embeddings + rerank scores deterministic given identical inputs, weights, `seed`.

---

## §1 — Mission

Embedding and reranking are UltraCortex's **reasoning scaffolding workers** (Architecture.md P15). They exist so the LLM never has to do work a deterministic Cell can do.

1. **VectorCell** owns embedding + HNSW ANN.
2. **RerankerCell** owns small cross-encoder for top-k refinement.
3. Both are per-shard, share-nothing, mmap'd weights.
4. Both align natively with **DeepSeek shapes and chunking heuristics**.
5. Both feed `recall` and `view` via hybrid retrieval (§4).

---

## §2 — VectorCell — Embedding

### §2.1 Dual-Index HNSW

| Index | Dim | Use case |
|-------|-----|----------|
| `index_768`  | 768  | smaller models, fast recall |
| `index_1536` | 1536 | high-fidelity semantic recall, DeepSeek-aligned |

Both indexes share `id_map`. NodeId resolves to either embedding.

### §2.2 DeepSeek-Aligned Chunking

1. Semantic boundary detection (paragraph + sentence).
2. Token cap: 512 or 1024 (namespace-configurable).
3. Overlap: 64 tokens.
4. Header injection: `[doc:%s | section:%s]` per chunk.

These defaults match DeepSeek V3/R1 ideal coherent ~1k-token windows.

### §2.3 Pipeline

```
text → chunker → tokenizer → model(mmap weights) → vector → HNSW.insert
```

- Per-shard share-nothing; no GPU sharing across shards.
- CPU path: `ggml`/`candle`. GPU path: optional via `--features=cuda`.
- Weights mmap'd; eviction OS-managed.

### §2.4 Determinism

- Canonical tokenizer output.
- Fixed seed, deterministic kernels.
- dtype f32; no quantization on hot path (PQ available in cold tier; **[GAP-004]**).

### §2.5 Performance Targets

| Op | p50 | p99 |
|----|-----|-----|
| chunk one doc (10 KB) | 0.5 ms | 3 ms |
| embed one chunk (1024 tok) CPU | 8 ms | 25 ms |
| ANN search k=10 | 200 μs | 1 ms |
| HNSW insert | 1 ms | 5 ms |

---

## §3 — RerankerCell — Small Cross-Encoder

### §3.1 Model

- Default: `bge-reranker-base` class (~110M params).
- Weights mmap'd; shared across shards via OS page cache.
- Hot-swap via `Cell::migrate(old) -> new`.

### §3.2 Pipeline

```
recall top-K (vector) ─┐
                       ├─→ RerankerCell ─→ top-k reordered with scores
recall top-K (BM25)   ─┘
```

K=50 default, k=10 default. Both per-namespace configurable.

### §3.3 Determinism

Same seed + same input pairs → byte-identical scores.

### §3.4 Performance Targets

| Op | p50 | p99 |
|----|-----|-----|
| score 50 pairs CPU | 30 ms | 80 ms |
| score 50 pairs GPU | 5 ms  | 15 ms |

---

## §4 — Hybrid Retrieval

### §4.1 Fusion

Default = **Reciprocal Rank Fusion (RRF)** with k=60:

```
score(d) = Σ_{r ∈ rankers} 1 / (k + rank_r(d))
```

Where `rankers = {VectorCell, BM25Cell, GraphCell}`.

### §4.2 Weights

Per-namespace overrides. Default `(vector: 1.0, bm25: 1.0, graph: 0.5)`. **[GAP-004]** tuning bench.

### §4.3 Pipeline

```
query ──┬─→ VectorCell.search(K)  ─┐
        ├─→ BM25Cell.search(K)    ─┼─→ RRF fuse ─→ RerankerCell ─→ top-k handles
        └─→ GraphCell.expand(K)   ─┘
```

Result is **handles, not bodies** (P1/P14). Bodies hydrate only when agent calls `hydrate`.

---

## §5 — Worker Offloading (P15)

| Work | Worker | Effect on LLM prompt |
|------|--------|----------------------|
| embedding similarity | VectorCell | LLM receives top handles, not candidates |
| lexical match | BM25Cell | top handles only |
| graph expansion | GraphCell | expanded handle set only |
| score refinement | RerankerCell | final order, no rationales |
| summarization | SummarizerCell ([GAP-NT-013]) | skeleton, not full body |
| congruence diff | CongruenceCell | `delta_handle` or none |
| anchor lookup | SpecAnchorCell | `anchor_ref` only |
| budget arithmetic | WorkBudgetCell | `tier_hint`, not raw budget |

**Cumulative:** LLM sees only the final decision surface — never candidates, never rationales, never diagnostics. Core token-efficiency lever.

---

## §6 — SummarizerCell (Proposed — [GAP-NT-013])

Phase 1B Cell producing ≤80-token skeletons from large bodies on write. Without it: every skeleton is hand-authored or LLM-extracted. With it: skeleton stored on write; recall serves it with zero LLM cost.

Status: proposed. Conformance: skeleton must preserve top-3 entities and canonical action verb.

---

## §7 — Prefix-Stable Embedding Pipeline

When Router calls VectorCell.search, response is lex-sorted by handle so subsequent identical queries produce identical bytes in the assembled View (RouterScheduler.md §9). Guarantees PrefixCacheStore (PersistenceLayer.md §9) returns identical bytes for same `ViewKey`.

---

## §8 — Batch Policy

- Embedding batches: up to 32 chunks per `embed()`.
- Reranker batches: up to 64 pairs per `score()`.
- Batches form from shard's inbox; no cross-shard.

---

## §9 — Cache Integration

- VectorCell results cached in CacheCell keyed by `(query_hash, k, filters)`.
- TTL: 5 min logical default.
- Invalidated on `node.written` / `node.superseded` for any indexed node.

---

## §10 — Conformance Tests

Every release MUST pass:

1. **Embedding determinism**: same input + seed → byte-identical vector × 1k iterations.
2. **Reranker determinism**: same input → byte-identical scores × 1k iterations.
3. **Hybrid recall@10**: ≥ 0.85 on held-out benchmark.
4. **Token-efficiency end-to-end**: tokens-injected-per-step ≤ 1.5 KB p50 on DeepSeek multi-step coding agent bench (**[GAP-NT-010]**).
5. **Prefix-cache hit rate**: ≥ 80% on same bench (**[GAP-DS-001]**).

---

## §11 — GAPs

| ID | Description |
|----|-------------|
| GAP-004    | Hybrid retrieval ranking weights |
| GAP-NT-010 | Token-injected-per-step acceptance bench |
| GAP-NT-013 | SummarizerCell |
| GAP-DS-001 | DeepSeek prefix-cache hit-rate measurement |

---

## §12 — Congruence Contract

Congruent with: Architecture.md (§10, §14), CellTaxonomy.md (VectorCell §7, BM25Cell §9, GraphCell §8, RerankerCell §15), RouterScheduler.md (view assembly), PersistenceLayer.md (PrefixCacheStore), NATIVE_TRINITY.md, DeepSeekOptimization.md.

_End of EmbeddingReranker.md v1.0 (UltraCortex)._


---

# 🆙 UltraCortex v1.0 Delta — SummarizerCell Closed, Librarian Subsumes

## §A.1 GAP-NT-013 (SummarizerCell) — CLOSED in UltraCortex v1.0

The SummarizerCell proposed in HyperCortex §6 is **subsumed by LibrarianCell** (CellTaxonomy.md §22).

Skeleton generation is now a `LibrarianCell` operation mode:

```rust
LibrarianMode::Skeleton — generates ≤80-token skeletons from large bodies
```

Unlike a standalone SummarizerCell, the Librarian's skeletons are:
- subject to WardenCell audit (mutual accountability),
- recorded in CrossCheckLedgerCell,
- governed by the Trinity (Quarantine, GapCell fixation, ContractCell pinning),
- emitted with PUBLIC/PRIVATE split per **P19** (Asymmetric Visibility).

This is strictly stronger than a deterministic summarizer Cell because it adds semantic understanding (the model can recognize entity equivalence) while remaining accountable to the substrate.

## §A.2 No Other Embedding/Reranker Changes

Sections §2 (VectorCell), §3 (RerankerCell), §4 (Hybrid Retrieval), §5 (Worker Offloading), §7 (Prefix-Stable Pipeline), §8 (Batch Policy), §9 (Cache Integration), §10 (Conformance Tests) of the HyperCortex content above remain normative for UltraCortex v1.0.

## §A.3 Congruence Contract (Updated)

Congruent with: Architecture-UltraCortex.md (§10, §14), CellTaxonomy.md (VectorCell §6, BM25Cell §8, GraphCell §7, RerankerCell §14, **LibrarianCell §22 — subsumes SummarizerCell**), RouterScheduler.md, PersistenceLayer.md, NATIVE_TRINITY.md, DeepSeekOptimization.md, **CURATOR_PAIR_PROTOCOL.md**, **LibrarianCell.md**.

_End of UltraCortex v1.0 Delta._
