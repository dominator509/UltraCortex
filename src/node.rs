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
use crate::obs::{AuditChain, Logger, Metrics};
use crate::persist::wal::WalWriter;
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
    pub logger: Logger,
    pub audit: Mutex<AuditChain>,
    pub events: Mutex<EventBus>,
    pub signer: Arc<dyn Signer>,

    pub shutting_down: AtomicBool,
}

impl Node {
    /// Construct with everything empty; the Bootstrap Operator (B1–B6)
    /// provisions cells in Trinity-first order and replays state.
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
        let kms = Arc::new(Kms::open(data_dir, tier)?);
        let cas = Arc::new(CasStore::open(data_dir)?);
        let snapshots = SnapshotStore::open(data_dir)?;
        let view_cache = PrefixCacheStore::open(data_dir)?;

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

        // Curator backends: pinned GGUF when configured + present, else the
        // deterministic backend. A missing/unverifiable weight file is a
        // hard error only when explicitly pinned (Bootstrap B3 fail-fast).
        let lib_backend: Arc<dyn CuratorBackend> = match (
            curator_cfg.external_cmd.as_ref(),
            curator_cfg.pinned.get("librarian"),
        ) {
            (Some(cmd), Some(sha)) => Arc::new(ExternalGgufBackend::new(
                data_dir,
                "librarian",
                sha,
                cmd,
                Some(metrics.clone()),
            )?),
            _ => Arc::new(DeterministicBackend),
        };
        let pool: Vec<(String, Arc<dyn CuratorBackend>)> = curator_cfg
            .adjudicator_pool
            .iter()
            .map(|name| {
                let backend: Arc<dyn CuratorBackend> = match (
                    curator_cfg.external_cmd.as_ref(),
                    curator_cfg.pinned.get(name),
                ) {
                    (Some(cmd), Some(sha)) => match ExternalGgufBackend::new(
                        data_dir,
                        name,
                        sha,
                        cmd,
                        Some(metrics.clone()),
                    ) {
                        Ok(b) => Arc::new(b),
                        Err(_) => Arc::new(DeterministicBackend),
                    },
                    _ => Arc::new(DeterministicBackend),
                };
                (name.clone(), backend)
            })
            .collect();

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
                warden: Mutex::new(WardenCell::new(ids::WARDEN)),
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
            logger,
            audit: Mutex::new(audit),
            events: Mutex::new(EventBus::new()),
            signer,
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
            CuratorConfig::default(),
            256,
        )
    }

    pub fn wal_for(&self, handle: &str) -> &Arc<WalWriter> {
        let shard = (fnv1a64(handle.as_bytes()) % self.shard_count) as usize;
        &self.shard_wals[shard]
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
            let _ = w.sync();
        }
        let _ = self.cross_check_wal.sync();
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
}
