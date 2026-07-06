# BootstrapOperator.md — **UltraCortex** Bootstrap Operator Specification

**Status:** v1.0 — Normative Single-Binary Lifecycle Specification (supersedes HyperCortex v1.0; BootstrapOperator.md v0.1)
**Owner:** Dominic Sarria-Wiley
**Companion documents:** Architecture.md v1.0, CellTaxonomy.md v1.0, RouterScheduler.md v1.0, PersistenceLayer.md v1.0, McpProtocol.md v1.0, NATIVE_TRINITY.md v1.0, ObservabilityAudit.md v1.0.

---

## §0 — Document Conventions

- **MUST / SHOULD / MAY** follow RFC 2119.
- **SPEC-DERIVED-§N.N** references Architecture.md.

---

## §1 — Mission

The Bootstrap Operator is the **single-binary lifecycle controller** of UltraCortex (Architecture.md §16, P9). It is the only entry point. There is no daemon path that bypasses it; this guarantees every running node has passed self-test.

The Operator owns:

1. Configuration loading.
2. L0 directory layout materialization.
3. L1 shard provisioning.
4. **Trinity shard bootstrap** (the four Phase 1A Trinity Cells must be live before any other Cell accepts traffic).
5. KMS key load or generate.
6. WAL replay.
7. L2 Router and L3 MCP surface open.
8. Self-test (full round-trip including DeepSeek view emission and a quarantine).
9. `ready` signal on stdout.

---

## §2 — Configuration

### §2.1 `ultracortex.toml`

```toml
[node]
node_id = "node-01"
data_dir = "/var/lib/ultracortex"
log_level = "info"

[router]
uds_path = "/run/ultracortex/ultracortex.sock"
tcp_addr = "127.0.0.1:7741"
mtls_required = true

[shards]
count = 6                       # default = physical cores − 2
trinity_topology = "dedicated"  # "dedicated" | "co-tenant-shard-0"

[persistence]
wal_group_commit_us = 250
snapshot_interval_logical = 300000   # 5 min logical
encryption_tier = "T2"

[kms]
provider = "file"                # "file" | "os-keychain" | "aws-kms" | "vault"
key_path = "/var/lib/ultracortex/keys/master.bin"

[budgets.default]
tokens_per_task = 8192
deadline_logical = 60000
retries_allowed = 3

[deepseek]
prefix_cache_enabled = true
fim_default = true
r1_strip_default = true
```

### §2.2 CLI Overrides

Every TOML field is overridable via CLI flag in `--snake.case=value` form. CLI > env > TOML > default.

---

## §3 — Lifecycle Phases

### Phase B1 — Config Resolve

1. Load `ultracortex.toml` (or `--config <path>`).
2. Merge env overrides.
3. Merge CLI overrides.
4. Validate against the canonical config schema (registered with ContractCell at first run).
5. Print resolved config in `--dry-run`; exit.

### Phase B2 — L0 Materialization

1. Create `<data_dir>/{wal,snapshots,cas,cache/views,cache/index,keys,manifest.cbor}` if absent.
2. Lock `<data_dir>/.lock` (single-writer guarantee).
3. Read `manifest.cbor`. If absent → fresh-node path (B3a). Else → recovery path (B3b).

### Phase B3a — Fresh-Node Path

1. Generate node UUID + initial KMS key (per `[kms]` config).
2. Initialize empty Manifest with default policies.
3. Initialize empty WAL (epoch 0).
4. **Bootstrap Trinity shard first**:
   - provision `ContractCell`,
   - provision `SpecAnchorCell`,
   - provision `DecisionLedgerCell`,
   - provision `CongruenceCell`,
   - provision `GapCell`.
5. Register the canonical config schema with `ContractCell`.
6. Register the initial set of `SPEC-DERIVED-§N.N` anchors from the docs corpus.
7. Provision Memory/Index/Coordination shards.
8. Emit audit event `bootstrap.fresh_node`.

### Phase B3b — Recovery Path

Per PersistenceLayer.md §8:

1. Open Trinity shard first, replay its WAL.
2. Verify audit chain integrity. Abort if invalid.
3. Open each Cell shard, mmap latest snapshot, replay WAL tail.
4. Reconstitute live state.
5. Reconstitute in-flight WorkBudget envelopes from the task index (lazy persistence).
6. Emit audit event `bootstrap.recovery_complete`.

### Phase B4 — Router & MCP Surface

1. Open Router on configured UDS + TCP endpoints.
2. Open subscription dispatcher.
3. Register tools manifest endpoint (McpProtocol.md §6.3).
4. **DO NOT** accept external traffic yet.

### Phase B5 — Self-Test

The Operator runs a synthetic agent against the local MCP surface:

1. **Recall round-trip**: insert a known FactNode, recall by query, assert handle and skeleton bytes.
2. **Write round-trip**: write a node with a valid SpecAnchor; verify WAL frame + Trinity audit chain.
3. **Supersede round-trip**: supersede the node; verify DecisionLedger entry.
4. **Quarantine round-trip**: send a write with a missing anchor; verify `Quarantined { quarantine_id }` response and a QuarantineRecord in the cell.
5. **Budget exceeded round-trip**: send a write with `tokens_remaining = 0`; verify `BudgetExceeded` response.
6. **DeepSeek view emission**: request a view with `Formatting::DeepSeekFim`; verify FIM framing is present and the view is in the PrefixCacheStore.
7. **Prefix-cache hit**: re-issue the same view request; verify `cache_hit: true` and latency ≤ 20 μs p99.

Failure of any step → audit `bootstrap.self_test_failed` and exit non-zero. Success → audit `bootstrap.self_test_passed`.

### Phase B6 — Ready

1. Print `ready node_id=<...> proto_version=1` on stdout.
2. Begin accepting external traffic.
3. Begin emitting subscriptions to escalation-list subscribers.

---

## §4 — Shutdown

A clean shutdown (SIGTERM):

1. Stop accepting new envelopes (return `RateLimited { retry_after: ∞ }`).
2. Drain in-flight envelopes up to `shutdown_drain_deadline` (default 30 s wall-clock).
3. Trigger one snapshot per Cell.
4. Flush WAL with final fsync.
5. Update Manifest with clean-shutdown marker + per-Cell state hashes.
6. Emit audit event `bootstrap.clean_shutdown`.
7. Exit zero.

A crash (SIGKILL or panic): the next boot follows recovery path B3b.

---

## §5 — Admin Commands

| Command | Action |
|---------|--------|
| `ultracortex snapshot --cell <id>` | force snapshot of a Cell |
| `ultracortex quarantine list` | list pending QuarantineRecords |
| `ultracortex quarantine reinject <id>` | re-dispatch a quarantined envelope |
| `ultracortex gap list` | list GapRecords by status |
| `ultracortex audit verify` | full audit-chain verification |
| `ultracortex congruence audit` | force CongruenceCell recompute |
| `ultracortex contract list` | list registered contracts + versions |

All admin commands are themselves audited.

---

## §6 — Trinity Shard Bootstrap Ordering (Critical)

Phase B3 MUST provision Trinity Cells **before** any non-Trinity Cell, in this order:

1. `ContractCell` — must be alive so other Cells can register their schemas.
2. `SpecAnchorCell` — must be alive so writes can verify anchors.
3. `DecisionLedgerCell` — must be alive so decisions can be appended.
4. `GapCell` — must be alive so envelopes carrying `gap_ref` can register.
5. `CongruenceCell` — must be alive to subscribe to spec-node changes.
6. (Phase 1B) `QuarantineCell`, `WorkBudgetCell` — must be alive before opening MCP surface.

Violation of this ordering is a fatal config error.

---

## §7 — GAPs

| ID | Description |
|----|-------------|
| GAP-NT-001 | Trinity shard topology default (dedicated vs co-tenant-shard-0) |
| GAP-NT-009 | Pre-validation chain ordering proof |

---

## §8 — Congruence Contract

Must remain congruent with: Architecture.md (§16), PersistenceLayer.md (§8 recovery), RouterScheduler.md (Trinity shard placement), NATIVE_TRINITY.md (Cell provisioning order), ObservabilityAudit.md (audit events emitted by the Operator).

_End of BootstrapOperator.md v1.0 (UltraCortex)._


---

# 🆙 UltraCortex v1.0 Delta — Curator Shard Provisioning, Self-Test Extensions

The HyperCortex Bootstrap Operator content above remains normative. UltraCortex v1.0 extends Phase B3 (provisioning) and Phase B5 (self-test) to cover the Curator Pair.

## §A.1 New `[curator]` Config Section

```toml
[curator]
librarian_model_sha256 = "..."   # pinned Gemma 2 2B Q4_K_M
warden_model_sha256    = "..."   # pinned Qwen 2.5 Coder 1.5B Q4_K_M
adjudicator_pool = [
    "...",  # Phi-3.5 Mini sha256
    "...",  # Llama 3.2 3B sha256
    "...",  # SmolLM-2 1.7B sha256
]
disagreement_quota_min = 0.92
disagreement_quota_max = 0.97
adversarial_probe_rate = 0.001
blind_reaudit_sample   = 0.01
curator_topology       = "dedicated"   # "dedicated" | "co-tenant-shard-0"
```

## §A.2 Phase B3 — Provisioning Order (Updated)

Trinity FIRST, then Curator, then everything else. **Violation of ordering is a fatal config error.**

```
B3.1  Provision ContractCell                     (Trinity, foundational)
B3.2  Provision SpecAnchorCell                   (Trinity)
B3.3  Provision DecisionLedgerCell               (Trinity)
B3.4  Provision GapCell                          (Trinity)
B3.5  Provision CongruenceCell                   (Trinity)
B3.6  Provision QuarantineCell, WorkBudgetCell   (Trinity)
B3.7  [NEW] Provision LibrarianCell              — mmap weights, verify SHA-256
B3.8  [NEW] Provision WardenCell                 — mmap weights, verify SHA-256,
                                                    verify DIFFERENT model family from Librarian
B3.9  [NEW] Provision AdjudicatorCell            — mmap pool, verify all SHA-256s,
                                                    compile deterministic policy table
B3.10 [NEW] Provision CrossCheckLedgerCell       — open dedicated WAL stream
B3.11 Provision Memory/Index/Coordination/Service shards
```

## §A.3 Phase B5 — Self-Test (Extended)

The Operator runs synthetic agent tests against the local MCP surface. UltraCortex adds 4 new tests (8–11):

```
B5.1  Recall round-trip
B5.2  Write round-trip (verify WAL + Trinity audit chain)
B5.3  Supersede round-trip
B5.4  Quarantine round-trip (missing anchor → Quarantined response)
B5.5  Budget-exceeded round-trip
B5.6  DeepSeek view emission (FIM framing + PrefixCacheStore)
B5.7  Prefix-cache hit re-emission
B5.8  [NEW] Librarian skeleton round-trip
        synthetic node.written → Librarian produces PUBLIC skeleton →
        Warden audits → Decision recorded
B5.9  [NEW] Warden flag round-trip
        synthetic envelope with hallucinated handle + flags.semantic_check=true →
        Warden flags → Librarian sanity-checks → quarantine
B5.10 [NEW] Adjudicator disagreement round-trip
        synthetic case where Librarian/Warden disagree → Adjudicator resolves
        deterministically → Decision recorded → CrossCheckRecord written
B5.11 [NEW] PUBLIC/PRIVATE boundary test
        agent cap-token attempts hydrate(librarian_rationale_handle) →
        PermissionDenied + curator.rationale_access_denied event fires
```

**Failure of ANY step → audit `bootstrap.self_test_failed` + exit non-zero.**

## §A.4 New Admin Commands

| Command | Action |
|---|---|
| `ultracortex curator status` | agreement rate, calibration band, degraded-mode status |
| `ultracortex curator probe-now` | manually inject adversarial probe |
| `ultracortex cross-check tail` | recent CrossCheckRecords (paged) |
| `ultracortex adjudicator stats` | deterministic vs LLM vs human resolution split |
| `ultracortex curator verify-weights` | re-verify all pinned SHA-256s against on-disk files |

All admin commands are themselves audited.

## §A.5 Shutdown (Updated)

Clean shutdown now includes:
- Snapshot every Trinity Cell.
- **(NEW)** Verify Curator KV cache erasure — KV pages MUST NOT survive to disk.
- **(NEW)** Final CrossCheckLedger WAL fsync + signature batch flush.
- Audit `bootstrap.clean_shutdown` with per-Cell state hashes (including Curator weight pins).

## §A.6 New GAPs (Bootstrap-scoped)

| ID | Description |
|---|---|
| GAP-CU-001 | Librarian default model (Gemma 2 2B vs Gemma 3) |
| GAP-CU-002 | Warden default model (Qwen 2.5 Coder 1.5B vs alternates) |
| GAP-CU-013 | Curator shard topology for small deployments |

## §A.7 Congruence Contract (Updated)

Congruent with: Architecture-UltraCortex.md (§19), PersistenceLayer.md (recovery + weight pinning), RouterScheduler.md, NATIVE_TRINITY.md (Trinity boot order), **CURATOR_PAIR_PROTOCOL.md**, **LibrarianCell.md**, **WardenCell.md**, **AdjudicatorCell.md**, **CrossCheckLedgerCell.md**, ObservabilityAudit.md (`curator.rationale_access_denied` must be non-zero by end of B5).

_End of UltraCortex v1.0 Delta._
