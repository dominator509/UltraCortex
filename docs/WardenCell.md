# WardenCell.md — **UltraCortex** Drift/Hallucination Warden

**Status:** v1.0 Normative | **Owner:** Dominic Sarria-Wiley | **Phase:** 1G

## §0 Conventions
RFC 2119. SPEC-DERIVED-§N.N. [GAP-CU-NNN]. Subject to Trinity governance per P20.

## §1 Mission
The Drift/Hallucination Warden is an **in-substrate LLM** that judges:
- agent envelopes for **semantic drift** from canonical substrate;
- agent envelopes for **hallucinated handles or facts**;
- LibrarianCell PUBLIC outputs (audits-the-Librarian flow per CURATOR_PAIR_PROTOCOL.md §1.2).

Opt-in sync on `flags.semantic_check`. Auto-sync on `severity=P0`. In the
current single-binary checkout, Librarian outputs are audited synchronously in
the Router write path before the output is acknowledged or activated. Subject
to LibrarianCell sanity-check (reverse flow, escalation power only).

## §2 Model Selection
**Default:** Qwen 2.5 Coder 1.5B Q4_K_M.
- ~1.2 GiB Q4 RAM, ~140 ms p50 CPU inference (AVX2/NEON, 4-core).
- Apache 2.0 license. Strong code/agent-reasoning per param.
- **MUST be a different model family than Librarian.** (CURATOR_PAIR_PROTOCOL.md §5.2)
- Pinned by SHA-256 in `ContractCell`. Swap = Decision.

**Alternates:** Phi-3.5 Mini (heavier reasoning), Qwen 2.5 Coder 3B (larger deployments).
Production configuration pins Qwen 2.5 Coder 1.5B Q4_K_M and injects its
backend into `WardenCell`; the family must differ from the Librarian. Missing
local weights or runner fail closed, while `--dry-run` uses explicit
development mode.

## §3 State

```rust
pub struct WardenState {
    model_handle:     ModelHandle,        // mmap'd weights, SHA-256 pinned
    kv_cache:         KvCacheArena,       // RAM-only, NEVER persisted
    pinned_seed:      u64,
    pinned_prompt:    SystemPromptHandle,
    mode:             WardenMode,         // EnvelopeJudge | LibrarianAudit
    blind_reaudit_q:  VecDeque<ReauditJob>,
}
```

## §4 Invocation Paths
- **Opt-in sync** via `flags.semantic_check = true` on the envelope.
- **Auto-sync** on `severity = P0` regardless of `flags.semantic_check`.
- **Synchronous** audit of every LibrarianCell PUBLIC output (lands as
  `RoutineAudit` in CrossCheckLedger before the write response completes).
- **Blind re-audit** triggered by deterministic schedule (1% sample, clean KV cache).

## §5 Output Schema
Per CURATOR_PAIR_PROTOCOL.md §3.1.

```rust
pub enum WardenJudgment {
    Pass,
    FlagDrift { drifted_from: Vec<Handle> },
    FlagHallucination { unknown_handles: Vec<Handle> },
}

// PUBLIC: judgment + grounded_in + confidence_band + independent_handle bool
// PRIVATE: rationale + considered_alts + confidence_precise + reasoning_trace
```

`independent_handle: true` MUST hold OR `independent_grounding_matched: true` with content-hash proof (CURATOR_PAIR_PROTOCOL.md §3.3).

## §6 Grounding Whitelist
The Warden MAY read:
- ✅ FactCell (canonical facts)
- ✅ DecisionLedgerCell (active Decisions)
- ✅ ContractCell schemas
- ✅ LibrarianCell **PUBLIC outputs only** (capability-scoped — rationale blocked)
- ✅ SpecAnchorCell (anchor coverage check)

The Warden MUST NOT read:
- ❌ Librarian rationales / reasoning traces / precise confidence (PRIVATE blocked by negation glob)
- ❌ Agent prompt / context
- ❌ Its own prior judgments (prevents echo chambers across audits)
- ❌ Librarian timing data (`logical_at` ordering) — CURATOR_PAIR_PROTOCOL.md §5.7

## §7 Audit-the-Librarian Flow
On every Librarian PUBLIC output landing as PENDING:
1. Router creates an isolated audit view and invokes Warden before returning
   the write response.
2. Warden independently re-derives grounding from substrate (NOT from Librarian's `grounded_in`).
3. Warden emits judgment:
   - **Agree** → Decision recorded; pending output becomes Active.
   - **DisagreeSpecific** → output Quarantined with grounded counter-rationale.
   - **Uncertain** → escalate to AdjudicatorCell.
4. CrossCheckRecord written.

## §8 Sanity-Check-the-Warden Reverse Flow
On every Warden flag (`FlagDrift` | `FlagHallucination`):
1. Pending-quarantine state.
2. Librarian invoked to sanity-check Warden's grounding:
   - **Agree** → real quarantine (QuarantineCell.absorb).
   - **Disagree** → escalate to AdjudicatorCell.
   - **Uncertain** → escalate to AdjudicatorCell.

**Librarian gets escalation power, not veto.** Cannot directly unblock; only escalate.

## §9 Asymmetric Visibility + Determinism
- `temperature = 0`, greedy decode, pinned seed.
- PUBLIC/PRIVATE split enforced by capability tokens.
- Replays bit-deterministic.

## §10 Performance Targets
| Op | p50 | p99 |
|---|---|---|
| Envelope judgment (CPU AVX2) | ~140 ms | ~350 ms |
| Librarian audit (synchronous, write-path) | ~75 ms/job effective | ~150 ms/job |
| Blind re-audit | ~140 ms | ~350 ms |

## §11 GAPs
No open Warden-specific implementation gap remains. Production `WardenCell`
receives the SHA-verified Qwen backend from bootstrap and uses it for the
semantic audit pass after fail-closed structural grounding checks. Cross-cutting
Curator rows are tracked in `docs/HANDOFF.md`; the remaining production-model
and runtime evidence gates are listed in `RELEASE_AUDIT_REMEDIATION.md`.

## §12 Congruence Contract
Congruent with: Architecture.md (§16), CURATOR_PAIR_PROTOCOL.md, LibrarianCell.md, AdjudicatorCell.md, CrossCheckLedgerCell.md, RouterScheduler.md (`flags.semantic_check` + auto-P0 + escalation path), McpProtocol.md (`SemanticDrift`/`HallucinationDetected` error codes), ObservabilityAudit.md (warden.* metrics).

_End of WardenCell.md v1.0._
