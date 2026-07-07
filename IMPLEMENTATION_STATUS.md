# IMPLEMENTATION_STATUS — UltraCortex v1.0 Rust tree

Status date: 2026-07-07. Sources of truth: the 22 spec documents in
`docs/`; conflicts resolved in favor of CURATOR_PAIR_PROTOCOL.md and
NATIVE_TRINITY.md per Architecture.md §1.

## 1. Verification status — read this first

This tree was authored in a sandbox **without a Rust toolchain or
network access**. That was true during authoring, but this checkout has
since been compiled and locally validated on **2026-07-07**:

- `cargo test` now passes locally (`158 passed`, 7 suites);
- the standard-vector and conformance coverage below still matters, but
  the remaining work is no longer "first compile" uncertainty;
- the audited open/closed gap register now lives in `docs/HANDOFF.md`.

Mitigations baked in during authoring:

- unit tests carry external standard vectors (FIPS 180-4 SHA-256,
  RFC 2202/4231 HMAC, CRC32C "123456789" → 0xE3069283, RFC 8949
  canonical CBOR examples, ULID base32 edge cases), so the first
  `cargo test` validates the primitives against ground truth rather
  than against the implementation's own assumptions;
- every inter-module call site was grep-verified against the callee's
  actual signature during authoring;
- the conformance suite (T1–T8, C1–C10) exercises the full stack
  through the public Router, not internals.

The expected first-compile fixes (lifetimes, imports, and a few wire/doc
alignment issues) have now been landed in this checkout. Nothing in the
design depends on anything unverifiable, but several policy and
deployment gaps remain open in `docs/HANDOFF.md`.

## 2. Structural deviations from the blueprint

| # | Blueprint | This tree | Why / upgrade path |
|---|-----------|-----------|--------------------|
| D1 | 9-crate workspace | 1 package, 11 modules mirroring the crate map (lib.rs table) | Faster iteration; module boundaries match crate boundaries, so `cargo new` + move is mechanical. |
| D2 | External deps allowed (serde, tokio, ed25519-dalek…) | **Zero dependencies**, std only | Supply-chain surface = 0; determinism easier to prove. All primitives in-tree with test vectors. Costs: no async runtime (thread-per-conn), hand-rolled CBOR. |
| D3 | Ed25519 capability-token signatures | HMAC-SHA256 behind the `Signer` trait | Single-node v0: issuer == verifier, HMAC is sound. Multi-node drops an Ed25519 `Signer` impl behind the same seam (router/captoken.rs). |
| D4 | CBOR full numeric tower | Canonical CBOR with **f64-only floats**, u64/i64 ints | Sufficient for every spec schema; canonical-form rules (shortest int encoding, sorted map keys, definite lengths) fully implemented. |
| D5 | Encryption tier T3 (external KMS wrap) | **Implemented via a persisted local keyring seam** | T3 now opens, stores custody state in `kms/keyring.cbor`, exposes audited `kms status` / `kms rotate` verbs, and verifies CrossCheck batch signatures on recovery. A real external KMS/HSM can still replace the local keyring behind the same seam later. |
| D6 | Async curation consumer on `node.written` | **Synchronous curation** inside the write path | Deterministic, replay-exact, and makes B5 self-tests direct. Seam: `router::run_curation_cycle` — an async consumer is a drop-in (spawn + queue) at the cost of replay complexity. |
| D7 | SIGTERM/SIGINT graceful shutdown | Admin `shutdown` verb over the socket; no signal handler | std has no signal API and libc is a dependency. `ultracortex shutdown` performs the identical snapshot → sync → clean-manifest sequence. Unclean kills are covered by WAL replay + torn-tail truncation. |
| D8 | TLS on TCP | None; UDS 0600 preferred, TCP loopback-only and fail-closed on non-loopback binds | The current v1.0 transport policy is explicit: local tooling may use plaintext loopback TCP, while LAN/multi-host TCP is out of scope until a future TLS-bearing transport lands. |
| D9 | Curator models Gemma-2-2B / Qwen-2.5-1.5B / pool via llama.cpp FFI | `CuratorBackend` trait: deterministic backend default; `ExternalGgufBackend` shells out to a pinned, SHA-verified llama-cli with temp 0 + seed | No weights or toolchain in the authoring sandbox. The deterministic backend is *functional* (lexical-centrality extractive skeletons, evidence-tally adjudication), not a stub. GGUF path is config-only (`[curator] external_cmd` + `[curator.pinned]`). Inference failures fall back + `curator.backend_fallback`. |
| D10 | Blob/Timeline/Scratchpad WAL replay | Snapshot-covered; WAL frames written for forensics but only Fact/Supersede/CuratorOutput frames re-applied on recovery | Bounded loss = one group-commit window between snapshots for those cells. Fact state — the governance-critical surface — replays fully. Extend `bootstrap::replay_frame` per cell to close. |
| D11 | Group commit 250 µs / 256 KiB, epoch roll at 1 GiB | Implemented in `persist::wal` per spec constants | Not perf-validated (no runtime). |

## 3. Coverage matrix (spec doc → module(s) → state)

| Doc | Where | State |
|-----|-------|-------|
| Architecture.md | lib.rs, node.rs | ✅ single binary, 25 cells, logical clocks, determinism discipline |
| CellTaxonomy.md | cells/, trinity/, curator/ | ✅ all 25 cell types, stable numeric ids, snapshot/restore each |
| NATIVE_TRINITY.md | trinity/ | ✅ 7 cells, fixed 5-step chain + optional Warden step 6, absorption, reservation-release, fixation N=8 |
| RouterScheduler.md | router/ | ✅ tokens, E1–E4, gap accounting, tier budgets + R1 truncation, built-in views, events w/ ALWAYS_DELIVER, facet gate |
| PersistenceLayer.md | persist/ | ✅ WALF frames + CRC, group commit, epoch roll, CAS aa/bb layout + refcount GC, CoW snapshots with measured/enforced `≤50 ms` pause target, manifest atomic rename, KMS T0–T3 with persisted keyring rotation + custody verification, PrefixCacheStore ViewKey, weight pinning |
| McpProtocol.md | proto/, router/envelope.rs | ✅ u32-LE frames (16 MiB), hello capability bits, 6 verbs, error codes incl. quarantine ids · ⚠️ no TLS (D8) |
| Bootstrap.md | bootstrap/ | ✅ B1 config merge, B2, B3a Trinity-first (fatal), B3b recovery (snapshot + replay + audit verify + weight verify), B5 ×11, B6 ready line · ⚠️ signals (D7) |
| CURATOR_PAIR_PROTOCOL.md | curator/, router/ | ✅ P19 PUBLIC/PRIVATE split w/ token exclusions enforced at the Router, P20 chain on curator writes, disagreement flow, all nine guardrails (quota band, probes ×10 boost, blind re-audit, boundary probes, calibration degrade, weight pinning, prior-blind referee, sanity-not-veto, ledger forensics) |
| LibrarianCell.md | curator/librarian.rs | ✅ skeleton ≤80 tok / supersede-proposal / archive-tag ops, PENDING→Active/Quarantined lifecycle, private facets in CAS · backend per D9 |
| WardenCell.md | curator/warden.rs | ✅ envelope gate (hallucination + drift), independent grounding **or** hash-proof (never bare pass), blind re-audit, flags |
| AdjudicatorCell.md | curator/adjudicator.rs | ✅ policy table (~70–80 % target), seeded pool rotation + per-judge salt, Uncertain→human queue + `resolve`, **structural prior-blindness** (no ledger param; token also excludes `cross_check/**`) |
| CrossCheckLedgerCell.md | curator/ledger.rs | ✅ own WAL stream (FLAG_CROSS_CHECK), W=200 window, >0.99 suspicious / <0.92 miscalibration, batch HMAC every 256 @ T2+, persisted signature sidecar + recovery verification |
| MemoryCells / IndexCells / CoordCells docs | cells/ | ✅ Fact s-p-o w/ supersession, Timeline, Scratchpad TTL, Playbook, Blob→CAS, Cache; HNSW (seeded, rebuild-identical), BM25, Graph, Reranker; AgentRegistry (revocation, escalation), Proposal quorum, Subscription globs |
| DeepSeekOptimization.md | deepseek.rs, router/view.rs | ✅ prefix-stable view layout, FIM wrap, R1 strip, flat lowercase tools manifest |
| ObservabilitySpec.md | obs.rs | ✅ counters/gauges/histograms, structured JSONL log, hash-chained audit log + verify |
| ConformanceSuite.md | tests/conformance.rs | ✅ T1–T8, C1–C10 implemented (see §4) |
| ThreatModel.md | throughout | ✅ mapped: token tamper/expiry/revocation tests, P19 non-disclosure (denial identical for absent vs excluded), quarantine non-drop, collusion tripwires · residual: no rate limiting, no TLS |
| DeploymentGuide.md / OperatorRunbook.md | README, main.rs | ✅ CLI + verbs; systemd/unit files not included |
| GapRegister.md | bootstrap (GAP-0001) | ✅ register/dispatch/fixation/board view |

## 4. Conformance inventory

Trinity: **T1** anchor coverage (every in-source `SPEC-DERIVED-§` marker
resolves) · **T2** decision conflict + respects/supersedes ·
**T3** congruence delta block→accept · **T4** fixation at N+1 ·
**T5** zero budget · **T6** quarantine no-drop + operator reinject
lands · **T7** audit chain verify · **T8** unprovisioned node rejects
(governance precedes the governable).

Curator: **C1** all four private facets denied to the Warden, metric
increments, denial doesn't leak existence · **C2** operator hydrates
rationale · **C3** audits carry independent grounds or hash-proof ·
**C4** 100/100 agreement → suspicious + ×10 probe boost ·
**C5** blind re-audit deterministic + order-insensitive · **C6** pool
rotation seed-exact · **C7** identical dispute, divergent ledger
histories → identical resolution (prior-blindness) · **C8** librarian
writes traverse the Trinity chain (P20) · **C9** fabricated-handle
probe caught + ledgered · **C10** E2E write→curate→audit→Active
skeleton served on recall, cross-node determinism spot-check.

## 5. Known sharp edges

- `handle_gate_dispute` InitiatorUpheld returns `AdjudicationPending`
  with `retry_after=now` and instructs a `semantic_check=false` resubmit
  rather than transparently completing the write — the gate path has
  already unwound its reservation. Deliberate: keeps the write path
  single-pass; revisit if resubmits prove noisy.
- The DeterministicBackend's dead-tie escalation rate (h & 0b111 == 0 ≈
  12.5 % of *dead ties*) yields the spec's ~1–2 % overall human-escalation
  target only because dead ties are rare after the policy table; tune the
  mask if pool composition changes.
- `recover()` replays shard WALs sequentially; frames across shards are
  ordered per-shard, and cross-shard ordering relies on `logical_at`
  monotonicity from the single node clock (true in v0's single-writer
  design).
