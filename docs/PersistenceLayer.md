# PersistenceLayer.md — **UltraCortex** L0 Persistence Layer Specification

**Status:** v1.0 — Normative L0 Specification
**Owner:** Dominic Sarria-Wiley
**Companion documents:** Architecture.md v1.0, RouterScheduler.md v1.0, CellTaxonomy.md v1.0, McpProtocol.md v1.0, NATIVE_TRINITY.md v1.0, DeepSeekOptimization.md v1.0.

---

## §0 — Document Conventions

- **MUST / SHOULD / MAY** follow RFC 2119.
- **SPEC-DERIVED-§N.N** markers reference Architecture.md.
- All bytes little-endian. Payloads **canonical CBOR** (RFC 8949).
- All times logical (u64); wall-clock fields suffixed `*_wall`.

---

## §1 — Mission

The Persistence Layer (L0) is the on-disk substrate. Narrow and absolute:

1. **Durable, append-only** storage for every state-changing message (WAL).
2. **Point-in-time CoW snapshots** per Cell stable region.
3. **Content-addressed blob storage** (CAS).
4. **Manifest** = single source of truth on what is on disk.
5. **KMS abstraction** for tier hierarchy T0–T3.
6. **Bit-deterministic recovery** from any (snapshot, WAL tail) pair.
7. **(NEW v1.0)** **PrefixCacheStore** — content-addressed, replayable index of canonical prefix-stable View serializations for DeepSeek prefix-cache reuse.

L0 holds **no domain logic**. It writes bytes, fsyncs, and reads back identically.

---

## §2 — Component Overview

```
┌─────────────────────────────────────────────────────────────────┐
│                         L0 — PERSISTENCE                        │
│                                                                 │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────────────┐  │
│  │  WAL (per    │  │  Snapshot    │  │  CAS Blob Store      │  │
│  │  shard)      │  │  Store (per  │  │  SHA-256 keyed       │  │
│  │  group-      │  │  Cell)       │  │  refcounted          │  │
│  │  committed   │  │  CoW page-   │  │  mark-and-sweep GC   │  │
│  │  fsync       │  │  level       │  │                      │  │
│  └──────┬───────┘  └──────┬───────┘  └──────────┬───────────┘  │
│         │                 │                     │              │
│  ┌──────▼─────────────────▼─────────────────────▼───────────┐  │
│  │                       MANIFEST                           │  │
│  │  epochs · WAL ranges · schema versions · ns policies ·   │  │
│  │  KMS key IDs · GC watermarks · PrefixCacheStore roots    │  │
│  └──────────────────────────────────────────────────────────┘  │
│                                                                 │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │           KMS Abstraction  (T0–T3)                       │  │
│  └──────────────────────────────────────────────────────────┘  │
│                                                                 │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │       PREFIX CACHE STORE  (NEW v1.0)                     │  │
│  │  content-addressed View bytes · keyed by ViewKey ·       │  │
│  │  size-tiered LRU + epoch GC                              │  │
│  └──────────────────────────────────────────────────────────┘  │
│                                                                 │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │       Flusher Thread (group-commit · checksum · fadvise) │  │
│  └──────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
```

---

## §3 — WAL (Write-Ahead Log)

### §3.1 Layout

- One WAL file per shard: `wal/<shard_id>/<epoch>.wal`.
- New epoch on snapshot boundary or every 1 GiB.
- Frames are appended; never overwritten.

### §3.2 Frame Format

```
+------------------------+------------------------+
| u32 magic = 0x57414C46 ("WALF")                 |
+------------------------+------------------------+
| u32 frame_len          | u64 logical_at         |
+------------------------+------------------------+
| u64 cell_id            | u8 op | u8 schema_ver  |
+------------------------+------------------------+
| u16 flags                                       |
+-------------------------------------------------+
| CBOR payload (canonical, optionally encrypted)  |
+-------------------------------------------------+
| u32 crc32c (covers everything above)            |
+-------------------------------------------------+
```

### §3.3 Group Commit

- Flusher coalesces pending frames every 250 μs or 256 KiB.
- Single `fsync` per group.
- Target fan-in ≥ 16 frames p50.

### §3.4 Trinity Carve-Outs

Trinity Cells (DecisionLedger, SpecAnchor, GapCell, ContractCell, QuarantineCell) WAL through the same per-shard WAL. Every Trinity frame has `flags.audit_chain = true` → AuditSubsystem (ObservabilityAudit.md §5) hash-chains it.

---

## §4 — Snapshots

### §4.1 CoW Page-Level Incremental

- Each Cell's stable region = mmap'd file divided into 64 KiB pages.
- On trigger, dirty pages → new file; clean pages referenced by offset.
- Snapshot pause window ≤ 50 ms target.
- The live checkout measures `pause_us` as the in-memory checkpoint cut, returns `total_us` plus capture/write/manifest breakdown from `ultracortex snapshot`, and increments `snapshot.pause_target_exceeded` if the pause window exceeds `50_000 µs`.
- Representative local proof: `tests/acceptance_bench.rs::snapshot_pause_bench_meets_target` with artifact `docs/benchmarks/snapshot_pause_2026-07-07.json` (`392 µs` p50, `562 µs` p99/max).

### §4.2 Triggers

- WAL ≥ 1 GiB,
- elapsed logical ≥ 5 min,
- explicit `ultracortex snapshot`,
- low disk watermark.

### §4.3 Manifest Update

Every successful snapshot atomically updates Manifest with new epoch, WAL truncation watermark, KMS key ID.

---

## §5 — CAS Blob Store

- Files in `cas/<aa>/<bb>/<sha256>` where `aa`, `bb` = first byte-pairs.
- Refcount in Manifest sidecar.
- Mark-and-sweep GC on Bulk priority; default 6 h logical.
- Encryption: per-namespace key (T2+).

---

## §6 — Manifest

Single canonical CBOR file (`manifest.cbor`), atomically replaced via `rename(2)`. Contains:

- active WAL ranges per shard,
- active snapshot epoch per Cell,
- schema versions per Cell type (from ContractCell),
- namespace policies (durability, encryption, token-budget defaults),
- KMS key IDs,
- GC watermarks,
- **PrefixCacheStore index roots** (§9).

The Manifest is the **only** file the Bootstrap Operator MUST read first.

---

## §7 — KMS Abstraction

```rust
trait KmsProvider {
    fn wrap(&self, key_id: KeyId, plaintext: &[u8]) -> Result<Vec<u8>>;
    fn unwrap(&self, key_id: KeyId, ciphertext: &[u8]) -> Result<Vec<u8>>;
    fn rotate(&self, key_id: KeyId) -> Result<KeyId>;
}
```

| Tier | Meaning | KMS interaction |
|------|---------|-----------------|
| T0   | plaintext disk | none |
| T1   | disk-encryption only | local key file |
| T2   | envelope encryption per namespace | KmsProvider per ns |
| T3   | per-namespace + auditable rotation | persisted local keyring (upgrade seam to external KMS / HSM) |

**[GAP-009]** closed in the current checkout. KMS ops are themselves audited (ObservabilityAudit.md §5).
Current-checkout note: T3 now opens locally, persists custody state in `kms/keyring.cbor`, exposes `ultracortex kms status` / `ultracortex kms rotate [--emergency]`, and retains prior key versions so pre-rotation payloads and batch signatures remain verifiable after a roll.

---

## §8 — Recovery

Bit-deterministic sequence:

1. Read `manifest.cbor`.
2. For each Cell, mmap its latest snapshot at recorded epoch.
3. For each shard, open its WAL at manifest's truncation watermark.
4. Replay WAL frames in order, calling `Cell::on_update` (pre-validation **bypassed**: replay assumes chain already passed at original write time; quarantine outcomes are themselves WAL frames).
5. Reconstruct Trinity Cells from their WAL frames; audit chain MUST verify.
6. Open Router/Scheduler with restored state.
7. Open MCP surface.
8. Emit `bootstrap.recovery_complete` audit event.

Recovery successful iff post-replay hash of every Cell's stable region equals the hash recorded at last clean shutdown.
Current-checkout note: boot-time verification now covers both the audit hash chain and completed CrossCheck batch signatures; recovery refuses MCP open if either check fails.

---

## §9 — PrefixCacheStore (NEW v1.0 — DeepSeek Optimization)

What makes DeepSeek prefix-cache reuse possible without recomputation.

### §9.1 Goal

For every `view` request (McpProtocol.md §4.5), canonical prefix-stable bytes stored content-addressed, indexed by `ViewKey`. Re-request with same key → served from disk in microseconds.

### §9.2 Layout

```
cache/views/<view_id>/<ns>/<version>/<params_hash>.cbor.lz4
cache/index/view_keys.btree
```

`ViewKey = (view_id, namespace_id, view_version, params_canonical_hash)`. Bytes are lz4-compressed canonical CBOR. Index = on-disk BTree (sled or fjall).

### §9.3 Lifecycle

- **Write:** Router assembles view → writes canonical bytes atomically.
- **Read:** ViewKey lookup → `Hit(bytes)` or `Miss`.
- **Invalidate:** `node.superseded` for any referenced node → tombstone affected entries. Readers race tombstone read.

### §9.4 Eviction Policy

Size-tiered LRU per tier:
- L0 (skeletons-only): up to 256 MiB,
- L1 (skeletons+handles): up to 1 GiB,
- L2 (skeletons+bodies): up to 4 GiB.

Epoch-based GC sweeps tombstones + superseded entries.

**[GAP-NT-014]** final eviction policy.

### §9.5 Determinism

Given identical WAL replay + identical view request, bytes MUST be byte-identical to cached entry — otherwise cache invalid and purged.

---

## §10 — Trinity Persistence

| Cell | Persistence | Recovery property |
|------|-------------|-------------------|
| SpecAnchorCell    | strict; WAL + snapshot | every anchor reconstructed |
| DecisionLedgerCell| strict; append-only WAL; snapshot is CoW shadow preserving full log | every Decision replayable in WAL order |
| CongruenceCell    | lazy; regenerated from FactCell + SpecAnchorCell on boot | matrix recomputed O(N) |
| GapCell           | strict; WAL + snapshot | counters + last-transition restored |
| QuarantineCell    | strict; WAL + retention-horizon GC | resolved records retained for `1_000_000` logical ticks; pending records never pruned |
| WorkBudgetCell    | lazy; in-memory envelopes; rebuilt from in-flight task list on boot | active envelopes resumed |
| ContractCell      | strict; WAL + snapshot; every active version mmapped | all live contracts available |

---

## §11 — GAPs

| ID | Description |
|----|-------------|
| GAP-002    | Final allocator selection |

---

## §12 — Congruence Contract

Congruent with: Architecture.md (§4.3, §8, §14, §15), CellTaxonomy.md (per-cell persistence), RouterScheduler.md (PrefixCacheStore reads/writes), McpProtocol.md (view bytes format), NATIVE_TRINITY.md, DeepSeekOptimization.md.

_End of PersistenceLayer.md v1.0 (UltraCortex)._


---

# 🆙 UltraCortex v1.0 Delta — Curator Weight Pinning, CrossCheckLedger WAL Stream

The HyperCortex L0 content above remains normative. UltraCortex v1.0 adds three new persistence concerns for the Curator Pair (P19, P20).

## §A.1 Weight Store (NEW)

Curator weight files are stored on disk and **pinned by SHA-256 in `ContractCell`**.

```
weights/
  librarian/<sha256>.gguf              # Gemma 2 2B Instruct, LibrarianCell
  warden/<sha256>.gguf                 # Qwen 2.5 Coder 1.5B, WardenCell
  phi-3.5-mini-q4_k_m/<sha256>.gguf     # Adjudicator pool
  llama-3.2-3b-q4_k_m/<sha256>.gguf     # Adjudicator pool
  smollm-2-1.7b-q4_k_m/<sha256>.gguf    # Adjudicator pool
```

**Invariants:**
- W-1: Every file's actual SHA-256 MUST equal its filename SHA-256 prefix.
- W-2: Bootstrap fails if any pinned weight file fails verification (BootstrapOperator §B3.7–9).
- W-3: Weight swaps require a Decision record in `DecisionLedgerCell` — never a silent merge.
- W-4: Manifest stores the active `(model_id, sha256)` tuple per Curator Cell.

## §A.2 CrossCheckLedger WAL Stream (NEW)

`CrossCheckLedgerCell` has its **own dedicated WAL stream** for forensic durability:

```
wal/cross_check/<epoch>.wal
```

- Same group-commit fsync semantics as Trinity WAL.
- Frames carry `flags.cross_check = true` → AuditSubsystem hash-chains them (ObservabilityAudit §5.2).
- Current checkout: batch-HMAC-signed at T2+ in 256-record batches; at T3 the signing key id is persisted alongside the batch HMAC in `wal/cross_check/batch-signatures.cbor` and replay-verified before service resumes.
- Retention: v1.0 keeps CrossCheck records indefinitely in the live snapshot/WAL state; there is no age-based pruning path for this ledger.

## §A.3 Curator KV Caches: RAM-Only (NORMATIVE)

Curator Cell KV caches are **ephemeral, RAM-only, NEVER persisted to disk**.

**Rationale:**
- **Privacy** — rationale tokens never touch persistent storage.
- **Determinism** — on boot, KV starts empty; pinned weights + pinned seeds + canonical input → reproducible inference.
- **Recovery** — boots are bit-deterministic because there's no KV-cache state to reconcile.

**Invariant K-1:** Snapshot writers MUST NOT include Curator KV cache pages.
v1.0 now fixes the operator planning budgets behind `[curator].kv_budget_profile`:
- `small` = Librarian `256 MiB`, Warden `256 MiB`, Adjudicator `128 MiB` (`640 MiB` total)
- `reference` = Librarian `384 MiB`, Warden `384 MiB`, Adjudicator `256 MiB` (`1_024 MiB` total, default)
- `heavy` = Librarian `768 MiB`, Warden `768 MiB`, Adjudicator `512 MiB` (`2_048 MiB` total)
Bootstrap validates the profile name and `curator status` surfaces the derived per-Cell MiB values.

## §A.4 Manifest Updates (NEW Fields)

```toml
[curator]
librarian_model_sha256 = "..."
warden_model_sha256    = "..."
adjudicator_pool_sha256 = ["...", "...", "..."]
cross_check_ledger_watermark = <u64 logical>
weight_store_gc_watermark    = <u64 logical>
```

## §A.5 Recovery Sequence (Updated Step)

The bit-deterministic recovery sequence (PersistenceLayer §8) gets one new step:

> **Step 4a (NEW):** Verify all Curator weight-file SHA-256s against ContractCell pins. Bootstrap fails if any mismatch.
>
> **Step 4b (NEW):** Initialize Curator Cells with empty KV caches. Weight files mmap'd.

## §A.6 KV Budget Profiles (Closed GAP-CU-010)

The unresolved budget-policy gap is now closed by the supported `small` / `reference` / `heavy` profiles above. The current checkout still treats these as operator planning limits rather than sampled live cache telemetry. Curator default-model selection is closed: production pins distinct Gemma and Qwen GGUF slots, while development mode is explicit.

## §A.7 Congruence Contract (Updated)

Congruent with: Architecture-UltraCortex.md (§8, §16.5), **CURATOR_PAIR_PROTOCOL.md** (weight-pinning enforcement of P20), **LibrarianCell.md**, **WardenCell.md**, **AdjudicatorCell.md**, **CrossCheckLedgerCell.md** (WAL stream), BootstrapOperator.md (recovery + self-test).

_End of UltraCortex v1.0 Delta._
