//! Node — the single-process state container (Architecture.md §2: "one
//! binary, one data directory, many cells").
//!
//! Owns every cell instance, the Trinity, the three curators, the
//! CrossCheckLedger, guardrail schedulers, persistence handles, metrics,
//! the audit chain, and the event bus. The Router (`crate::router`)
//! operates on `&Node`; all mutation goes through the interior Mutexes.
//!
//! LOCK ORDER (deadlock discipline): a thread may hold at most one group
//! lock at a time, with ONE sanctioned nesting: a curator lock
//! (librarian/warden/adjudicator) may acquire cells-group locks through
//! [`SubstrateView`] calls. The reverse (holding a cells lock while taking
//! a curator lock) is forbidden. Trinity, ledger, events, and guardrails
//! locks never nest with anything.

use crate::cells::coord::{AgentRegistryCell, ProposalCell, SubscriptionCell};
use crate::cells::index::{Bm25Cell, GraphCell, RerankerCell, VectorCell};
use crate::cells::memory::{
    BlobCell, CacheCell, FactCell, PlaybookCell, ScratchpadCell, TimelineCell,
};
use crate::cells::CatalogCell;
use crate::core::cbor::Cbor;
use crate::core::crypto::sha256;
use crate::core::ulid::DetRng;
use crate::core::{fnv1a64, LogicalClock, ShardTopology, UcError, UcResult};
use crate::curator::adjudicator::AdjudicatorCell;
use crate::curator::guardrails::{BlindReauditScheduler, CalibrationTracker, ProbeScheduler};
use crate::curator::ledger::CrossCheckLedgerCell;
use crate::curator::librarian::LibrarianCell;
use crate::curator::warden::WardenCell;
use crate::curator::{
    CuratorBackend, CuratorConfig, DeterministicBackend, ExternalGgufBackend, SubstrateView,
};
use crate::obs::{AuditChain, Logger, Metrics, OtlpConfig, OtlpExporter};
use crate::persist::wal::{
    payload_nonce, payload_purpose, WalFrame, WalPos, WalWriter, FLAG_CROSS_CHECK,
};
use crate::persist::{CasStore, EncryptionTier, Kms, Manifest, PrefixCacheStore, SnapshotStore};
use crate::router::captoken::{HmacSigner, Signer};
use crate::router::events::EventBus;
use crate::trinity::cells::{
    CongruenceCell, ContractCell, DecisionLedgerCell, GapCell, QuarantineCell, SpecAnchorCell,
    WorkBudgetCell,
};
use crate::trinity::Trinity;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

// Stable numeric cell ids (WAL frames key on these; renumbering breaks
// replay — treat as append-only).
pub mod ids {
    use crate::core::CellId;
    pub const CATALOG: CellId = CellId(1);
    pub const FACT: CellId = CellId(2);
    pub const TIMELINE: CellId = CellId(3);
    pub const SCRATCHPAD: CellId = CellId(4);
    pub const PLAYBOOK: CellId = CellId(5);
    pub const VECTOR: CellId = CellId(6);
    pub const BM25: CellId = CellId(7);
    pub const GRAPH: CellId = CellId(8);
    pub const RERANKER: CellId = CellId(9);
    pub const BLOB: CellId = CellId(10);
    pub const AGENT_REGISTRY: CellId = CellId(11);
    pub const PROPOSAL: CellId = CellId(12);
    pub const SUBSCRIPTION: CellId = CellId(13);
    pub const CACHE: CellId = CellId(14);
    pub const CONTRACT: CellId = CellId(20);
    pub const SPEC_ANCHOR: CellId = CellId(21);
    pub const DECISION_LEDGER: CellId = CellId(22);
    pub const WORK_BUDGET: CellId = CellId(23);
    pub const CONGRUENCE: CellId = CellId(24);
    pub const GAP: CellId = CellId(25);
    pub const QUARANTINE: CellId = CellId(26);
    pub const LIBRARIAN: CellId = CellId(30);
    pub const WARDEN: CellId = CellId(31);
    pub const ADJUDICATOR: CellId = CellId(32);
    pub const CROSS_CHECK: CellId = CellId(33);
}

pub const SNAPSHOT_PAUSE_TARGET_US: u64 = 50_000;
/// Five logical minutes, expressed in the persistence layer's logical-clock
/// units. The logical clock is deterministic; operators that need a
/// wall-clock SLA should use the snapshot metrics separately.
pub const AUTO_SNAPSHOT_LOGICAL_INTERVAL: u64 = 5 * 60 * 1_000;
pub const AUTO_SNAPSHOT_WAL_BYTES: u64 = 1 << 30;

fn configured_curator_backend(
    data_dir: &Path,
    slot: &str,
    model_name: &str,
    sha: Option<&String>,
    external_cmd: Option<&String>,
    strict: bool,
    metrics: &Arc<Metrics>,
) -> UcResult<Arc<dyn CuratorBackend>> {
    match (external_cmd, sha) {
        (Some(cmd), Some(sha)) => Ok(Arc::new(ExternalGgufBackend::new_for_slot(
            data_dir,
            slot,
            model_name,
            sha,
            cmd,
            Some(metrics.clone()),
        )?)),
        _ if strict => Err(UcError::unsupported(format!(
            "curator {slot} default requires pinned {model_name} weights and a local GGUF runner"
        ))),
        _ => Ok(Arc::new(DeterministicBackend)),
    }
}

#[derive(Clone, Debug)]
pub struct SnapshotOutcome {
    pub name: String,
    pub cells: usize,
    pub pause_us: u64,
    pub total_us: u64,
    pub capture_us: u64,
    pub hash_us: u64,
    pub write_us: u64,
    pub manifest_us: u64,
    pub within_target: bool,
}

/// Memory / index / coordination cells behind one lock group each.
pub struct Cells {
    pub catalog: Mutex<CatalogCell>,
    pub fact: Mutex<FactCell>,
    pub timeline: Mutex<TimelineCell>,
    pub scratchpad: Mutex<ScratchpadCell>,
    pub playbook: Mutex<PlaybookCell>,
    pub vector: Mutex<VectorCell>,
    pub bm25: Mutex<Bm25Cell>,
    pub graph: Mutex<GraphCell>,
    pub reranker: Mutex<RerankerCell>,
    pub blob: Mutex<BlobCell>,
    pub agent_registry: Mutex<AgentRegistryCell>,
    pub proposal: Mutex<ProposalCell>,
    pub subscription: Mutex<SubscriptionCell>,
    pub cache: Mutex<CacheCell>,
}

pub struct Curators {
    pub librarian: Mutex<LibrarianCell>,
    pub warden: Mutex<WardenCell>,
    pub adjudicator: Mutex<AdjudicatorCell>,
}

pub struct Guardrails {
    pub probe: Mutex<ProbeScheduler>,
    pub blind: Mutex<BlindReauditScheduler>,
    pub calibration: Mutex<CalibrationTracker>,
}

pub struct Node {
    pub node_id: String,
    pub data_dir: PathBuf,
    pub clock: LogicalClock,
    pub boot_seed: u64,
    pub shard_count: u64,
    pub trinity_topology: ShardTopology,
    pub view_version: Mutex<u64>,

    pub cells: Cells,
    pub trinity: Mutex<Trinity>,
    pub curators: Curators,
    pub cross_check: Mutex<CrossCheckLedgerCell>,
    pub guardrails: Guardrails,
    pub curator_cfg: CuratorConfig,
    /// PUBLIC bodies of curator artifacts (librarian outputs, warden
    /// judgments, adjudications), maintained by the Router. SubstrateView
    /// reads THIS instead of locking curator cells — the one sanctioned
    /// lock nesting stays strictly curator→cells (see LOCK ORDER above).
    pub curator_public_index: Mutex<BTreeMap<String, String>>,

    // Persistence.
    pub shard_wals: Vec<Arc<WalWriter>>,
    pub cross_check_wal: Arc<WalWriter>,
    pub kms: Arc<Kms>,
    pub cas: Arc<CasStore>,
    pub snapshots: SnapshotStore,
    pub view_cache: PrefixCacheStore,
    pub manifest: Mutex<Manifest>,

    // Observability + wiring.
    pub metrics: Arc<Metrics>,
    pub otlp: OtlpExporter,
    pub logger: Logger,
    pub audit: Mutex<AuditChain>,
    pub events: Mutex<EventBus>,
    pub signer: Arc<dyn Signer>,

    /// Serializes durable state transitions with cross-cell snapshot cuts.
    /// Individual cell locks remain the ownership boundary; this lock makes
    /// a multi-cell operation and a full snapshot one logical instant.
    mutation_barrier: Mutex<()>,
    auto_snapshot_lock: Mutex<()>,
    durable_wal_bytes: Mutex<u64>,
    last_snapshot_logical_at: Mutex<u64>,

    pub shutting_down: AtomicBool,
}

impl Node {
    /// Construct with everything empty; the Bootstrap Operator (B1–B6)
    /// provisions cells in Trinity-first order and replays state.
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        node_id: &str,
        data_dir: &Path,
        shard_count: u64,
        tier: EncryptionTier,
        boot_seed: u64,
        trinity_topology: ShardTopology,
        curator_cfg: CuratorConfig,
        embedder_dim: usize,
    ) -> UcResult<Node> {
        std::fs::create_dir_all(data_dir).map_err(UcError::from)?;
        let metrics = Arc::new(Metrics::new());
        curator_cfg.validate_model_pair()?;
        let kms = Arc::new(Kms::open(data_dir, tier)?);
        let cas = Arc::new(CasStore::open_with_kms(data_dir, kms.clone())?);
        let snapshots = SnapshotStore::open_with_kms(data_dir, kms.clone())?;
        let view_cache = PrefixCacheStore::open_with_kms(data_dir, kms.clone())?;

        let mut shard_wals = Vec::with_capacity(shard_count as usize);
        for s in 0..shard_count {
            let dir = data_dir.join(format!("wal/shard-{s:03}"));
            shard_wals.push(WalWriter::open(&dir).map_err(UcError::internal)?);
        }
        let cross_check_wal =
            WalWriter::open(&data_dir.join("wal/cross_check")).map_err(UcError::internal)?;

        let audit = AuditChain::open(&data_dir.join("audit.chain")).map_err(UcError::internal)?;
        let logger = Logger::new(Some(&data_dir.join("node.log")), false).map_err(UcError::from)?;

        // Node key doubles as the HMAC token key (Kms derives a subkey).
        let signer: Arc<dyn Signer> = Arc::new(HmacSigner::new(kms.subkey("captoken")));

        // Curator backends: development mode is explicitly deterministic;
        // strict mode requires every configured slot, including every
        // adjudicator judge, to resolve to a pinned GGUF backend.
        let lib_backend = configured_curator_backend(
            data_dir,
            "librarian",
            &curator_cfg.librarian_model,
            curator_cfg.pinned.get("librarian"),
            curator_cfg.external_cmd.as_ref(),
            curator_cfg.strict_model_pins,
            &metrics,
        )?;
        let warden_backend = configured_curator_backend(
            data_dir,
            "warden",
            &curator_cfg.warden_model,
            curator_cfg.pinned.get("warden"),
            curator_cfg.external_cmd.as_ref(),
            curator_cfg.strict_model_pins,
            &metrics,
        )?;
        let mut pool: Vec<(String, Arc<dyn CuratorBackend>)> = Vec::new();
        for name in &curator_cfg.adjudicator_pool {
            let backend: Arc<dyn CuratorBackend> = if curator_cfg.strict_model_pins {
                let cmd = curator_cfg.external_cmd.as_ref().ok_or_else(|| {
                    UcError::unsupported("strict curator mode requires a GGUF runner")
                })?;
                let sha = curator_cfg.pinned.get(name).ok_or_else(|| {
                    UcError::schema(format!("missing curator pin for adjudicator: {name}"))
                })?;
                Arc::new(ExternalGgufBackend::new_for_slot(
                    data_dir,
                    name,
                    name,
                    sha,
                    cmd,
                    Some(metrics.clone()),
                )?)
            } else {
                Arc::new(DeterministicBackend)
            };
            pool.push((name.clone(), backend));
        }

        let mut cross_check = CrossCheckLedgerCell::new(ids::CROSS_CHECK);
        cross_check.quota_low = curator_cfg.disagreement_quota_low;
        cross_check.quota_high = curator_cfg.disagreement_quota_high;
        cross_check.attach_persistence(cross_check_wal.clone(), kms.clone());

        let manifest = Manifest {
            node_id: node_id.to_string(),
            proto_version: crate::router::envelope::PROTO_VERSION,
            logical_at: 0,
            clean_shutdown: false,
            encryption_tier: tier.as_str().to_string(),
            shard_count,
            state_hashes: BTreeMap::new(),
            last_snapshot: None,
        };

        Ok(Node {
            node_id: node_id.to_string(),
            data_dir: data_dir.to_path_buf(),
            clock: LogicalClock::new(1),
            boot_seed,
            shard_count,
            trinity_topology,
            view_version: Mutex::new(1),
            cells: Cells {
                catalog: Mutex::new(CatalogCell::new(ids::CATALOG)),
                fact: Mutex::new(FactCell::new(ids::FACT)),
                timeline: Mutex::new(TimelineCell::new(ids::TIMELINE)),
                scratchpad: Mutex::new(ScratchpadCell::new(ids::SCRATCHPAD)),
                playbook: Mutex::new(PlaybookCell::new(ids::PLAYBOOK)),
                vector: Mutex::new(VectorCell::new(ids::VECTOR, embedder_dim, boot_seed)),
                bm25: Mutex::new(Bm25Cell::new(ids::BM25)),
                graph: Mutex::new(GraphCell::new(ids::GRAPH)),
                reranker: Mutex::new(RerankerCell::new(ids::RERANKER, embedder_dim)),
                blob: Mutex::new(BlobCell::new(ids::BLOB)),
                agent_registry: Mutex::new(AgentRegistryCell::new(ids::AGENT_REGISTRY)),
                proposal: Mutex::new(ProposalCell::new(ids::PROPOSAL)),
                subscription: Mutex::new(SubscriptionCell::new(ids::SUBSCRIPTION)),
                cache: Mutex::new(CacheCell::new(ids::CACHE)),
            },
            trinity: Mutex::new(Trinity {
                contract: ContractCell::new(ids::CONTRACT),
                spec_anchor: SpecAnchorCell::new(ids::SPEC_ANCHOR),
                decision_ledger: DecisionLedgerCell::new(ids::DECISION_LEDGER),
                work_budget: WorkBudgetCell::new(ids::WORK_BUDGET),
                congruence: CongruenceCell::new(ids::CONGRUENCE),
                quarantine: QuarantineCell::new(ids::QUARANTINE),
                gap: GapCell::new(ids::GAP),
            }),
            curators: Curators {
                librarian: Mutex::new(LibrarianCell::new(ids::LIBRARIAN, lib_backend)),
                warden: Mutex::new(WardenCell::with_backend(ids::WARDEN, warden_backend)),
                adjudicator: Mutex::new(AdjudicatorCell::new(ids::ADJUDICATOR, pool)),
            },
            cross_check: Mutex::new(cross_check),
            curator_public_index: Mutex::new(BTreeMap::new()),
            guardrails: Guardrails {
                probe: Mutex::new(ProbeScheduler::new(boot_seed, curator_cfg.probe_rate)),
                blind: Mutex::new(BlindReauditScheduler::new(
                    boot_seed,
                    curator_cfg.blind_reaudit_rate,
                )),
                calibration: Mutex::new(CalibrationTracker::new()),
            },
            curator_cfg,
            shard_wals,
            cross_check_wal,
            kms,
            cas,
            snapshots,
            view_cache,
            manifest: Mutex::new(manifest),
            metrics,
            otlp: OtlpExporter::new(OtlpConfig::default()),
            logger,
            audit: Mutex::new(audit),
            events: Mutex::new(EventBus::new()),
            signer,
            mutation_barrier: Mutex::new(()),
            auto_snapshot_lock: Mutex::new(()),
            durable_wal_bytes: Mutex::new(0),
            last_snapshot_logical_at: Mutex::new(0),
            shutting_down: AtomicBool::new(false),
        })
    }

    /// Ephemeral in-tempdir node for tests + `--dry-run`.
    pub fn ephemeral(tag: &str) -> UcResult<Node> {
        let dir = std::env::temp_dir().join(format!(
            "ultracortex-{tag}-{}-{}",
            std::process::id(),
            DetRng::new(fnv1a64(tag.as_bytes())).next_u64()
        ));
        Node::open(
            &format!("node-{tag}"),
            &dir,
            2,
            EncryptionTier::T1,
            42,
            ShardTopology::Dedicated,
            CuratorConfig::development(),
            256,
        )
    }

    pub fn wal_for(&self, handle: &str) -> &Arc<WalWriter> {
        let shard = (fnv1a64(handle.as_bytes()) % self.shard_count) as usize;
        &self.shard_wals[shard]
    }

    /// Append an encrypted, durably committed WAL frame. The frame format
    /// remains unchanged; only the canonical CBOR payload is sealed before
    /// it crosses the storage boundary.
    pub fn append_wal(&self, handle: &str, frame: &WalFrame) -> UcResult<WalPos> {
        let mut durable = frame.clone();
        let purpose = payload_purpose(frame.cell_id, frame.flags);
        durable.payload = self.kms.seal(
            &purpose,
            payload_nonce(frame.logical_at, frame.cell_id, frame.op, &frame.payload),
            &frame.payload,
        );
        let wal = if frame.flags & FLAG_CROSS_CHECK != 0 {
            &self.cross_check_wal
        } else {
            self.wal_for(handle)
        };
        let frame_bytes = durable.to_bytes().len() as u64;
        let result = wal.append(&durable).map_err(UcError::internal);
        if result.is_ok() {
            let mut durable_bytes = self.durable_wal_bytes.lock().unwrap();
            *durable_bytes = durable_bytes.saturating_add(frame_bytes);
        }
        result
    }

    /// Decode a WAL payload, accepting pre-encryption frames for a bounded
    /// migration window. New frames are always marked by KMS.
    pub fn decode_wal_payload(&self, frame: &WalFrame) -> UcResult<Vec<u8>> {
        self.kms
            .unseal_legacy(&payload_purpose(frame.cell_id, frame.flags), &frame.payload)
    }

    /// Acquire the operation barrier used by Router/admin mutations.
    pub fn mutation_guard(&self) -> std::sync::MutexGuard<'_, ()> {
        self.mutation_barrier.lock().unwrap()
    }

    /// Mark the recovery watermark used by the automatic snapshot policy.
    /// Existing WAL bytes predate this process and will be covered by the
    /// recovered manifest; only new writes count toward the next cut.
    pub fn set_snapshot_watermark(&self, logical_at: u64) {
        *self.last_snapshot_logical_at.lock().unwrap() = logical_at;
        *self.durable_wal_bytes.lock().unwrap() = 0;
    }

    /// Run the configured automatic snapshot policy after a successful
    /// request. The separate lock prevents two concurrent request threads
    /// from both deciding that the same threshold is due.
    pub fn maybe_snapshot(&self, at: u64) -> UcResult<bool> {
        let _auto = self.auto_snapshot_lock.lock().unwrap();
        let bytes_due = *self.durable_wal_bytes.lock().unwrap() >= AUTO_SNAPSHOT_WAL_BYTES;
        let logical_due = at.saturating_sub(*self.last_snapshot_logical_at.lock().unwrap())
            >= AUTO_SNAPSHOT_LOGICAL_INTERVAL;
        if !bytes_due && !logical_due {
            return Ok(false);
        }
        self.write_snapshot(at)?;
        self.metrics.inc("snapshot.automatic");
        Ok(true)
    }

    pub fn tick(&self) -> u64 {
        self.clock.tick()
    }

    pub fn now(&self) -> u64 {
        self.clock.now()
    }

    pub fn bump_view_version(&self) {
        *self.view_version.lock().unwrap() += 1;
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    /// Snapshot every cell's state (cell_id -> canonical CBOR).
    pub fn snapshot_all(&self) -> BTreeMap<u64, Cbor> {
        use crate::cells::CellBehavior;
        let _barrier = self.mutation_barrier.lock().unwrap();
        let mut out = BTreeMap::new();
        macro_rules! snap {
            ($field:expr, $id:expr) => {
                out.insert($id.0, $field.lock().unwrap().snapshot_state());
            };
        }
        snap!(self.cells.catalog, ids::CATALOG);
        snap!(self.cells.fact, ids::FACT);
        snap!(self.cells.timeline, ids::TIMELINE);
        snap!(self.cells.scratchpad, ids::SCRATCHPAD);
        snap!(self.cells.playbook, ids::PLAYBOOK);
        snap!(self.cells.vector, ids::VECTOR);
        snap!(self.cells.bm25, ids::BM25);
        snap!(self.cells.graph, ids::GRAPH);
        snap!(self.cells.reranker, ids::RERANKER);
        snap!(self.cells.blob, ids::BLOB);
        snap!(self.cells.agent_registry, ids::AGENT_REGISTRY);
        snap!(self.cells.proposal, ids::PROPOSAL);
        snap!(self.cells.subscription, ids::SUBSCRIPTION);
        snap!(self.cells.cache, ids::CACHE);
        {
            let t = self.trinity.lock().unwrap();
            out.insert(ids::CONTRACT.0, t.contract.snapshot_state());
            out.insert(ids::SPEC_ANCHOR.0, t.spec_anchor.snapshot_state());
            out.insert(ids::DECISION_LEDGER.0, t.decision_ledger.snapshot_state());
            out.insert(ids::WORK_BUDGET.0, t.work_budget.snapshot_state());
            out.insert(ids::CONGRUENCE.0, t.congruence.snapshot_state());
            out.insert(ids::GAP.0, t.gap.snapshot_state());
            out.insert(ids::QUARANTINE.0, t.quarantine.snapshot_state());
        }
        snap!(self.curators.librarian, ids::LIBRARIAN);
        snap!(self.curators.warden, ids::WARDEN);
        snap!(self.curators.adjudicator, ids::ADJUDICATOR);
        snap!(self.cross_check, ids::CROSS_CHECK);
        out
    }

    /// Restore every cell from a snapshot map (recovery path B3b).
    pub fn restore_all(&self, states: &BTreeMap<u64, Cbor>) -> UcResult<()> {
        use crate::cells::CellBehavior;
        macro_rules! rest {
            ($field:expr, $id:expr) => {
                if let Some(s) = states.get(&$id.0) {
                    $field.lock().unwrap().restore_state(s)?;
                }
            };
        }
        rest!(self.cells.catalog, ids::CATALOG);
        rest!(self.cells.fact, ids::FACT);
        rest!(self.cells.timeline, ids::TIMELINE);
        rest!(self.cells.scratchpad, ids::SCRATCHPAD);
        rest!(self.cells.playbook, ids::PLAYBOOK);
        rest!(self.cells.vector, ids::VECTOR);
        rest!(self.cells.bm25, ids::BM25);
        rest!(self.cells.graph, ids::GRAPH);
        rest!(self.cells.reranker, ids::RERANKER);
        rest!(self.cells.blob, ids::BLOB);
        rest!(self.cells.agent_registry, ids::AGENT_REGISTRY);
        rest!(self.cells.proposal, ids::PROPOSAL);
        rest!(self.cells.subscription, ids::SUBSCRIPTION);
        rest!(self.cells.cache, ids::CACHE);
        {
            let mut t = self.trinity.lock().unwrap();
            if let Some(s) = states.get(&ids::CONTRACT.0) {
                t.contract.restore_state(s)?;
            }
            if let Some(s) = states.get(&ids::SPEC_ANCHOR.0) {
                t.spec_anchor.restore_state(s)?;
            }
            if let Some(s) = states.get(&ids::DECISION_LEDGER.0) {
                t.decision_ledger.restore_state(s)?;
            }
            if let Some(s) = states.get(&ids::WORK_BUDGET.0) {
                t.work_budget.restore_state(s)?;
            }
            if let Some(s) = states.get(&ids::CONGRUENCE.0) {
                t.congruence.restore_state(s)?;
            }
            if let Some(s) = states.get(&ids::GAP.0) {
                t.gap.restore_state(s)?;
            }
            if let Some(s) = states.get(&ids::QUARANTINE.0) {
                t.quarantine.restore_state(s)?;
            }
        }
        rest!(self.curators.librarian, ids::LIBRARIAN);
        rest!(self.curators.warden, ids::WARDEN);
        rest!(self.curators.adjudicator, ids::ADJUDICATOR);
        rest!(self.cross_check, ids::CROSS_CHECK);
        // Rebuild the public index from the curator snapshots directly (no
        // curator locks needed beyond the restores above).
        {
            let mut idx = self.curator_public_index.lock().unwrap();
            idx.clear();
            if let Some(lib) = states.get(&ids::LIBRARIAN.0) {
                if let Some(arr) = lib.get("outputs").and_then(|v| v.as_array()) {
                    for o in arr {
                        if let Some(h) = o.opt_str("output_handle") {
                            idx.insert(h, o.opt_str("body").unwrap_or_default());
                        }
                    }
                }
            }
            if let Some(w) = states.get(&ids::WARDEN.0) {
                if let Some(arr) = w.get("audits").and_then(|v| v.as_array()) {
                    for a in arr {
                        if let Some(h) = a.opt_str("judgment_handle") {
                            idx.insert(h, String::new());
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Record a curator artifact's PUBLIC body for SubstrateView lookups.
    pub fn index_public(&self, handle: &str, body: &str) {
        self.curator_public_index
            .lock()
            .unwrap()
            .insert(handle.to_string(), body.to_string());
    }

    /// Append a mandatory forensic event and make it durable before the
    /// associated state transition becomes visible.
    pub fn audit_event(&self, at: u64, event: &str, fields: &[(&str, Cbor)]) -> UcResult<()> {
        let result = self
            .audit
            .lock()
            .unwrap()
            .append(at, event, fields)
            .map(|_| ())
            .map_err(UcError::internal);
        if let Err(err) = &result {
            // Audit records are mandatory evidence, not best-effort logs. A
            // failed append permanently stops the node so callers cannot
            // continue making unaudited state changes.
            self.metrics.inc("audit.persistence_failure");
            self.shutting_down.store(true, Ordering::SeqCst);
            self.logger.error(
                at,
                "audit.persistence_failure",
                &[("event", event.to_string()), ("error", err.message.clone())],
            );
        }
        result
    }

    /// State hashes for the manifest (clean-shutdown integrity record).
    pub fn state_hashes(&self) -> BTreeMap<u64, [u8; 32]> {
        Self::state_hashes_for(&self.snapshot_all())
    }

    fn state_hashes_for(states: &BTreeMap<u64, Cbor>) -> BTreeMap<u64, [u8; 32]> {
        states
            .iter()
            .map(|(id, state)| (*id, sha256(&state.encode())))
            .collect()
    }

    /// Write a snapshot, update the manifest, and record the operator-visible
    /// pause window.
    pub fn write_snapshot(&self, at: u64) -> UcResult<SnapshotOutcome> {
        self.write_snapshot_internal(at, false)
    }

    fn write_snapshot_internal(
        &self,
        at: u64,
        update_state_hashes: bool,
    ) -> UcResult<SnapshotOutcome> {
        let total_started = Instant::now();

        let capture_started = Instant::now();
        let states = self.snapshot_all();
        let capture_us = capture_started.elapsed().as_micros() as u64;

        let (state_hashes, hash_us) = if update_state_hashes {
            let hash_started = Instant::now();
            let hashes = Self::state_hashes_for(&states);
            (Some(hashes), hash_started.elapsed().as_micros() as u64)
        } else {
            (None, 0)
        };

        let write_started = Instant::now();
        let snap_name = self.snapshots.write(at, &states)?;
        let write_us = write_started.elapsed().as_micros() as u64;

        let manifest_started = Instant::now();
        {
            let mut m = self.manifest.lock().unwrap();
            m.logical_at = at;
            m.last_snapshot = Some(snap_name.clone());
            if let Some(hashes) = state_hashes {
                m.state_hashes = hashes;
            }
            m.save(&self.data_dir)?;
        }
        self.set_snapshot_watermark(at);
        let manifest_us = manifest_started.elapsed().as_micros() as u64;

        let total_us = total_started.elapsed().as_micros() as u64;
        let pause_us = capture_us;
        let within_target = pause_us <= SNAPSHOT_PAUSE_TARGET_US;
        self.metrics.observe("snapshot.pause_us", pause_us);
        self.metrics.observe("snapshot.total_us", total_us);
        self.metrics.observe("snapshot.capture_us", capture_us);
        self.metrics.observe("snapshot.hash_us", hash_us);
        self.metrics.observe("snapshot.write_us", write_us);
        self.metrics.observe("snapshot.manifest_us", manifest_us);
        self.metrics
            .gauge_set("snapshot.last_pause_us", pause_us as i64);
        self.metrics
            .gauge_set("snapshot.last_total_us", total_us as i64);
        self.metrics
            .gauge_set("snapshot.last_cells", states.len() as i64);
        if !within_target {
            self.metrics.inc("snapshot.pause_target_exceeded");
            self.logger.warn(
                at,
                "snapshot.pause_target_exceeded",
                &[
                    ("pause_us", pause_us.to_string()),
                    ("total_us", total_us.to_string()),
                    ("pause_target_us", SNAPSHOT_PAUSE_TARGET_US.to_string()),
                    ("cells", states.len().to_string()),
                ],
            );
        } else {
            self.logger.info(
                at,
                "snapshot.completed",
                &[
                    ("pause_us", pause_us.to_string()),
                    ("total_us", total_us.to_string()),
                    ("pause_target_us", SNAPSHOT_PAUSE_TARGET_US.to_string()),
                    ("cells", states.len().to_string()),
                    ("snapshot", snap_name.clone()),
                ],
            );
        }

        Ok(SnapshotOutcome {
            name: snap_name,
            cells: states.len(),
            pause_us,
            total_us,
            capture_us,
            hash_us,
            write_us,
            manifest_us,
            within_target,
        })
    }

    /// Clean shutdown: snapshot → WAL sync → manifest(clean=true).
    pub fn shutdown(&self) -> UcResult<()> {
        self.shutting_down.store(true, Ordering::SeqCst);
        let at = self.now();
        let snap = self.write_snapshot_internal(at, true)?;
        for w in &self.shard_wals {
            w.sync().map_err(UcError::internal)?;
        }
        self.cross_check_wal.sync().map_err(UcError::internal)?;
        {
            let mut m = self.manifest.lock().unwrap();
            m.logical_at = at;
            m.clean_shutdown = true;
            m.save(&self.data_dir)?;
        }
        self.logger.info(
            at,
            "node.shutdown",
            &[
                ("clean", "true".into()),
                ("snapshot", snap.name),
                ("pause_us", snap.pause_us.to_string()),
            ],
        );
        Ok(())
    }
}

/// The Node itself is the curators' window onto public substrate state.
/// Private facets are *not reachable* through this trait — the resolution
/// path for them lives solely in the Router's hydrate handler behind the
/// facet-scope check (P19 by construction).
impl SubstrateView for Node {
    fn handle_exists(&self, handle: &str) -> bool {
        if handle.starts_with("fact/") {
            return self.cells.fact.lock().unwrap().exists(handle);
        }
        if handle.starts_with("blob/") {
            return self.cells.blob.lock().unwrap().exists(handle);
        }
        if handle.starts_with("decision/") {
            return self.trinity.lock().unwrap().decision_ledger.exists(handle);
        }
        if (handle.starts_with("librarian/output/") || handle.starts_with("warden/judgment/"))
            && !crate::curator::is_private_facet(handle)
        {
            return self
                .curator_public_index
                .lock()
                .unwrap()
                .contains_key(handle);
        }
        false
    }

    fn active_sp(&self, subject: &str, predicate: &str) -> Vec<(String, String)> {
        self.cells
            .fact
            .lock()
            .unwrap()
            .active_for_sp(subject, predicate)
            .into_iter()
            .map(|f| (f.handle.clone(), f.object.clone()))
            .collect()
    }

    fn public_text(&self, handle: &str) -> Option<String> {
        if handle.starts_with("fact/") {
            return self
                .cells
                .fact
                .lock()
                .unwrap()
                .get(handle)
                .map(|f| f.object.clone());
        }
        if handle.starts_with("blob/") {
            let sha = {
                let blob = self.cells.blob.lock().unwrap();
                blob.lookup(handle).map(|(sha, _, _)| *sha)
            }?;
            return self
                .cas
                .get(&sha)
                .ok()
                .map(|b| String::from_utf8_lossy(&b).into_owned());
        }
        if (handle.starts_with("librarian/output/") || handle.starts_with("warden/judgment/"))
            && !crate::curator::is_private_facet(handle)
        {
            return self
                .curator_public_index
                .lock()
                .unwrap()
                .get(handle)
                .cloned();
        }
        if handle.starts_with("decision/") {
            return self
                .trinity
                .lock()
                .unwrap()
                .decision_ledger
                .get(handle)
                .map(|d| d.statement.clone());
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_opens_snapshots_and_restores() {
        let node = Node::ephemeral("nodetest").unwrap();
        // Seed a fact directly for the snapshot roundtrip.
        {
            let mut fact = node.cells.fact.lock().unwrap();
            fact.insert(crate::cells::memory::Fact {
                handle: "fact/T1".into(),
                subject: "s".into(),
                predicate: "p".into(),
                object: "o".into(),
                confidence: None,
                written_at: 1,
                superseded_by: None,
                supersedes: None,
                anchor: "Architecture.md\u{00a7}4".into(),
            });
        }
        let snap = node.snapshot_all();
        assert!(snap.len() >= 25);
        let node2 = Node::ephemeral("nodetest2").unwrap();
        node2.restore_all(&snap).unwrap();
        assert!(node2.handle_exists("fact/T1"));
        assert_eq!(node2.public_text("fact/T1").as_deref(), Some("o"));
        assert_eq!(
            node2.active_sp("s", "p"),
            vec![("fact/T1".to_string(), "o".to_string())]
        );
        // State hashes match after restore.
        assert_eq!(node.state_hashes(), node2.state_hashes());
    }

    #[test]
    fn wal_sharding_is_stable() {
        let node = Node::ephemeral("shards").unwrap();
        let a = node.wal_for("fact/AAAA").dir().to_path_buf();
        let b = node.wal_for("fact/AAAA").dir().to_path_buf();
        assert_eq!(a, b);
    }

    #[test]
    fn t1_wal_boundary_does_not_store_plaintext_payload() {
        let dir = std::env::temp_dir().join(format!("uc-node-t1-wal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let node = Node::open(
            "node-t1-wal",
            &dir,
            1,
            EncryptionTier::T1,
            42,
            ShardTopology::Dedicated,
            CuratorConfig::development(),
            256,
        )
        .unwrap();
        let sentinel = b"wal-boundary-secret";
        let payload = Cbor::map(vec![(
            "secret",
            Cbor::t(String::from_utf8(sentinel.to_vec()).unwrap()),
        )])
        .encode();
        let frame = crate::persist::wal::WalFrame {
            logical_at: 1,
            cell_id: ids::FACT.0,
            op: crate::persist::wal::WalOp::Write,
            schema_ver: 1,
            flags: 0,
            payload: payload.clone(),
        };
        let wal_dir = node.wal_for("fact/t1-wal").dir().to_path_buf();
        node.append_wal("fact/t1-wal", &frame).unwrap();
        let wal_file = std::fs::read_dir(&wal_dir)
            .unwrap()
            .flatten()
            .find(|entry| entry.file_name().to_string_lossy().ends_with(".wal"))
            .unwrap();
        let raw = std::fs::read(wal_file.path()).unwrap();
        assert!(!raw.windows(sentinel.len()).any(|w| w == sentinel));
        let replayed = crate::persist::wal::replay_dir(&wal_dir).unwrap().frames;
        assert_eq!(replayed.len(), 1);
        assert_eq!(node.decode_wal_payload(&replayed[0]).unwrap(), payload);

        node.shutdown().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn snapshot_outcome_surfaces_pause_measurement() {
        let node = Node::ephemeral("snapshot-outcome").unwrap();
        let at = node.now();
        let snap = node.write_snapshot(at).unwrap();
        assert!(snap.name.starts_with("snap-"));
        assert!(snap.cells >= 25);
        assert_eq!(
            snap.within_target,
            snap.pause_us <= SNAPSHOT_PAUSE_TARGET_US
        );
        let metrics = node.metrics.snapshot();
        assert!(metrics.get("snapshot.pause_us.count").copied().unwrap_or(0) > 0);
        assert_eq!(
            node.metrics.gauge("snapshot.last_pause_us"),
            snap.pause_us as i64
        );
        assert_eq!(node.metrics.gauge("snapshot.last_cells"), snap.cells as i64);
    }

    #[test]
    fn logical_snapshot_threshold_triggers_automatic_snapshot() {
        let node = Node::ephemeral("automatic-snapshot").unwrap();
        node.set_snapshot_watermark(0);
        assert!(node.maybe_snapshot(AUTO_SNAPSHOT_LOGICAL_INTERVAL).unwrap());
        assert_eq!(node.metrics.counter("snapshot.automatic"), 1);
        assert!(!node.maybe_snapshot(AUTO_SNAPSHOT_LOGICAL_INTERVAL).unwrap());
    }
}
