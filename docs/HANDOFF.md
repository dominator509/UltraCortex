# HANDOFF.md — UltraCortex v1.0 Live GAP Register

**Status:** v1.0 | Live tracking | Update cadence: every HALT | Audited against current checkout on 2026-08-13 (`cargo test`: 162 passed)

## §0 Conventions
Four GAP namespaces: `GAP-NNN` (carryover), `GAP-NT-NNN` (Trinity), `GAP-DS-NNN` (DeepSeek), `GAP-CU-NNN` (Curator pair — NEW v1.0). Status: open / in_progress / blocked / resolved / quarantined / deferred / closed.

Rows marked `closed` below are gaps whose implementation now exists in the current checkout and is covered by local code/tests. Rows marked `in_progress` have concrete behavior implemented but lack a full benchmark/closure proof. Rows left `open` or `deferred` remain real policy, benchmark, deployment, or runtime-environment gaps even if some supporting code already exists.
Second-pass checklist audit on 2026-08-13 found no stale open/deferred rows; the remaining notes below were tightened to match the live checkout.

## §1 Carryover GAPs
| ID | Description | Phase | Status | Audit note |
|---|---|---|---|---|
| GAP-001 | Rebalancing | 2+ | deferred | Runtime rebalancing is not implemented in the current single-binary checkout. |
| GAP-002 | Final allocator selection | 0 | open | `Cargo.toml` and `src/` still contain no allocator-selection seam (`#[global_allocator]`, jemalloc, mimalloc, or equivalent). |
| GAP-003 | WASM-hosted Cells | 2+ | deferred | No WASM runtime or Cell-host boundary exists in this tree. |
| GAP-004 | Hybrid retrieval ranking weights | 1B | open | The live `recall` path unions BM25 + Vector hits and reranks them, but the documented `(vector, bm25, graph)` weighting policy is not wired into the Router and GraphCell does not participate in ranking yet. |
| GAP-005 | Cross-shard transactions | deferred | deferred | Writes still route to exactly one shard by handle hash; there is no transaction coordinator, rollback path, or cross-shard replay contract. |
| GAP-006 | MCP-over-TCP TLS defaults | 1C | closed | v1.0 now ratifies a fail-closed transport policy: plaintext TCP is permitted only on loopback, config/runtime reject non-loopback listener addresses, and the loopback-only policy is covered by proto + bootstrap regression tests. |
| GAP-009 | T3 key-rotation cadence | 1E | closed | T3 now persists custody state in `kms/keyring.cbor`, exposes audited `kms status` / `kms rotate [--emergency]` admin verbs, and the recorded drill artifact `docs/benchmarks/kms_rotation_drill_2026-07-07.json` proves seal/unseal continuity plus retained-key verification across key rolls. |
| GAP-010 | Final token-efficiency acceptance bench | 1B/1F | closed | `tests/acceptance_bench.rs` now drives a deterministic multi-step Router workload and the recorded artifact `docs/benchmarks/deepseek_acceptance_2026-07-07.json` clears the v1.0 gate (`856` bytes p50, `1052` bytes p99, `84.21%` cache-hit rate, hydrate/recall `0.25`). |
| GAP-011 | Proposal quorum defaults | 1D | closed | `ProposalCell` quorum behavior is implemented and unit-tested in the current checkout. |
| GAP-012 | Snapshot pause-window upper bound | 1E | closed | The live snapshot path now measures the checkpoint cut as `pause_us`, surfaces `total_us` plus capture/write/manifest breakdown through `ultracortex snapshot`, alerts via `snapshot.pause_target_exceeded` above `50_000 µs`, and the recorded artifact `docs/benchmarks/snapshot_pause_2026-07-07.json` proves the representative workload stayed within bound (`562 µs` max). |
| GAP-013 | OTel exporter default endpoints | 1F | closed | `OtlpConfig` provides conventional localhost OTLP/HTTP JSON endpoints (`4318/v1/{metrics,traces,logs}`), TOML/env/CLI overrides, explicit `metrics export`, and a loopback collector smoke test. |
| GAP-014 | Federation | 2+ | deferred | No federation/runtime multi-host substrate is implemented. |

## §2 Native Trinity GAPs
| ID | Description | Status | Audit note |
|---|---|---|---|
| GAP-NT-001 | Trinity-shard topology default | closed | Bootstrap now defaults Trinity to `dedicated`, accepts `co-tenant-shard-0` as the supported override, and surfaces the live choice through admin status plus boot coverage. |
| GAP-NT-002 | Anchor edge granularity | closed | v1.0 now ratifies section-scoped anchors: the live identity is exact `doc§section`, line fragments are not separate anchor IDs, and local tests cover both accepted section anchors and rejected line-like fragments. |
| GAP-NT-003 | Decision conflict policy for `scope=*` | closed | `DecisionLedgerCell` now treats `scope=*` as a wildcard fallback that governs only when no exact-scope active decision exists; exact-scope decisions shadow the wildcard, with regression coverage proving fallback and shadowing behavior. |
| GAP-NT-004 | Congruence delta acceptance UI | closed | The admin plane now supports congruence preview/audit/accept, and `congruence_admin_workflow_accepts_delta_and_unblocks_write` proves writes stay blocked until the relevant delta is accepted. |
| GAP-NT-005 | Gap dispatch counter window N | closed | `N=8` is implemented and covered by `src/trinity/cells.rs` + `gap_fixation_at_n_plus_one` conformance coverage; this gap is now implementation-complete in this checkout. |
| GAP-NT-006 | Quarantine retention horizon | closed | `QuarantineCell` now exposes and enforces the v1.0 policy: resolved items are retained for `1_000_000` logical ticks while pending items are never pruned; sweep behavior is covered by local tests and the admin plane. |
| GAP-NT-007 | WorkBudget defaults per namespace | closed | WorkBudget now seeds namespace defaults (`admin=250_000`, `bootstrap=1_000_000`, `curator=10_000_000`, fallback `100_000`), surfaces them through `budget defaults`, and round-trip coverage proves restore correctness. |
| GAP-NT-008 | ContractCell migration tooling | closed | ContractCell now exposes plan/apply/verify migration tooling with stored `migration_plan_handle` + Decision linkage, downgrade rejection, and deadline-gated apply that deprecates the source schema only once the logical cutoff is reached. |
| GAP-NT-009 | Pre-validation chain ordering proof | closed | The chain-order proof sketch is now backed by `fixed_chain_order_short_circuits_before_later_steps`, which locks the short-circuit ordering in local regression coverage. |
| GAP-NT-010 | Token-injected-per-step acceptance bench | closed | `deepseek_acceptance_bench_meets_targets` now proves the read-path budget target on the real Router, with a checked-in artifact showing `856` bytes p50 and `1052` bytes p99 across the representative workload. |
| GAP-NT-011 | Severity-tag propagation across cross-cell calls | closed | Originating severity now survives Router -> Curator -> Trinity handoff for curator-spawned writes, with regression coverage proving `P0`, `P1`, and `P2` are preserved in quarantined curator artifacts. |
| GAP-NT-012 | Audit signing key custody | closed | CrossCheck batch signatures now persist key ids in `wal/cross_check/batch-signatures.cbor`, recovery reloads and verifies every completed batch before serving, and the recorded drill artifact `docs/benchmarks/kms_rotation_drill_2026-07-07.json` plus `audit verify` surface the expected/verified batch counts and signature integrity. |
| GAP-NT-013 | SummarizerCell | closed | Closed by Librarian skeleton generation in the current checkout. |
| GAP-NT-014 | PrefixCacheStore eviction policy | closed | Prefix-cache eviction is implemented as per-tier deterministic LRU with hard-capacity enforcement and tombstone invalidation via `PrefixCacheStore`; no test-only stub. |

## §3 DeepSeek GAPs
| ID | Description | Status | Audit note |
|---|---|---|---|
| GAP-DS-001 | DeepSeek prefix-cache hit-rate measurement | closed | The deterministic acceptance bench now measures prefix-cache reuse on the live `view` path and records an `84.21%` hit rate in `docs/benchmarks/deepseek_acceptance_2026-07-07.json`. |
| GAP-DS-002 | FIM framing for non-Coder DeepSeek variants | closed | The live `view` formatting path now applies real FIM tags for `deepseek-coder` and a plain prefix+suffix splice for `deepseek-v3` / `deepseek-r1`, with end-to-end coverage in `tests/deepseek_formatting.rs`. |
| GAP-DS-003 | R1 `<think>` canonical strip format | closed | `r1_strip` is implemented and covered by `r1_strip_variants` tests in `src/deepseek.rs`. |
| GAP-DS-004 | View-schema versioning for prefix stability | closed | The live `view` path now rejects stale/future exact version requests, supports opt-in forward migration via `allow_migrate`, returns `view_version` / `migrated_from` / `view_key` metadata, and `tests/view_versioning.rs` proves the mixed-version reader contract end-to-end. |

## §4 Curator Pair GAPs (NEW v1.0)

| ID | Description | Phase | Status | Audit note |
|---|---|---|---|---|
| GAP-CU-001 | Librarian default model (Gemma 2 2B vs Gemma 3) | 1G | closed | Production `CuratorConfig` pins `gemma-2-2b-it-q4_k_m`, verifies its SHA-256 file at boot, and wires the backend through `LibrarianCell`; missing deployment artifacts fail closed. |
| GAP-CU-002 | Warden default model (Qwen 2.5 Coder 1.5B vs alternates) | 1G | closed | Production `CuratorConfig` pins the distinct `qwen2.5-coder-1.5b-q4_k_m` family, verifies it at boot, wires it through `WardenCell`, and self-test rejects a non-GGUF production backend. |
| GAP-CU-003 | Confidence-band threshold defaults | 1G | closed | `ConfidenceBand::from_precise` fixes visible defaults (`<0.45`, `0.45..0.8`, `>=0.8`) and policy defaults are part of current curator behavior. |
| GAP-CU-004 | Disagreement quota bounds (default 92–97%) | 1G | closed | Quota defaults are implemented (`0.92`/`0.97`) in `CuratorConfig`, loaded from TOML/env, and enforced in `CrossCheckLedgerCell::health`. |
| GAP-CU-005 | Adversarial probe schedule + corpus | 1G | closed | v1.0 ratifies deterministic fabricated-handle probes at base rate `0.001`, with `x10` boost under suspicious agreement; scheduler and probe-path coverage exist locally and the admin plane surfaces the defaults. |
| GAP-CU-006 | Calibration drift detection window size | 1G | closed | v1.0 ratifies a rolling window of `50` outcomes with degraded mode below `0.85` for High and `0.60` for Medium confidence bands; thresholds are tested and surfaced by `curator status`. |
| GAP-CU-007 | Adjudicator LLM pool composition (3 vs 5 models) | 1G | closed | Default pool is explicit and concrete (`phi-3.5-mini`, `llama-3.2-3b`, `smollm2-1.7b`) with seeded rotation over configured members. |
| GAP-CU-008 | Adjudicator rotation policy details | 1G | closed | Rotation is implemented as `seed % pool.len()`, with judge-specific tie-break salt and policy table precedence before pool voting. |
| GAP-CU-009 | Cross-check ledger retention horizon | 1G | closed | v1.0 now ratifies indefinite local retention for CrossCheck records (never prune in-process); the admin plane surfaces the policy and snapshot/restore coverage proves records persist. |
| GAP-CU-010 | Per-Cell KV cache size budget | 1G | closed | v1.0 now ratifies supported KV-budget profiles (`small`, `reference`, `heavy`), surfaces the derived per-Cell MiB limits through `curator status`, and validates operator overrides in bootstrap/config parsing. |
| GAP-CU-011 | Blind re-audit sample rate (default 1%) | 1G | closed | Blind re-audit uses concrete defaults (`blind_reaudit_rate = 0.01`) and deterministic scheduler, with coverage tests for scheduler behavior. |
| GAP-CU-012 | Capability-token negation glob canonicalization | 1G | closed | Negation globs are implemented, enforced, and covered by local conformance/self-tests. |
| GAP-CU-013 | Curator shard topology for small deployments | 1G | closed | Bootstrap now defaults Curator placement to `dedicated`, accepts `co-tenant-shard-0` as the supported small-deployment override, and surfaces the live choice through admin status plus boot coverage. |
| GAP-CU-014 | Human escalation routing policy | 1G | closed | Human escalations now have an explicit v1.0 route: the Adjudicator queues them, `curator.adjudication` is ALWAYS_DELIVER to `AgentRegistry` escalation subscribers, and operators acknowledge by `resolve`ing queued handles. |

## §5 Inter-Agent Handoff Protocol
Every write carries `agent_id`. Every Decision references issuing agent. Cross-agent contention detected by DecisionLedgerCell. Resolution via explicit `supersede`. Human-in-the-loop is the only path to resolve true contention.

## §6 SPEC-DERIVED Coverage
After Phase 1A.2 (SpecAnchorCell live): every `SPEC-DERIVED-§N.N` marker in the v1.0 corpus has a live AnchorNode. Coverage auto-reported by `ultracortex contract list --anchors`.

## §7 Congruence Audit Summary
After Phase 1A.3 (CongruenceCell live): every pair (doc_i, doc_j) in the SoT-13 has a matrix entry. Unaccepted deltas empty at HALT-gate evaluation.

## §8 Next Actions
- Per-gap closure criteria now live in `GAP_CLOSURE_CHECKLIST.md`.
- Assign owners to all `Owner: TBD` rows.
- Provision the operator-owned `llama-cli` and the two SHA-pinned GGUF files before a production (non-`--dry-run`) boot; no software credential is required.

_End of HANDOFF.md v1.0 (UltraCortex)._
