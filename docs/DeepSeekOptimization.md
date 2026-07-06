# DeepSeekOptimization.md — UltraCortex v1.0 DeepSeek Integration

**Status:** v1.0 Normative | RFC 2119 | Unchanged from HyperCortex v1.0 except §13 (Curator-Pair interaction)

## §0 Conventions
RFC 2119. SPEC-DERIVED-§N.N. [GAP-DS-NNN].

## §1 Mission
First-class DeepSeek alignment with provider-agnostic protocol surface. Targets:
- ≤1.5 KB tokens/step p50 (≤4 KB p99)
- ≥80% prefix-cache hit rate
- ≤0.25 hydrate/recall ratio

## §2 Prefix-Stable View Layout
Canonical order: header → handles[] → skeletons[] → bodies[] → footer. Lex-sorted by handle at every level.

## §3 PrefixCacheStore Integration
ViewKey = (view_id, namespace_id, view_version, params_canonical_hash). Lookup on every `view` request.

## §4 FIM Framing
DeepSeek-Coder: `<|fim_begin|>{prefix}<|fim_hole|>{suffix}<|fim_end|>`.

## §5 R1 `<think>` Strip Mode
Default strip; opt-in `include_reasoning: true`.

## §6 Function-Call Grammar
Lowercase verbs, single-level arg objects, explicit required arrays, canonical key ordering.

## §7 Chunking Defaults
512/1024 token windows, 64-token overlap, `[doc:%s | section:%s]` headers.

## §8 Token-Budget Envelope
Mandatory WorkBudget. Tier escalation L0→L1→L2→L3 only on lower-tier failure.

## §9 Symbolic Pointer Compression
Handles, not text. ~25 tokens vs ~400 tokens.

## §10 Worker Offloading
Vector, BM25, Graph, Reranker, **Librarian** — all do reasoning the LLM doesn't.

## §11 Measured Targets (UPDATED v1.0)

| Metric | HyperCortex Target | UltraCortex Target |
|---|---|---|
| Tokens/step p50 | ≤1.5 KB | ≤1.5 KB (with Curator: plausibly ≤800 B) |
| Tokens/step p99 | ≤4 KB | ≤4 KB |
| Prefix-cache hit | ≥80% | ≥80% |
| Hydration ratio | ≤0.25 | ≤0.25 |
| Skeleton quality | regex-extracted | **Librarian-generated** (semantically richer) |

## §12 Conformance Tests
1. Prefix stability — byte-identical bytes on identical requests.
2. Append stability — long shared prefix preserved.
3. FIM framing correctness.
4. R1 strip correctness.
5. Function-call grammar lints.
6. End-to-end token efficiency targets met.

## §13 Curator-Pair Interaction (NEW v1.0)
The Curator Pair operates **in parallel** with DeepSeek optimization, not in conflict:
- **LibrarianCell** generates better skeletons than regex extraction → further compresses tokens-injected-per-step. Plausible improvement: ≤1.5 KB p50 → **≤800 bytes p50**.
- **WardenCell** audits agent envelopes for semantic drift before they consume tokens. Latency cost = 0 unless `flags.semantic_check=true` or `severity=P0` (rare).
- **PrefixCacheStore** is unaffected — Curator outputs are stored separately (`cache/curator/...`) from view caches.
- **R1 `<think>` strip** still applies. Warden does NOT see agent `<think>` blocks (they're stripped before envelope arrives).

## §14 GAPs
GAP-DS-001..004 unchanged.

## §15 Congruence
Congruent with Architecture.md (§14), McpProtocol.md, RouterScheduler.md, PersistenceLayer.md, EmbeddingReranker.md, NATIVE_TRINITY.md, **CURATOR_PAIR_PROTOCOL.md** (does not contradict P19/P20).

_End of DeepSeekOptimization.md v1.0 (UltraCortex)._
