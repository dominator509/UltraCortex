# LibrarianCell.md — **UltraCortex** Memory Archive Librarian

**Status:** v1.0 Normative | **Owner:** Dominic Sarria-Wiley | **Phase:** 1G

## §0 Conventions
RFC 2119. SPEC-DERIVED-§N.N. [GAP-CU-NNN]. Subject to Trinity governance per P20.

## §1 Mission
The Memory Archive Librarian is an **in-substrate LLM** that curates memory at write time:
- generates ≤80-token **skeletons** from large bodies (closes GAP-NT-013, the proposed SummarizerCell);
- proposes **supersessions** when new facts subsume old ones;
- emits **archive tags** classifying facts for hot vs cold tiering.

Async-only. Never in the default pre-validation hot path. Subject to mutual accountability via WardenCell (CURATOR_PAIR_PROTOCOL.md §3, §5).

## §2 Model Selection
**Default:** Gemma 2 2B Q4_K_M.
- ~1.5 GiB Q4 RAM, ~180 ms p50 CPU inference (AVX2/NEON, 4-core).
- Apache 2.0 license. Proven on-device deployment (Chrome).
- Pinned by SHA-256 in `ContractCell`. Swapping = a Decision record, never silent merge.

**Alternates:** Gemma 3, SmolLM-2 1.7B (smaller deployments). **[GAP-CU-001]**.

## §3 State

```rust
pub struct LibrarianState {
    model_handle:     ModelHandle,        // mmap'd weights, SHA-256 pinned
    kv_cache:         KvCacheArena,       // RAM-only, NEVER persisted
    pinned_seed:      u64,                // from ContractCell
    pinned_prompt:    SystemPromptHandle, // pinned in ContractCell
    mode:             LibrarianMode,      // Skeleton | Supersede | ArchiveTag
    pending_outputs:  HashMap<TaskId, CuratorOutput>, // PENDING until Warden audits
}
```

## §4 Invocation Paths
- **ASYNC ONLY** on `node.written` events from Memory/Payload Cells (FactCell, TimelineCell, BlobCell).
- **NEVER** in the default pre-validation hook chain — would add ~180 ms to a ≤100 μs budget.
- **Opt-in sync** only via `flags.semantic_check = true` (rare; explicit caller request).

Jobs batch in groups of 16 to amortize tokenizer + KV-cache warmup.
v1.0 ratifies three supported Librarian KV-cache planning budgets via `[curator].kv_budget_profile`: `small = 256 MiB`, `reference = 384 MiB` (default), `heavy = 768 MiB`. `curator status` surfaces the live profile and derived MiB totals.

## §5 Output Schema
Per CURATOR_PAIR_PROTOCOL.md §3.1, every output split into PUBLIC + PRIVATE.

PUBLIC: `operation` (Skeleton | SupersedeProposal | ArchiveTag), `target_handle`, `grounded_in`, `confidence_band`, `schema_id`, `spec_anchor`, `logical_at`.

PRIVATE: `rationale_handle`, `considered_alts`, `confidence_precise`, `reasoning_trace`, `private_seed`. **Capability-scoped — Warden cannot read.**

Output lands as PENDING. WardenCell audits before it becomes Active. (CURATOR_PAIR_PROTOCOL.md §1.2)

## §6 Grounding Whitelist
The Librarian MAY read:
- ✅ FactCell (canonical facts)
- ✅ TimelineCell (recent events)
- ✅ Its own prior PUBLIC outputs (for consistency)
- ✅ ContractCell schemas governing the write
- ✅ The node currently being written + its body

The Librarian MUST NOT read:
- ❌ Agent prompt / agent's full context
- ❌ WardenCell outputs (asymmetric — auditor sees auditee, not reverse)
- ❌ ScratchpadCell content not explicitly scoped to it
- ❌ CrossCheckLedger prior decisions

This is what keeps the Librarian grounded in substrate truth, not agent context.

## §7 Determinism Contract
- `temperature = 0`, greedy decode.
- `seed = envelope.seed` (forwarded from caller).
- Pinned system prompt + pinned weights → byte-identical output for identical input.
- Replays bit-deterministic under WAL replay.

## §8 PreValidator Behavior
- Default: no-op. Librarian is not in the chain.
- With `flags.semantic_check`: opt-in path runs sync after Trinity chain succeeds.

## §9 Performance Targets
| Op | p50 | p99 |
|---|---|---|
| Single inference (CPU AVX2) | ~180 ms | ~400 ms |
| Single inference (CPU NEON ARM64) | ~210 ms | ~450 ms |
| 16-job batch | ~1.5 s | ~3.0 s |
| Effective per-job (batched) | ~95 ms | ~190 ms |

GPU optional via `--features=cuda` → ~30 ms p50 per job.

## §10 Closes GAP-NT-013
The proposed SummarizerCell from HyperCortex is subsumed. Skeleton generation is now a LibrarianCell mode.

## §11 GAPs
| ID | Description |
|---|---|
| GAP-CU-001 | Librarian default model |
| GAP-CU-003 | Confidence-band threshold defaults |

## §12 Congruence Contract
Congruent with: Architecture.md (§16), CURATOR_PAIR_PROTOCOL.md, WardenCell.md, CrossCheckLedgerCell.md, RouterScheduler.md (`flags.semantic_check`), PersistenceLayer.md (weight-file storage + ContractCell pinning), ObservabilityAudit.md (librarian.* metrics).

_End of LibrarianCell.md v1.0._
