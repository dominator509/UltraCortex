# SYSTEM_REQUIREMENTS.md — UltraCortex v1.0 Theoretical System Requirements

**Status:** v1.0 Supporting Document | Derived from spec budgets

## §0 Conventions
Theoretical minimums derived from RouterScheduler.md §13, PersistenceLayer.md, CURATOR_PAIR_PROTOCOL.md.

## §1 Three Deployment Tiers

| Resource | Min (Dev) | Reference (Prod) | Heavy (Multi-Tenant) |
|---|---|---|---|
| **CPU cores** | 6 physical (was 4 in HyperCortex; +2 for Curator+Adjudicator shards) | 14–18 (was 12–16) | 36–72 (was 32–64) |
| **RAM** | **6 GiB** (was 4; +1.5 GiB Librarian Q4 + ~0.5 GiB Warden KV+overhead at min) | **34–66 GiB** (was 32–64; modest delta) | **128–512 GiB** (essentially unchanged) |
| **Disk** | 24 GiB SSD (was 20; +~4 GiB weight files: Gemma+Qwen+Adjudicator pool Q4) | 1 TB NVMe | 4–16 TB NVMe |
| **OS** | Linux 5.10+ / macOS 12+ / Win11+WSL2 | Linux 5.13+ (io_uring) | Linux 5.13+ |
| **CPU SIMD** | **AVX2/NEON REQUIRED** for Curator inference (CPU p50 jumps from ~180 ms → ~600 ms without) | AVX2/NEON | AVX2/NEON |
| **GPU** | None (CPU is the supported baseline) | Optional CUDA for reranker | CUDA recommended (heavy) |
| **KMS** | OS keychain / file | KMS provider | Cloud KMS / HSM (T3) |
| **Network** | Loopback only (UDS) | 10 GbE | 25–100 GbE |

## §2 Curator Resource Delta vs HyperCortex

| Component | RAM | Disk | CPU effect |
|---|---|---|---|
| LibrarianCell (Gemma 2 2B Q4_K_M) | ~1.5 GiB | ~2.0 GB weight file | 1 dedicated shard, ~180 ms p50 inference |
| WardenCell (Qwen 2.5 Coder 1.5B Q4_K_M) | ~1.2 GiB | ~1.0 GB weight file | 1 dedicated shard, ~140 ms p50 inference |
| AdjudicatorCell (3-model pool) | ~5.7 GiB (only one model active at a time + RAM caching) | ~3.5 GB combined | 1 dedicated shard, ~300 ms p50 inference when LLM-active |
| CrossCheckLedgerCell | ~64 MiB index | grows with audit volume | trivial |
| **Total delta (typical)** | **+2.5 GiB working** (Adjudicator inactive most of the time) | **+6.5 GB** | **+2 shards** vs HyperCortex |

## §3 What UltraCortex Does NOT Require

Same non-requirements as HyperCortex, plus:
- ❌ **No internet connectivity for Curator inference** (models run locally)
- ❌ **No API budget for Curator inference** (models are open-source, run locally)
- ❌ **No GPU** for Curator operation (CPU baseline is supported and tested)
- ❌ **No external LLM API calls** at all — UltraCortex is fully self-contained

## §4 Latency Budget Summary

| Op | p50 | p99 |
|---|---|---|
| Standard write (no semantic check) | ~150 μs end-to-end | ~1 ms |
| Write with `flags.semantic_check=true` (Warden sync) | ~150 ms | ~400 ms |
| Adjudicator deterministic resolution | <50 μs | <200 μs |
| Adjudicator LLM resolution | ~300 ms | ~700 ms |
| Async Librarian curation (batched 16-job) | ~95 ms/job effective | ~190 ms/job |

The hot path is **unaffected** by Curator Cells unless `flags.semantic_check=true` or `severity=P0`.

## §5 Theoretical Sizing Math

RAM ≈ Σ(cell stable regions) + PrefixCacheStore cap + HNSW indexes + **Curator weights (~3 GiB resident) + KV caches (~1 GiB)** + Trinity shard overhead.

Disk ≈ WAL retention + CAS blobs + snapshots + PrefixCacheStore + **CrossCheckLedger WAL stream + ~7 GB of model weights**.

Sustained throughput per shard ≥100k msg/s (unchanged from HyperCortex).

## §6 Open Sizing GAPs

Inherited GAPs + NEW:
- **GAP-CU-010** — per-Cell KV cache size budget → affects RAM at scale.
- **GAP-CU-013** — Curator shard topology for small deployments (co-tenant on shard 0 vs dedicated).
- **GAP-CU-007** — Adjudicator LLM pool composition (3 vs 5 models) → affects RAM ceiling.

## §7 Quick Reference Card

| Tier | Cores | RAM | Disk | Sustained Throughput |
|---|---|---|---|---|
| **Min (dev)** | 6 | 6 GiB | 24 GiB SSD | ~30 k ops/s |
| **Reference (prod)** | 14–18 | 34–66 GiB ECC | 1 TB NVMe | ~1–1.4 M ops/s |
| **Heavy (multi-tenant)** | 36–72 | 128–512 GiB ECC | 4–16 TB NVMe | ~3–6 M ops/s |

_End of SYSTEM_REQUIREMENTS.md v1.0 (UltraCortex)._
