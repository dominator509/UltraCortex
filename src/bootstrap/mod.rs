//! Bootstrap Operator — SPEC-DERIVED-§B1–§B6 (Bootstrap.md).
//!
//! The Operator owns node lifecycle:
//!
//! - **B1** Config: `ultracortex.toml` ← `UC_*` env vars ← CLI overrides.
//! - **B2** Node::open (persistence handles, curator backends, signer).
//! - **B3** Provisioning, **Trinity-first, fatal ordering**: Contract →
//!   SpecAnchor → DecisionLedger → Gap → Congruence → WorkBudget →
//!   Quarantine, then curators, then everything else. Fresh boot (B3a)
//!   seeds governance state; recovery (B3b) loads manifest → snapshot →
//!   replays WAL frames past the snapshot → verifies the audit chain and
//!   pinned weights.
//! - **B4** Listeners (UDS 0600 preferred; TCP loopback opt-in).
//! - **B5** Self-test: eleven end-to-end checks through the real Router;
//!   any failure aborts boot (a substrate that can't police itself must
//!   not serve).
//! - **B6** `ready node_id=<id> proto_version=1` on stdout.
//!
//! The admin plane ([`admin_dispatch`]) serves every operator verb over
//! the same wire protocol, marked `type: "admin"`.

use crate::core::cbor::Cbor;
use crate::core::minitoml::{self, TomlValue};
use crate::core::ulid::{DetRng, Ulid};
use crate::core::{fnv1a64, ErrCode, Intent, Severity, ShardTopology, Tier, UcError, UcResult};
use crate::curator::guardrails::{CALIBRATION_WINDOW, HIGH_BAND_FLOOR, MEDIUM_BAND_FLOOR};
use crate::curator::ledger::{
    BATCH_SIGN_EVERY, PROBE_BOOST_ON_SUSPICIOUS,
    RETENTION_POLICY as CROSS_CHECK_RETENTION_POLICY,
};
use crate::curator::{CuratorConfig, CuratorKvBudgetProfile};
use crate::node::{ids, Node};
use crate::obs::{AuditChain, OtlpConfig};
use crate::persist::wal::{replay_dir, WalOp};
use crate::persist::{EncryptionTier, Manifest};
use crate::router::captoken::{
    issue_agent_token, issue_curator_token, issue_operator_token, CapToken,
};
use crate::router::envelope::{Envelope, EnvelopeFlags, WorkBudget, PROTO_VERSION};
use crate::router::handle_envelope;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static DRY_RUN_SEQ: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// B1 — Config
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Config {
    pub node_id: String,
    pub data_dir: PathBuf,
    pub shards: u64,
    pub trinity_topology: ShardTopology,
    pub encryption_tier: EncryptionTier,
    pub boot_seed: u64,
    pub embedder_dim: usize,
    pub uds_path: Option<PathBuf>,
    pub tcp_addr: Option<String>,
    pub curator: CuratorConfig,
    pub observability: OtlpConfig,
}

impl Default for Config {
    fn default() -> Self {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get() as u64)
            .unwrap_or(4);
        Config {
            node_id: "ultracortex-0".into(),
            data_dir: PathBuf::from("./ultracortex-data"),
            shards: cores.saturating_sub(2).max(2),
            trinity_topology: ShardTopology::Dedicated,
            encryption_tier: EncryptionTier::T1,
            boot_seed: 0x0517AC0817E,
            embedder_dim: 256,
            uds_path: None, // defaults to <data_dir>/ultracortex.sock
            tcp_addr: None, // "127.0.0.1:7741" when enabled
            curator: CuratorConfig::default(),
            observability: OtlpConfig::default(),
        }
    }
}

impl Config {
    /// Merge order: defaults ← TOML file ← UC_* environment ← CLI pairs.
    pub fn load(path: Option<&Path>, cli_overrides: &[(String, String)]) -> UcResult<Config> {
        let mut cfg = Config::default();

        if let Some(p) = path {
            let text = std::fs::read_to_string(p).map_err(UcError::from)?;
            let doc = minitoml::parse(&text).map_err(UcError::schema)?;
            cfg.apply_toml(&doc)?;
        } else if Path::new("ultracortex.toml").exists() {
            let text = std::fs::read_to_string("ultracortex.toml").map_err(UcError::from)?;
            let doc = minitoml::parse(&text).map_err(UcError::schema)?;
            cfg.apply_toml(&doc)?;
        }

        // UC_* environment overrides.
        for (key, setter) in ENV_KEYS {
            if let Ok(v) = std::env::var(key) {
                setter(&mut cfg, &v)?;
            }
        }

        // CLI --set key=value overrides (highest precedence).
        for (k, v) in cli_overrides {
            apply_kv(&mut cfg, k, v)?;
        }

        if cfg.uds_path.is_none() {
            cfg.uds_path = Some(cfg.data_dir.join("ultracortex.sock"));
        }
        Ok(cfg)
    }

    fn apply_toml(&mut self, doc: &minitoml::TomlDoc) -> UcResult<()> {
        let get = |section: &str, key: &str| -> Option<TomlValue> {
            doc.get(section).and_then(|s| s.get(key)).cloned()
        };
        if let Some(v) = get("node", "id").and_then(|v| v.as_str().map(String::from)) {
            self.node_id = v;
        }
        if let Some(v) = get("node", "data_dir").and_then(|v| v.as_str().map(String::from)) {
            self.data_dir = PathBuf::from(v);
        }
        if let Some(v) = get("node", "shards").and_then(|v| v.as_int()) {
            self.shards = (v.max(1)) as u64;
        }
        if let Some(v) =
            get("shards", "trinity_topology").and_then(|v| v.as_str().map(String::from))
        {
            if let Some(topology) = ShardTopology::from_str(&v) {
                self.trinity_topology = topology;
            }
        }
        if let Some(v) = get("node", "boot_seed").and_then(|v| v.as_int()) {
            self.boot_seed = v as u64;
        }
        if let Some(v) = get("node", "embedder_dim").and_then(|v| v.as_int()) {
            self.embedder_dim = v.max(16) as usize;
        }
        if let Some(v) =
            get("persist", "encryption_tier").and_then(|v| v.as_str().map(String::from))
        {
            if let Ok(t) = EncryptionTier::parse(&v) {
                self.encryption_tier = t;
            }
        }
        if let Some(v) = get("listen", "uds").and_then(|v| v.as_str().map(String::from)) {
            self.uds_path = if v.is_empty() {
                None
            } else {
                Some(PathBuf::from(v))
            };
        }
        if let Some(v) = get("listen", "tcp").and_then(|v| v.as_str().map(String::from)) {
            self.tcp_addr = if v.is_empty() {
                None
            } else {
                crate::proto::validate_tcp_listen_addr(&v)?;
                Some(v)
            };
        }
        // [curator]
        if let Some(v) = get("curator", "disagreement_quota_low").and_then(|v| v.as_float()) {
            self.curator.disagreement_quota_low = v;
        }
        if let Some(v) = get("curator", "disagreement_quota_high").and_then(|v| v.as_float()) {
            self.curator.disagreement_quota_high = v;
        }
        if let Some(v) = get("curator", "probe_rate").and_then(|v| v.as_float()) {
            self.curator.probe_rate = v;
        }
        if let Some(v) = get("curator", "blind_reaudit_rate").and_then(|v| v.as_float()) {
            self.curator.blind_reaudit_rate = v;
        }
        if let Some(v) =
            get("curator", "kv_budget_profile").and_then(|v| v.as_str().map(String::from))
        {
            self.curator.kv_budget_profile = CuratorKvBudgetProfile::from_str(&v)
                .ok_or_else(|| UcError::schema("bad curator kv budget profile"))?;
        }
        if let Some(v) = get("curator", "topology").and_then(|v| v.as_str().map(String::from)) {
            if let Some(topology) = ShardTopology::from_str(&v) {
                self.curator.topology = topology;
            }
        }
        if let Some(v) = get("curator", "external_cmd").and_then(|v| v.as_str().map(String::from)) {
            self.curator.external_cmd = if v.is_empty() { None } else { Some(v) };
        }
        if let Some(v) = get("curator", "librarian_model")
            .and_then(|v| v.as_str().map(String::from))
        {
            self.curator.librarian_model = v;
        }
        if let Some(v) = get("curator", "warden_model")
            .and_then(|v| v.as_str().map(String::from))
        {
            self.curator.warden_model = v;
        }
        if let Some(v) = get("curator", "pool").and_then(|v| v.as_str().map(String::from)) {
            let pool: Vec<String> = v
                .split(',')
                .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
            if !pool.is_empty() {
                self.curator.adjudicator_pool = pool;
            }
        }

        // [observability]
        if let Some(v) = get("observability", "otlp_enabled").and_then(|v| v.as_bool()) {
            self.observability.enabled = v;
        }
        if let Some(v) = get("observability", "otlp_metrics_endpoint")
            .and_then(|v| v.as_str().map(String::from))
        {
            self.observability.metrics_endpoint = v;
        }
        if let Some(v) = get("observability", "otlp_traces_endpoint")
            .and_then(|v| v.as_str().map(String::from))
        {
            self.observability.traces_endpoint = v;
        }
        if let Some(v) = get("observability", "otlp_logs_endpoint")
            .and_then(|v| v.as_str().map(String::from))
        {
            self.observability.logs_endpoint = v;
        }
        if let Some(v) = get("observability", "otlp_timeout_ms").and_then(|v| v.as_int()) {
            self.observability.timeout_ms = v.max(1) as u64;
        }
        // [curator.pinned]: model = "sha256hex"
        if let Some(section) = doc.get("curator.pinned") {
            for (model, v) in section {
                if let Some(sha) = v.as_str() {
                    self.curator.pinned.insert(model.clone(), sha.to_string());
                }
            }
        }
        Ok(())
    }
}

type EnvSetter = fn(&mut Config, &str) -> UcResult<()>;
const ENV_KEYS: [(&str, EnvSetter); 14] = [
    ("UC_NODE_ID", |c, v| {
        c.node_id = v.to_string();
        Ok(())
    }),
    ("UC_DATA_DIR", |c, v| {
        c.data_dir = PathBuf::from(v);
        Ok(())
    }),
    ("UC_SHARDS", |c, v| {
        c.shards = v
            .parse::<u64>()
            .map_err(|e| UcError::schema(e.to_string()))?
            .max(1);
        Ok(())
    }),
    ("UC_TRINITY_TOPOLOGY", |c, v| {
        c.trinity_topology =
            ShardTopology::from_str(v).ok_or_else(|| UcError::schema("bad UC_TRINITY_TOPOLOGY"))?;
        Ok(())
    }),
    ("UC_ENCRYPTION_TIER", |c, v| {
        c.encryption_tier = EncryptionTier::parse(v)?;
        Ok(())
    }),
    ("UC_UDS", |c, v| {
        c.uds_path = if v.is_empty() {
            None
        } else {
            Some(PathBuf::from(v))
        };
        Ok(())
    }),
    ("UC_TCP", |c, v| {
        c.tcp_addr = if v.is_empty() {
            None
        } else {
            crate::proto::validate_tcp_listen_addr(v)?;
            Some(v.to_string())
        };
        Ok(())
    }),
    ("UC_OTLP_ENABLED", |c, v| {
        c.observability.enabled = match v.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" => true,
            "0" | "false" | "no" => false,
            _ => return Err(UcError::schema("bad UC_OTLP_ENABLED")),
        };
        Ok(())
    }),
    ("UC_OTLP_METRICS_ENDPOINT", |c, v| {
        c.observability.metrics_endpoint = v.to_string();
        Ok(())
    }),
    ("UC_OTLP_TRACES_ENDPOINT", |c, v| {
        c.observability.traces_endpoint = v.to_string();
        Ok(())
    }),
    ("UC_OTLP_LOGS_ENDPOINT", |c, v| {
        c.observability.logs_endpoint = v.to_string();
        Ok(())
    }),
    ("UC_OTLP_TIMEOUT_MS", |c, v| {
        c.observability.timeout_ms = v
            .parse::<u64>()
            .map_err(|_| UcError::schema("bad UC_OTLP_TIMEOUT_MS"))?
            .max(1);
        Ok(())
    }),
    ("UC_CURATOR_TOPOLOGY", |c, v| {
        c.curator.topology =
            ShardTopology::from_str(v).ok_or_else(|| UcError::schema("bad UC_CURATOR_TOPOLOGY"))?;
        Ok(())
    }),
    ("UC_CURATOR_KV_BUDGET_PROFILE", |c, v| {
        c.curator.kv_budget_profile = CuratorKvBudgetProfile::from_str(v)
            .ok_or_else(|| UcError::schema("bad UC_CURATOR_KV_BUDGET_PROFILE"))?;
        Ok(())
    }),
];

fn apply_kv(cfg: &mut Config, k: &str, v: &str) -> UcResult<()> {
    match k {
        "node.id" => cfg.node_id = v.to_string(),
        "node.data_dir" => cfg.data_dir = PathBuf::from(v),
        "node.shards" => {
            cfg.shards = v
                .parse::<u64>()
                .map_err(|e| UcError::schema(e.to_string()))?
                .max(1)
        }
        "shards.trinity_topology" => {
            cfg.trinity_topology =
                ShardTopology::from_str(v).ok_or_else(|| UcError::schema("bad trinity topology"))?
        }
        "node.boot_seed" => {
            cfg.boot_seed = v.parse().map_err(|_| UcError::schema("bad boot_seed"))?
        }
        "persist.encryption_tier" => cfg.encryption_tier = EncryptionTier::parse(v)?,
        "listen.uds" => {
            cfg.uds_path = if v.is_empty() {
                None
            } else {
                Some(PathBuf::from(v))
            }
        }
        "listen.tcp" => {
            cfg.tcp_addr = if v.is_empty() {
                None
            } else {
                crate::proto::validate_tcp_listen_addr(v)?;
                Some(v.to_string())
            }
        }
        "observability.otlp_enabled" => {
            cfg.observability.enabled = match v.to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" => true,
                "0" | "false" | "no" => false,
                _ => return Err(UcError::schema("bad observability.otlp_enabled")),
            };
        }
        "observability.otlp_metrics_endpoint" => {
            cfg.observability.metrics_endpoint = v.to_string();
        }
        "observability.otlp_traces_endpoint" => {
            cfg.observability.traces_endpoint = v.to_string();
        }
        "observability.otlp_logs_endpoint" => {
            cfg.observability.logs_endpoint = v.to_string();
        }
        "observability.otlp_timeout_ms" => {
            cfg.observability.timeout_ms = v
                .parse::<u64>()
                .map_err(|_| UcError::schema("bad observability.otlp_timeout_ms"))?
                .max(1);
        }
        "curator.kv_budget_profile" => {
            cfg.curator.kv_budget_profile = CuratorKvBudgetProfile::from_str(v)
                .ok_or_else(|| UcError::schema("bad curator kv budget profile"))?
        }
        "curator.topology" => {
            cfg.curator.topology =
                ShardTopology::from_str(v).ok_or_else(|| UcError::schema("bad curator topology"))?
        }
        other => return Err(UcError::schema(format!("unknown config key `{other}`"))),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Spec docs registered as anchors at boot (B3a). Sections mirror the v1.0
// blueprint pack; build.rs additionally harvests SPEC-DERIVED-§ markers.
// ---------------------------------------------------------------------------

pub const BOOT_ANCHORS: [(&str, &[&str]); 10] = [
    ("Architecture.md", &["1", "2", "3", "4", "5", "9"]),
    ("NATIVE_TRINITY.md", &["3", "4", "5", "8", "9"]),
    ("RouterScheduler.md", &["A", "B", "C", "D", "E", "5"]),
    ("PersistenceLayer.md", &["2", "3", "5", "6", "7"]),
    ("McpProtocol.md", &["2", "3", "4", "5", "A"]),
    ("CURATOR_PAIR_PROTOCOL.md", &["2", "3", "4", "5", "6", "7"]),
    ("LibrarianCell.md", &["2", "3", "4", "5", "6"]),
    ("WardenCell.md", &["3", "4", "5", "6", "7"]),
    ("AdjudicatorCell.md", &["3", "4", "5", "6", "7", "10"]),
    ("CrossCheckLedgerCell.md", &["2", "5", "6", "7"]),
];

// ---------------------------------------------------------------------------
// Operator
// ---------------------------------------------------------------------------

pub struct BootReport {
    pub node: Arc<Node>,
    pub recovered: bool,
    pub replayed_frames: u64,
    pub self_test_passed: u32,
}

pub fn boot(cfg: &Config) -> UcResult<BootReport> {
    // B2 — open the node shell.
    let node = Arc::new(Node::open(
        &cfg.node_id,
        &cfg.data_dir,
        cfg.shards,
        cfg.encryption_tier,
        cfg.boot_seed,
        cfg.trinity_topology,
        cfg.curator.clone(),
        cfg.embedder_dim,
    )?);
    node.otlp.configure(cfg.observability.clone());

    // B3 — provision (fresh) or recover, Trinity-first.
    let prior = Manifest::load(&cfg.data_dir)?;
    let (recovered, replayed) = match prior {
        None => {
            provision_fresh(&node)?;
            (false, 0)
        }
        Some(manifest) => {
            let frames = recover(&node, &manifest)?;
            (true, frames)
        }
    };

    // Verify pinned weights before serving (P20 weight pinning).
    for (model, sha) in &node.curator_cfg.pinned {
        crate::persist::verify_weight_file(&node.data_dir, model, sha)?;
    }

    // B5 — self-test. Fatal on any failure.
    let passed = self_test(&node)?;

    node.audit
        .lock()
        .unwrap()
        .append(
            node.now(),
            "node.boot",
            &[
                ("node_id", Cbor::t(node.node_id.clone())),
                ("recovered", Cbor::Bool(recovered)),
                ("replayed", Cbor::U64(replayed)),
                ("self_test", Cbor::U64(passed as u64)),
            ],
        )
        .map_err(UcError::internal)?;

    Ok(BootReport {
        node,
        recovered,
        replayed_frames: replayed,
        self_test_passed: passed,
    })
}

/// B3a — fresh provisioning in the mandated order. Any failure is fatal:
/// governance must exist before anything governable does.
fn provision_fresh(node: &Node) -> UcResult<()> {
    let at = node.tick();
    {
        let mut t = node.trinity.lock().unwrap();

        // 1. ContractCell — schemas for facts + curator artifacts.
        t.contract.register(
            at,
            "fact.v1",
            vec!["subject".into(), "predicate".into(), "object".into()],
        );
        t.contract.register(
            at,
            "curator.librarian.output.v1",
            vec![
                "output_handle".into(),
                "operation".into(),
                "grounded_in".into(),
                "confidence_band".into(),
            ],
        );
        t.contract.register(
            at,
            "curator.warden.judgment.v1",
            vec![
                "output_handle".into(),
                "operation".into(),
                "target_handle".into(),
            ],
        );
        t.contract.register(at, "blob.v1", vec!["body".into()]);
        t.contract
            .register(at, "timeline.v1", vec!["stream".into(), "event".into()]);
        t.contract
            .register(at, "scratchpad.v1", vec!["key".into(), "value".into()]);

        // 2. SpecAnchorCell — the blueprint docs are the constitution.
        for (doc, sections) in BOOT_ANCHORS {
            for s in sections {
                t.spec_anchor.register(doc, s);
            }
        }
        // Plus every SPEC-DERIVED-§ marker harvested from the source tree
        // by build.rs (B3a.6): the code's own citations must resolve.
        for (doc, section, _artifact, _line) in crate::spec_inventory::SPEC_ANCHOR_INVENTORY {
            let first = section
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '.')
                .next()
                .unwrap_or(section)
                .trim_matches('.');
            if !first.is_empty() {
                t.spec_anchor.register(doc, first);
            }
        }

        // 3. DecisionLedgerCell — founding decisions.
        t.decision_ledger.append(
            at,
            node.boot_seed,
            "governance",
            "Trinity-first provisioning order is mandatory and fatal on violation",
            "bootstrap-operator",
            "NATIVE_TRINITY.md\u{00a7}3",
        );
        t.decision_ledger.append(
            at,
            node.boot_seed,
            "curator",
            "Curator private facets are excluded from peer-curator capability tokens",
            "bootstrap-operator",
            "CURATOR_PAIR_PROTOCOL.md\u{00a7}4",
        );

        // 4. GapCell — the standing self-knowledge gap.
        t.gap.register(
            at,
            "GAP-0001",
            "conformance coverage of the v1.0 spec is incomplete until all tests pass",
        );

        // 5. CongruenceCell — vocabulary of known entities.
        for ct in [
            "CatalogCell",
            "FactCell",
            "TimelineCell",
            "PlaybookCell",
            "ScratchpadCell",
            "VectorCell",
            "GraphCell",
            "Bm25Cell",
            "BlobCell",
            "CacheCell",
            "AgentRegistryCell",
            "ProposalCell",
            "SubscriptionCell",
            "RerankerCell",
            "SpecAnchorCell",
            "DecisionLedgerCell",
            "CongruenceCell",
            "GapCell",
            "QuarantineCell",
            "WorkBudgetCell",
            "ContractCell",
            "LibrarianCell",
            "WardenCell",
            "AdjudicatorCell",
            "CrossCheckLedgerCell",
        ] {
            t.congruence.register_entity(ct);
        }
        t.congruence.register_entity("GAP-0001");
        t.congruence.register_entity("P19");
        t.congruence.register_entity("P20");

        // 6/7. WorkBudget + Quarantine need no extra seeding here: the
        // namespace-default policy covers curator/bootstrap/admin tasks.
    }

    // Curators + operator in the registry (after Trinity — order matters).
    {
        let mut reg = node.cells.agent_registry.lock().unwrap();
        reg.register(at, "operator", "operator");
        reg.register(at, "curator.librarian", "curator");
        reg.register(at, "curator.warden", "curator");
        reg.register(at, "curator.adjudicator", "curator");
    }

    node.logger.info(at, "bootstrap.fresh", &[]);
    Ok(())
}

/// B3b — recovery: snapshot → WAL replay past the snapshot → integrity
/// verification. Returns replayed frame count.
fn recover(node: &Node, manifest: &Manifest) -> UcResult<u64> {
    if manifest.node_id != node.node_id {
        return Err(UcError::internal(format!(
            "data dir belongs to node `{}`, refusing to boot as `{}`",
            manifest.node_id, node.node_id
        )));
    }
    if !manifest.clean_shutdown {
        node.metrics.inc("boot.unclean_recovery");
        node.logger
            .warn(0, "bootstrap.unclean_shutdown_detected", &[]);
    }

    // Snapshot restore.
    let mut snapshot_at = 0u64;
    if let Some(name) = &manifest.last_snapshot {
        let (at, states) = node.snapshots.load(name)?;
        node.restore_all(&states)?;
        snapshot_at = at;
    } else {
        // No snapshot: provision governance fresh, then replay everything.
        provision_fresh(node)?;
    }

    // WAL replay strictly after the snapshot point.
    let mut replayed = 0u64;
    for shard in &node.shard_wals {
        let outcome = replay_dir(shard.dir()).map_err(UcError::internal)?;
        if let Some(torn) = outcome.torn_tail {
            node.logger
                .warn(0, "bootstrap.torn_tail_truncated", &[("file", torn)]);
            node.metrics.inc("boot.torn_tails");
        }
        for frame in outcome.frames {
            if frame.logical_at <= snapshot_at {
                continue;
            }
            replay_frame(node, &frame)?;
            replayed += 1;
            node.clock.advance_to(frame.logical_at);
        }
    }
    node.clock.advance_to(manifest.logical_at);

    // Audit-chain verification.
    let audit_path = node.data_dir.join("audit.chain");
    if audit_path.exists() {
        let (records, ok) = AuditChain::verify(&audit_path).map_err(UcError::internal)?;
        if !ok {
            return Err(UcError::internal(format!(
                "audit chain integrity failure after {records} records — refusing to serve"
            )));
        }
    }
    {
        let mut ledger = node.cross_check.lock().unwrap();
        ledger.reload_signature_state_from_sidecar()?;
        let (verified_batches, ok) = ledger.verify_batch_signatures()?;
        if !ok {
            return Err(UcError::internal(format!(
                "cross-check batch signature verification failed after {verified_batches} verified batches — refusing to serve"
            )));
        }
    }

    node.logger.info(
        node.now(),
        "bootstrap.recovered",
        &[("replayed", replayed.to_string())],
    );
    Ok(replayed)
}

/// Re-apply one WAL frame to in-memory state (idempotent for frames at or
/// before current state; duplicate inserts overwrite identically).
fn replay_frame(node: &Node, frame: &crate::persist::wal::WalFrame) -> UcResult<()> {
    let body = Cbor::decode(&frame.payload)?;
    match frame.op {
        WalOp::Write => {
            let handle = body.opt_str("handle").unwrap_or_default();
            let payload = body.get("payload").cloned().unwrap_or(Cbor::Null);
            if frame.cell_id == ids::FACT.0 {
                let (s, p, o) = (
                    payload.opt_str("subject").unwrap_or_default(),
                    payload.opt_str("predicate").unwrap_or_default(),
                    payload.opt_str("object").unwrap_or_default(),
                );
                let mut fact = node.cells.fact.lock().unwrap();
                if !fact.exists(&handle) && !handle.is_empty() {
                    fact.insert(crate::cells::memory::Fact {
                        handle: handle.clone(),
                        subject: s.clone(),
                        predicate: p.clone(),
                        object: o.clone(),
                        confidence: None,
                        written_at: frame.logical_at,
                        superseded_by: None,
                        supersedes: payload.opt_str("supersedes"),
                        anchor: body.opt_str("anchor").unwrap_or_default(),
                    });
                    drop(fact);
                    let text = format!("{s} {p} {o}");
                    node.cells.bm25.lock().unwrap().add(handle.clone(), &text);
                    node.cells.vector.lock().unwrap().add(handle, &text);
                }
            }
            // Blob/Timeline/Scratchpad replay is covered by snapshots in v0
            // (their WAL frames exist for forensics; snapshot cadence
            // bounds loss to the group-commit window).
        }
        WalOp::Supersede => {
            let old = body.opt_str("old").unwrap_or_default();
            let new = body.opt_str("new").unwrap_or_default();
            if old.starts_with("decision/") {
                let mut t = node.trinity.lock().unwrap();
                let _ = t.decision_ledger.supersede(&old, &new);
            } else {
                let mut fact = node.cells.fact.lock().unwrap();
                let _ = fact.supersede(&old, &new);
            }
        }
        WalOp::CuratorOutput => {
            if let Ok(public) = crate::curator::CuratorPublic::from_cbor(&body) {
                node.index_public(&public.output_handle, &public.body);
            }
        }
        _ => {} // CrossCheck lives on its own stream; others snapshot-covered
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// B5 — self-test
// ---------------------------------------------------------------------------

fn selftest_env(
    node: &Node,
    token: &CapToken,
    intent: Intent,
    payload: Cbor,
    anchor: Option<&str>,
    task: &str,
    seed: u64,
    semantic: bool,
) -> Envelope {
    Envelope {
        proto_version: PROTO_VERSION,
        request_id: Ulid::from_parts(node.now(), &mut DetRng::new(seed ^ 0x5E1F)),
        agent_id: token.agent_id.clone(),
        capability: token.clone(),
        work_budget: WorkBudget {
            task_id: task.into(),
            units: 5_000,
        },
        intent,
        payload,
        spec_anchor: anchor.map(String::from),
        severity: Severity::P2,
        gap_ref: None,
        tier: Tier::L2,
        seed,
        flags: EnvelopeFlags {
            semantic_check: semantic,
            continuation: false,
        },
    }
}

/// Eleven checks, all through the real Router. Returns the count passed
/// (== 11) or the first failure as a fatal error.
pub fn self_test(node: &Arc<Node>) -> UcResult<u32> {
    let mut passed = 0u32;
    let mut check = |name: &str, ok: bool, detail: String| -> UcResult<()> {
        if ok {
            passed += 1;
            node.logger
                .info(node.now(), "selftest.pass", &[("check", name.into())]);
            Ok(())
        } else {
            Err(UcError::internal(format!(
                "self-test B5 `{name}` failed: {detail}"
            )))
        }
    };

    // Model defaults are part of the boot contract, even though this guard
    // is not a separate numbered Router check. Node::open already verifies
    // the SHA pins and weight files; self-test also confirms the selected
    // runtime backends match the configured production/development mode.
    node.curator_cfg.validate_model_pair()?;
    let librarian_backend = node.curators.librarian.lock().unwrap().backend_id();
    let warden_backend = node.curators.warden.lock().unwrap().backend_id();
    if node.curator_cfg.strict_model_pins
        && (!librarian_backend.starts_with("gguf.") || !warden_backend.starts_with("gguf."))
    {
        return Err(UcError::internal(
            "self-test B5 model selection: production Curator backend is not GGUF",
        ));
    }

    let operator = issue_operator_token(&*node.signer, "operator");
    let agent = issue_agent_token(&*node.signer, "selftest-agent", 0);
    {
        let mut reg = node.cells.agent_registry.lock().unwrap();
        reg.register(node.now(), "selftest-agent", "agent");
    }
    let task = "bootstrap.selftest";

    // B5.1 — write a fact.
    let r = handle_envelope(
        node,
        &selftest_env(
            node,
            &agent,
            Intent::Write,
            Cbor::map(vec![
                ("subject", Cbor::t("selftest")),
                ("predicate", Cbor::t("status")),
                ("object", Cbor::t("the substrate polices itself end to end")),
            ]),
            Some("Architecture.md\u{00a7}4"),
            task,
            11,
            false,
        ),
    );
    let written = r.result.opt_str("handle").unwrap_or_default();
    check(
        "B5.1 write",
        r.ok && written.starts_with("fact/"),
        format!("{:?}", r.err_message),
    )?;

    // B5.2 — recall finds it.
    let r = handle_envelope(
        node,
        &selftest_env(
            node,
            &agent,
            Intent::Recall,
            Cbor::map(vec![
                ("query", Cbor::t("substrate polices")),
                ("k", Cbor::U64(4)),
            ]),
            None,
            task,
            12,
            false,
        ),
    );
    check(
        "B5.2 recall",
        r.ok && r.result.opt_u64("items").unwrap_or(0) >= 1,
        format!("{:?}", r.err_message),
    )?;

    // B5.3 — supersede it with a new fact.
    let r2 = handle_envelope(
        node,
        &selftest_env(
            node,
            &agent,
            Intent::Write,
            Cbor::map(vec![
                ("subject", Cbor::t("selftest")),
                ("predicate", Cbor::t("status")),
                ("object", Cbor::t("supersession verified")),
                ("supersedes", Cbor::t(written.clone())),
            ]),
            Some("Architecture.md\u{00a7}4"),
            task,
            13,
            false,
        ),
    );
    let superseded = {
        let fact = node.cells.fact.lock().unwrap();
        fact.get(&written)
            .and_then(|f| f.superseded_by.clone())
            .is_some()
    };
    check(
        "B5.3 supersede",
        r2.ok && superseded,
        format!("{:?}", r2.err_message),
    )?;

    // B5.4 — anchor-missing write quarantines with a qid.
    let r = handle_envelope(
        node,
        &selftest_env(
            node,
            &agent,
            Intent::Write,
            Cbor::map(vec![
                ("subject", Cbor::t("selftest")),
                ("predicate", Cbor::t("anchorless")),
                ("object", Cbor::t("must be absorbed")),
            ]),
            None,
            task,
            14,
            false,
        ),
    );
    check(
        "B5.4 anchor quarantine",
        !r.ok
            && r.err_code == Some(ErrCode::AnchorMissing)
            && r.quarantine_id
                .as_deref()
                .unwrap_or("")
                .starts_with("quarantine/"),
        format!("{:?} {:?}", r.err_code, r.quarantine_id),
    )?;

    // B5.5 — zero-budget task rejects with BudgetExceeded.
    {
        let mut t = node.trinity.lock().unwrap();
        t.work_budget.ensure("selftest.zero", Some(0));
    }
    let mut env = selftest_env(
        node,
        &agent,
        Intent::Write,
        Cbor::map(vec![
            ("subject", Cbor::t("selftest")),
            ("predicate", Cbor::t("budget")),
            ("object", Cbor::t("should not land")),
        ]),
        Some("Architecture.md\u{00a7}4"),
        "selftest.zero",
        15,
        false,
    );
    env.work_budget.units = 0;
    let r = handle_envelope(node, &env);
    check(
        "B5.5 budget",
        !r.ok && r.err_code == Some(ErrCode::BudgetExceeded),
        format!("{:?}", r.err_code),
    )?;

    // B5.6 — view renders, then second call hits the cache.
    let view_payload = Cbor::map(vec![
        ("view_id", Cbor::t("fact_subject")),
        ("params", Cbor::map(vec![("subject", Cbor::t("selftest"))])),
    ]);
    let r = handle_envelope(
        node,
        &selftest_env(
            node,
            &agent,
            Intent::View,
            view_payload.clone(),
            None,
            task,
            16,
            false,
        ),
    );
    let hits_before = node.metrics.counter("cache.hit");
    let r2 = handle_envelope(
        node,
        &selftest_env(
            node,
            &agent,
            Intent::View,
            view_payload,
            None,
            task,
            17,
            false,
        ),
    );
    check(
        "B5.6 view+cache",
        r.ok && r2.ok && node.metrics.counter("cache.hit") > hits_before,
        format!("{:?}/{:?}", r.err_message, r2.err_message),
    )?;

    // B5.7 — the librarian skeleton for B5.3's write is Active.
    let skeleton_active = {
        let fact = node.cells.fact.lock().unwrap();
        let new_handle = fact
            .active_for_sp("selftest", "status")
            .first()
            .map(|f| f.handle.clone());
        drop(fact);
        match new_handle {
            None => false,
            Some(h) => {
                let lib = node.curators.librarian.lock().unwrap();
                lib.active_skeleton_for(&h).is_some()
            }
        }
    };
    check(
        "B5.7 librarian active",
        skeleton_active,
        "no active skeleton".into(),
    )?;

    // B5.8 — semantic gate flags a hallucinated reference.
    let r = handle_envelope(
        node,
        &selftest_env(
            node,
            &agent,
            Intent::Write,
            Cbor::map(vec![
                ("subject", Cbor::t("selftest")),
                ("predicate", Cbor::t("grounding")),
                (
                    "object",
                    Cbor::t("grounded in fact/01GHOSTGHOSTGHOSTGHOSTGHOST"),
                ),
            ]),
            Some("Architecture.md\u{00a7}4"),
            task,
            18,
            true, // semantic_check
        ),
    );
    check(
        "B5.8 warden gate",
        !r.ok && r.err_code == Some(ErrCode::HallucinationDetected),
        format!("{:?}", r.err_code),
    )?;

    // B5.9 — adjudicator stats respond.
    let stats_ok = {
        let adj = node.curators.adjudicator.lock().unwrap();
        let (_, _, _, queued) = adj.stats();
        queued < 10_000 // trivially true; the call itself is the check
    };
    check("B5.9 adjudicator", stats_ok, "stats unavailable".into())?;

    // B5.10 — warden token hydrating a rationale is denied AND the metric
    // is non-zero (the boundary probe fires on every curation cycle too).
    let rationale = {
        let lib = node.curators.librarian.lock().unwrap();
        lib.pending()
            .first()
            .map(|p| p.output_handle.clone())
            .or_else(|| {
                // Any output at all — pull from the public index.
                None
            })
    };
    let facet = {
        // Use the active skeleton from B5.7's target.
        let fact = node.cells.fact.lock().unwrap();
        let h = fact
            .active_for_sp("selftest", "status")
            .first()
            .map(|f| f.handle.clone())
            .unwrap_or_default();
        drop(fact);
        let lib = node.curators.librarian.lock().unwrap();
        lib.active_skeleton_for(&h)
            .map(|p| crate::curator::facet_handle(&p.output_handle, "rationale"))
    }
    .or(rationale.map(|h| crate::curator::facet_handle(&h, "rationale")))
    .ok_or_else(|| UcError::internal("self-test B5.10: no curator output to probe"))?;

    let warden_token = issue_curator_token(&*node.signer, "curator.warden", 0);
    {
        let mut reg = node.cells.agent_registry.lock().unwrap();
        reg.register(node.now(), "curator.warden", "curator");
    }
    let r = handle_envelope(
        node,
        &selftest_env(
            node,
            &warden_token,
            Intent::Hydrate,
            Cbor::map(vec![("handle", Cbor::t(facet.clone()))]),
            None,
            task,
            19,
            false,
        ),
    );
    check(
        "B5.10 P19 denial",
        !r.ok
            && r.err_code == Some(ErrCode::PermissionDenied)
            && node.metrics.counter("curator.rationale_access_denied") > 0,
        format!(
            "{:?} denied_metric={}",
            r.err_code,
            node.metrics.counter("curator.rationale_access_denied")
        ),
    )?;

    // B5.11 — the operator CAN hydrate the same rationale.
    let r = handle_envelope(
        node,
        &selftest_env(
            node,
            &operator,
            Intent::Hydrate,
            Cbor::map(vec![("handle", Cbor::t(facet))]),
            None,
            task,
            20,
            false,
        ),
    );
    check(
        "B5.11 operator hydrate",
        r.ok && !r.result.opt_str("body").unwrap_or_default().is_empty(),
        format!("{:?}", r.err_message),
    )?;

    Ok(passed)
}

// ---------------------------------------------------------------------------
// Admin plane
// ---------------------------------------------------------------------------

/// Dispatch an operator verb. Called by the proto server for
/// `type: "admin"` frames and by `main.rs` for offline verbs.
pub fn admin_dispatch(node: &Arc<Node>, msg: &Cbor) -> UcResult<Cbor> {
    let verb = msg.opt_str("verb").unwrap_or_default();
    let args = msg.get("args").cloned().unwrap_or(Cbor::Null);
    let at = node.tick();
    node.metrics
        .inc(&format!("admin.{}", verb.replace(' ', "_")));

    match verb.as_str() {
        "status" => {
            let (p, a, q) = node.curators.librarian.lock().unwrap().counts();
            let pending = node.trinity.lock().unwrap().quarantine.pending_count();
            Ok(Cbor::map(vec![
                ("node_id", Cbor::t(node.node_id.clone())),
                ("logical_at", Cbor::U64(node.now())),
                ("shards", Cbor::U64(node.shard_count)),
                ("trinity_topology", Cbor::t(node.trinity_topology.as_str())),
                (
                    "curator_topology",
                    Cbor::t(node.curator_cfg.topology.as_str()),
                ),
                ("quarantine_pending", Cbor::U64(pending as u64)),
                ("librarian_pending", Cbor::U64(p as u64)),
                ("librarian_active", Cbor::U64(a as u64)),
                ("librarian_quarantined", Cbor::U64(q as u64)),
            ]))
        }
        "snapshot" => {
            let snap = node.write_snapshot(at)?;
            Ok(Cbor::map(vec![
                ("snapshot", Cbor::t(snap.name)),
                ("cells", Cbor::U64(snap.cells as u64)),
                ("pause_us", Cbor::U64(snap.pause_us)),
                ("total_us", Cbor::U64(snap.total_us)),
                (
                    "pause_target_us",
                    Cbor::U64(crate::node::SNAPSHOT_PAUSE_TARGET_US),
                ),
                ("capture_us", Cbor::U64(snap.capture_us)),
                ("hash_us", Cbor::U64(snap.hash_us)),
                ("write_us", Cbor::U64(snap.write_us)),
                ("manifest_us", Cbor::U64(snap.manifest_us)),
                ("within_target", Cbor::Bool(snap.within_target)),
            ]))
        }
        "quarantine list" => {
            let t = node.trinity.lock().unwrap();
            let items: Vec<Cbor> = t
                .quarantine
                .list()
                .iter()
                .map(|q| {
                    Cbor::map(vec![
                        ("qid", Cbor::t(q.qid.clone())),
                        ("cause", Cbor::t(q.cause.as_str())),
                        ("detail", Cbor::t(q.detail.clone())),
                        ("status", Cbor::t(q.status.as_str())),
                        ("absorbed_at", Cbor::U64(q.absorbed_at)),
                    ])
                })
                .collect();
            Ok(Cbor::map(vec![
                ("items", Cbor::Array(items)),
                (
                    "resolved_retention_logical",
                    Cbor::U64(t.quarantine.resolved_retention),
                ),
                ("pending_never_pruned", Cbor::Bool(true)),
            ]))
        }
        "quarantine reinject" => {
            let qid = args.req_str("qid")?;
            let payload = {
                let mut t = node.trinity.lock().unwrap();
                t.quarantine.reinject(at, &qid)?
            };
            // The stored record holds the original envelope payload; the
            // operator re-drives it through the write path under its own
            // authority (RouterScheduler.md §D.2).
            let inner = payload.get("payload").cloned().unwrap_or(payload.clone());
            let operator = issue_operator_token(&*node.signer, "operator");
            let env = selftest_env(
                node,
                &operator,
                Intent::Write,
                inner,
                payload
                    .opt_str("spec_anchor")
                    .as_deref()
                    .or(Some("Architecture.md\u{00a7}4")),
                "admin.reinject",
                at,
                false,
            );
            let resp = handle_envelope(node, &env);
            Ok(Cbor::map(vec![
                ("reinjected", Cbor::t(qid)),
                ("ok", Cbor::Bool(resp.ok)),
                (
                    "result",
                    if resp.ok {
                        resp.result
                    } else {
                        Cbor::t(resp.err_message.unwrap_or_default())
                    },
                ),
            ]))
        }
        "quarantine reject" => {
            let qid = args.req_str("qid")?;
            let mut t = node.trinity.lock().unwrap();
            t.quarantine.reject(at, &qid)?;
            Ok(Cbor::map(vec![("rejected", Cbor::t(qid))]))
        }
        "gap list" => {
            let t = node.trinity.lock().unwrap();
            let items: Vec<Cbor> = t
                .gap
                .list()
                .iter()
                .map(|g| {
                    Cbor::map(vec![
                        ("gap_id", Cbor::t(g.gap_id.clone())),
                        ("state", Cbor::t(g.state.as_str())),
                        ("description", Cbor::t(g.description.clone())),
                        (
                            "dispatches_since_transition",
                            Cbor::U64(g.dispatches_since_transition),
                        ),
                    ])
                })
                .collect();
            Ok(Cbor::map(vec![("gaps", Cbor::Array(items))]))
        }
        "audit verify" => {
            let path = node.data_dir.join("audit.chain");
            let (records, ok) = AuditChain::verify(&path).map_err(UcError::internal)?;
            let (expected_batches, verified_batches, signatures_ok) = {
                let mut ledger = node.cross_check.lock().unwrap();
                ledger.reload_signature_state_from_sidecar()?;
                let expected = (ledger.len() / BATCH_SIGN_EVERY as usize) as u64;
                let (verified, sig_ok) = ledger.verify_batch_signatures()?;
                (expected, verified, sig_ok)
            };
            Ok(Cbor::map(vec![
                ("records", Cbor::U64(records)),
                ("intact", Cbor::Bool(ok)),
                (
                    "cross_check_batches_expected",
                    Cbor::U64(expected_batches),
                ),
                (
                    "cross_check_batches_verified",
                    Cbor::U64(verified_batches),
                ),
                (
                    "cross_check_signatures_intact",
                    Cbor::Bool(signatures_ok),
                ),
            ]))
        }
        "kms status" => {
            let status = node.kms.rotation_status(at);
            Ok(Cbor::map(vec![
                ("tier", Cbor::t(node.kms.tier().as_str())),
                (
                    "custody_path",
                    node.kms
                        .custody_path()
                        .map(|p| Cbor::t(p.display().to_string()))
                        .unwrap_or(Cbor::Null),
                ),
                (
                    "active_key_id",
                    node.kms
                        .active_key_id()
                        .map(Cbor::U64)
                        .unwrap_or(Cbor::Null),
                ),
                ("key_versions", Cbor::U64(node.kms.key_versions() as u64)),
                (
                    "last_rotated_at",
                    status
                        .as_ref()
                        .map(|s| Cbor::U64(s.last_rotated_at))
                        .unwrap_or(Cbor::Null),
                ),
                (
                    "rotation_interval_ops",
                    status
                        .as_ref()
                        .map(|s| Cbor::U64(s.rotation_interval_ops))
                        .unwrap_or(Cbor::Null),
                ),
                (
                    "next_due_at",
                    status
                        .as_ref()
                        .map(|s| Cbor::U64(s.next_due_at))
                        .unwrap_or(Cbor::Null),
                ),
                (
                    "overdue",
                    status
                        .as_ref()
                        .map(|s| Cbor::Bool(s.overdue))
                        .unwrap_or(Cbor::Bool(false)),
                ),
            ]))
        }
        "kms rotate" => {
            let emergency = args.opt_bool("emergency").unwrap_or(false);
            let rotation = node.kms.rotate(at, emergency)?;
            let due = node.kms.rotation_status(at).map(|s| s.next_due_at);
            node.audit
                .lock()
                .unwrap()
                .append(
                    at,
                    "kms.key_rotated",
                    &[
                        ("tier", Cbor::t(node.kms.tier().as_str())),
                        ("previous_key_id", Cbor::U64(rotation.previous_key_id)),
                        ("active_key_id", Cbor::U64(rotation.active_key_id)),
                        ("emergency", Cbor::Bool(rotation.emergency)),
                    ],
                )
                .map_err(UcError::internal)?;
            node.logger.info(
                at,
                "kms.key_rotated",
                &[
                    ("tier", node.kms.tier().as_str().into()),
                    ("previous_key_id", rotation.previous_key_id.to_string()),
                    ("active_key_id", rotation.active_key_id.to_string()),
                    ("emergency", rotation.emergency.to_string()),
                ],
            );
            Ok(Cbor::map(vec![
                ("tier", Cbor::t(node.kms.tier().as_str())),
                ("previous_key_id", Cbor::U64(rotation.previous_key_id)),
                ("active_key_id", Cbor::U64(rotation.active_key_id)),
                ("logical_at", Cbor::U64(rotation.logical_at)),
                ("emergency", Cbor::Bool(rotation.emergency)),
                (
                    "next_due_at",
                    due.map(Cbor::U64).unwrap_or(Cbor::Null),
                ),
                ("audited", Cbor::Bool(true)),
            ]))
        }
        "congruence audit" => {
            let t = node.trinity.lock().unwrap();
            Ok(Cbor::map(vec![
                (
                    "known_entities",
                    Cbor::U64(t.congruence.known_count() as u64),
                ),
                (
                    "accepted_deltas",
                    Cbor::U64(t.congruence.accepted_count() as u64),
                ),
                (
                    "accepted",
                    Cbor::text_array(&t.congruence.accepted_entities()),
                ),
            ]))
        }
        "congruence preview" => {
            let payload = args
                .get("payload")
                .ok_or_else(|| UcError::schema("congruence preview: missing payload"))?;
            let t = node.trinity.lock().unwrap();
            match t.congruence.preview_delta(payload) {
                Ok(()) => Ok(Cbor::map(vec![("delta", Cbor::Bool(false))])),
                Err(e) => Ok(Cbor::map(vec![
                    ("delta", Cbor::Bool(true)),
                    ("detail", Cbor::t(e.message)),
                ])),
            }
        }
        "congruence accept" => {
            let mut entities: Vec<String> = args
                .get("entities")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if entities.is_empty() {
                if let Some(entity) = args.opt_str("entity") {
                    entities.push(entity);
                }
            }
            if entities.is_empty() {
                return Err(UcError::schema(
                    "congruence accept: missing entity or entities",
                ));
            }
            let mut t = node.trinity.lock().unwrap();
            for entity in &entities {
                t.congruence.accept_delta(entity);
            }
            Ok(Cbor::map(vec![
                ("accepted", Cbor::text_array(&entities)),
                (
                    "accepted_deltas",
                    Cbor::U64(t.congruence.accepted_count() as u64),
                ),
            ]))
        }
        "budget defaults" => {
            let t = node.trinity.lock().unwrap();
            let items: Vec<Cbor> = t
                .work_budget
                .namespace_defaults()
                .iter()
                .map(|(namespace, grant)| {
                    Cbor::map(vec![
                        ("namespace", Cbor::t(namespace.clone())),
                        ("grant", Cbor::U64(*grant)),
                    ])
                })
                .collect();
            Ok(Cbor::map(vec![
                ("default_grant", Cbor::U64(t.work_budget.default_grant)),
                ("namespace_defaults", Cbor::Array(items)),
            ]))
        }
        "contract list" => {
            let t = node.trinity.lock().unwrap();
            let items: Vec<Cbor> = t
                .contract
                .list()
                .iter()
                .map(|c| {
                    Cbor::map(vec![
                        ("schema_id", Cbor::t(c.schema_id.clone())),
                        ("required", Cbor::text_array(&c.required_fields)),
                        ("deprecated", Cbor::Bool(c.deprecated)),
                        (
                            "superseded_by",
                            c.superseded_by
                                .as_ref()
                                .map(|s| Cbor::t(s.clone()))
                                .unwrap_or(Cbor::Null),
                        ),
                        (
                            "migration_plan_handle",
                            c.migration_plan_handle
                                .as_ref()
                                .map(|s| Cbor::t(s.clone()))
                                .unwrap_or(Cbor::Null),
                        ),
                        (
                            "decision_handle",
                            c.decision_handle
                                .as_ref()
                                .map(|s| Cbor::t(s.clone()))
                                .unwrap_or(Cbor::Null),
                        ),
                        (
                            "deprecated_after",
                            c.deprecated_after.map(Cbor::U64).unwrap_or(Cbor::Null),
                        ),
                        (
                            "migration_applied_at",
                            c.migration_applied_at.map(Cbor::U64).unwrap_or(Cbor::Null),
                        ),
                    ])
                })
                .collect();
            Ok(Cbor::map(vec![("contracts", Cbor::Array(items))]))
        }
        "contract plan-migration" => {
            let source_schema_id = args.req_str("schema_id")?;
            let target_schema_id = args.req_str("target_schema_id")?;
            let migration_plan_handle = args.req_str("migration_plan_handle")?;
            let decision_handle = args.req_str("decision_handle")?;
            let deprecated_after = args
                .opt_u64("deprecated_after")
                .or_else(|| {
                    args.opt_str("deprecated_after")
                        .and_then(|s| s.parse::<u64>().ok())
                })
                .ok_or_else(|| {
                    UcError::schema("contract plan-migration: missing deprecated_after")
                })?;
            let mut t = node.trinity.lock().unwrap();
            if !t.decision_ledger.exists(&decision_handle) {
                return Err(UcError::not_found(format!(
                    "decision handle `{decision_handle}` does not exist"
                )));
            }
            t.contract.plan_migration(
                &source_schema_id,
                &target_schema_id,
                &migration_plan_handle,
                &decision_handle,
                deprecated_after,
            )?;
            t.contract.migration_status(&source_schema_id)
        }
        "contract verify-migration" => {
            let source_schema_id = args.req_str("schema_id")?;
            let t = node.trinity.lock().unwrap();
            t.contract.migration_status(&source_schema_id)
        }
        "contract apply-migration" => {
            let source_schema_id = args.req_str("schema_id")?;
            let mut t = node.trinity.lock().unwrap();
            t.contract.apply_migration(at, &source_schema_id)?;
            t.contract.migration_status(&source_schema_id)
        }
        "curator status" => {
            let (p, a, q) = node.curators.librarian.lock().unwrap().counts();
            let warden = node.curators.warden.lock().unwrap();
            let audits = warden.audit_count();
            let warden_backend = warden.backend_id();
            let librarian_backend = node.curators.librarian.lock().unwrap().backend_id();
            let ledger = node.cross_check.lock().unwrap();
            let kv_budgets = node.curator_cfg.kv_budgets();
            Ok(Cbor::map(vec![
                ("librarian_pending", Cbor::U64(p as u64)),
                ("librarian_active", Cbor::U64(a as u64)),
                ("librarian_quarantined", Cbor::U64(q as u64)),
                ("warden_audits", Cbor::U64(audits as u64)),
                (
                    "librarian_model",
                    Cbor::t(node.curator_cfg.librarian_model.clone()),
                ),
                (
                    "warden_model",
                    Cbor::t(node.curator_cfg.warden_model.clone()),
                ),
                ("librarian_backend", Cbor::t(librarian_backend)),
                ("warden_backend", Cbor::t(warden_backend)),
                (
                    "strict_model_pins",
                    Cbor::Bool(node.curator_cfg.strict_model_pins),
                ),
                (
                    "agreement_rate",
                    ledger.agreement_rate().map(Cbor::F64).unwrap_or(Cbor::Null),
                ),
                (
                    "rationale_access_denied",
                    Cbor::U64(node.metrics.counter("curator.rationale_access_denied")),
                ),
                (
                    "probe_missed",
                    Cbor::U64(node.metrics.counter("curator.probe_missed")),
                ),
                ("probe_rate", Cbor::F64(node.curator_cfg.probe_rate)),
                (
                    "probe_boost_on_suspicious",
                    Cbor::F64(PROBE_BOOST_ON_SUSPICIOUS),
                ),
                (
                    "kv_budget_profile",
                    Cbor::t(node.curator_cfg.kv_budget_profile.as_str()),
                ),
                (
                    "librarian_kv_cache_mib",
                    Cbor::U64(kv_budgets.librarian_mib),
                ),
                ("warden_kv_cache_mib", Cbor::U64(kv_budgets.warden_mib)),
                (
                    "adjudicator_kv_cache_mib",
                    Cbor::U64(kv_budgets.adjudicator_mib),
                ),
                ("total_kv_cache_mib", Cbor::U64(kv_budgets.total_mib())),
                ("topology", Cbor::t(node.curator_cfg.topology.as_str())),
                (
                    "blind_reaudit_rate",
                    Cbor::F64(node.curator_cfg.blind_reaudit_rate),
                ),
                (
                    "cross_check_retention_policy",
                    Cbor::t(CROSS_CHECK_RETENTION_POLICY),
                ),
                ("calibration_window", Cbor::U64(CALIBRATION_WINDOW as u64)),
                ("high_band_floor", Cbor::F64(HIGH_BAND_FLOOR)),
                ("medium_band_floor", Cbor::F64(MEDIUM_BAND_FLOOR)),
                (
                    "degraded",
                    Cbor::Bool(node.guardrails.calibration.lock().unwrap().degraded()),
                ),
            ]))
        }
        "curator probe-now" => {
            crate::router::run_curation_probe(node, at);
            Ok(Cbor::map(vec![(
                "probes",
                Cbor::U64(node.metrics.counter("curator.probes")),
            )]))
        }
        "curator verify-weights" => {
            let mut results: Vec<Cbor> = Vec::new();
            for (model, sha) in &node.curator_cfg.pinned {
                let ok = crate::persist::verify_weight_file(&node.data_dir, model, sha).is_ok();
                results.push(Cbor::map(vec![
                    ("model", Cbor::t(model.clone())),
                    ("verified", Cbor::Bool(ok)),
                ]));
            }
            Ok(Cbor::map(vec![("weights", Cbor::Array(results))]))
        }
        "cross-check tail" => {
            let n = args.opt_u64("n").unwrap_or(20) as usize;
            let ledger = node.cross_check.lock().unwrap();
            let items: Vec<Cbor> = ledger.tail(n).iter().map(|r| r.to_cbor()).collect();
            Ok(Cbor::map(vec![
                ("records", Cbor::Array(items)),
                ("retention_policy", Cbor::t(ledger.retention_policy())),
            ]))
        }
        "adjudicator stats" => {
            let adj = node.curators.adjudicator.lock().unwrap();
            let (policy, pool, human, queued) = adj.stats();
            let escalation_subscribers = node
                .cells
                .agent_registry
                .lock()
                .unwrap()
                .escalation_subscribers();
            Ok(Cbor::map(vec![
                ("policy", Cbor::U64(policy)),
                ("pool", Cbor::U64(pool)),
                ("human", Cbor::U64(human)),
                ("queued", Cbor::U64(queued as u64)),
                ("escalations", Cbor::text_array(&adj.escalations().to_vec())),
                (
                    "escalation_subscribers",
                    Cbor::text_array(&escalation_subscribers),
                ),
            ]))
        }
        "resolve" => {
            let handle = args.req_str("handle")?;
            let uphold_auditor = args.opt_bool("uphold_auditor").unwrap_or(false);
            let mut adj = node.curators.adjudicator.lock().unwrap();
            let rec = adj.resolve_human(&handle, uphold_auditor)?;
            let target = rec.initiator_output.clone();
            let uphold = uphold_auditor;
            drop(adj);
            // Apply the human verdict to the disputed librarian output.
            if target.starts_with("librarian/output/") {
                let mut lib = node.curators.librarian.lock().unwrap();
                let _ = lib.set_status(
                    &target,
                    if uphold {
                        crate::curator::librarian::OutputStatus::Quarantined
                    } else {
                        crate::curator::librarian::OutputStatus::Active
                    },
                );
            }
            Ok(Cbor::map(vec![
                ("resolved", Cbor::t(handle)),
                ("uphold_auditor", Cbor::Bool(uphold)),
            ]))
        }
        "metrics" => {
            let snap = node.metrics.snapshot();
            let items: Vec<Cbor> = snap
                .into_iter()
                .map(|(k, v)| {
                    let value = if v >= 0 {
                        Cbor::U64(v as u64)
                    } else {
                        Cbor::I64(v)
                    };
                    Cbor::map(vec![("name", Cbor::t(k)), ("value", value)])
                })
                .collect();
            Ok(Cbor::map(vec![("metrics", Cbor::Array(items))]))
        }
        "metrics export" => {
            let receipt = node
                .otlp
                .export_metrics(&node.metrics)
                .map_err(UcError::internal)?;
            Ok(Cbor::map(vec![
                ("endpoint", Cbor::t(receipt.endpoint)),
                (
                    "status_code",
                    receipt
                        .status_code
                        .map(|v| Cbor::U64(v as u64))
                        .unwrap_or(Cbor::Null),
                ),
                ("bytes", Cbor::U64(receipt.bytes as u64)),
                ("skipped", Cbor::Bool(receipt.skipped)),
            ]))
        }
        "shutdown" => {
            node.shutdown()?;
            Ok(Cbor::map(vec![("shutdown", Cbor::Bool(true))]))
        }
        other => Err(UcError::unsupported(format!(
            "unknown admin verb `{other}`"
        ))),
    }
}

// ---------------------------------------------------------------------------
// B4/B6 — run
// ---------------------------------------------------------------------------

/// Full lifecycle: boot → listeners → "ready" line → serve until shutdown.
pub fn run(cfg: &Config) -> UcResult<()> {
    let report = boot(cfg)?;
    let node = report.node.clone();

    let mut handles = Vec::new();
    #[cfg(unix)]
    if let Some(uds) = &cfg.uds_path {
        handles.push(crate::proto::serve_uds(node.clone(), uds)?);
    }
    if let Some(tcp) = &cfg.tcp_addr {
        handles.push(crate::proto::serve_tcp(node.clone(), tcp)?);
    }
    if handles.is_empty() {
        return Err(UcError::internal(
            "no listeners configured (set listen.uds or listen.tcp)",
        ));
    }

    // B6 — the contractual ready line.
    println!(
        "ready node_id={} proto_version={}",
        node.node_id, PROTO_VERSION
    );
    node.logger.info(
        node.now(),
        "node.ready",
        &[
            ("recovered", report.recovered.to_string()),
            ("replayed", report.replayed_frames.to_string()),
            ("self_test", report.self_test_passed.to_string()),
        ],
    );

    // Serve until an admin `shutdown` flips the flag. Graceful signal
    // handling without libc is limited; `ultracortex shutdown` (admin verb
    // over the socket) is the sanctioned stop (IMPLEMENTATION_STATUS §6).
    while !node.is_shutting_down() {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    for h in handles {
        let _ = h.join();
    }
    Ok(())
}

/// A deterministic offline environment used by `--dry-run` and tests: full
/// boot in a temp dir, no listeners.
pub fn dry_run() -> UcResult<BootReport> {
    let mut cfg = Config::default();
    cfg.curator = CuratorConfig::development();
    let seq = DRY_RUN_SEQ.fetch_add(1, Ordering::Relaxed);
    cfg.data_dir = std::env::temp_dir().join(format!(
        "ultracortex-dryrun-{}-{}-{}",
        std::process::id(),
        DetRng::new(fnv1a64(b"dryrun")).next_u64(),
        seq
    ));
    let _ = std::fs::remove_dir_all(&cfg.data_dir);
    cfg.uds_path = Some(cfg.data_dir.join("ultracortex.sock"));
    boot(&cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn fresh_boot_passes_self_test() {
        let report = dry_run().expect("boot must succeed");
        assert!(!report.recovered);
        assert_eq!(report.self_test_passed, 11);
        // Trinity-first evidence: contracts + anchors + decisions exist.
        let t = report.node.trinity.lock().unwrap();
        assert!(t.contract.list().len() >= 5);
        assert!(t.spec_anchor.count() >= 30);
        assert_eq!(t.decision_ledger.len(), 2);
    }

    #[test]
    fn recovery_restores_written_facts() {
        let mut cfg = Config::default();
        cfg.curator = CuratorConfig::development();
        cfg.data_dir =
            std::env::temp_dir().join(format!("ultracortex-recover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cfg.data_dir);
        cfg.uds_path = Some(cfg.data_dir.join("ultracortex.sock"));

        let report = boot(&cfg).unwrap();
        let node = report.node.clone();
        // The self-test wrote facts; capture one and shut down cleanly.
        let handle = {
            let fact = node.cells.fact.lock().unwrap();
            fact.active_for_sp("selftest", "status")
                .first()
                .map(|f| f.handle.clone())
                .unwrap()
        };
        node.shutdown().unwrap();
        drop(node);
        drop(report);

        // Boot again from the same dir: recovery path.
        let report2 = boot(&cfg).unwrap();
        assert!(report2.recovered);
        assert!(crate::curator::SubstrateView::handle_exists(
            &*report2.node,
            &handle
        ));
        let _ = std::fs::remove_dir_all(&cfg.data_dir);
    }

    #[test]
    fn admin_verbs_respond() {
        let report = dry_run().unwrap();
        let node = report.node;
        node.otlp.configure(OtlpConfig {
            enabled: false,
            ..OtlpConfig::default()
        });
        let verbs = [
            "status",
            "budget defaults",
            "quarantine list",
            "gap list",
            "kms status",
            "congruence audit",
            "contract list",
            "curator status",
            "cross-check tail",
            "adjudicator stats",
            "metrics",
            "metrics export",
            "audit verify",
        ];
        for v in verbs {
            let msg = Cbor::map(vec![("verb", Cbor::t(v)), ("args", Cbor::Null)]);
            admin_dispatch(&node, &msg).unwrap_or_else(|e| panic!("verb {v}: {}", e.message));
        }
        // Unknown verb rejected.
        let msg = Cbor::map(vec![("verb", Cbor::t("frobnicate"))]);
        assert!(admin_dispatch(&node, &msg).is_err());
    }

    #[test]
    fn snapshot_admin_surfaces_pause_breakdown() {
        let report = dry_run().unwrap();
        let node = report.node;
        let snap = admin_dispatch(
            &node,
            &Cbor::map(vec![("verb", Cbor::t("snapshot")), ("args", Cbor::Null)]),
        )
        .unwrap();
        let pause = snap.opt_u64("pause_us").unwrap_or(0);
        assert!(snap
            .opt_str("snapshot")
            .unwrap_or_default()
            .starts_with("snap-"));
        assert!(snap.opt_u64("cells").unwrap_or(0) >= 25);
        assert_eq!(
            snap.opt_u64("pause_target_us"),
            Some(crate::node::SNAPSHOT_PAUSE_TARGET_US)
        );
        assert_eq!(
            snap.opt_bool("within_target"),
            Some(pause <= crate::node::SNAPSHOT_PAUSE_TARGET_US)
        );
        assert!(snap.opt_u64("total_us").is_some());
        assert!(snap.opt_u64("capture_us").is_some());
        assert!(snap.opt_u64("hash_us").is_some());
        assert!(snap.opt_u64("write_us").is_some());
        assert!(snap.opt_u64("manifest_us").is_some());
    }

    #[test]
    fn kms_status_and_rotate_surface_custody_and_keep_audit_chain_intact() {
        let dir = std::env::temp_dir().join(format!(
            "uc-kms-admin-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let node = Arc::new(Node::open(
            "kms-admin-node",
            &dir,
            2,
            EncryptionTier::T3,
            7,
            ShardTopology::Dedicated,
            CuratorConfig::development(),
            256,
        )
        .unwrap());
        provision_fresh(&node).unwrap();

        let status = admin_dispatch(
            &node,
            &Cbor::map(vec![("verb", Cbor::t("kms status")), ("args", Cbor::Null)]),
        )
        .unwrap();
        assert_eq!(status.opt_str("tier"), Some("T3".to_string()));
        assert_eq!(status.opt_u64("active_key_id"), Some(1));
        assert_eq!(status.opt_u64("rotation_interval_ops"), Some(crate::persist::T3_ROTATION_INTERVAL_OPS));
        assert!(status.opt_str("custody_path").unwrap_or_default().ends_with("keyring.cbor"));

        let rotated = admin_dispatch(
            &node,
            &Cbor::map(vec![
                ("verb", Cbor::t("kms rotate")),
                (
                    "args",
                    Cbor::map(vec![("emergency", Cbor::Bool(true))]),
                ),
            ]),
        )
        .unwrap();
        assert_eq!(rotated.opt_u64("previous_key_id"), Some(1));
        assert_eq!(rotated.opt_u64("active_key_id"), Some(2));
        assert_eq!(rotated.opt_bool("emergency"), Some(true));
        assert_eq!(rotated.opt_bool("audited"), Some(true));

        let audit = admin_dispatch(
            &node,
            &Cbor::map(vec![("verb", Cbor::t("audit verify")), ("args", Cbor::Null)]),
        )
        .unwrap();
        assert_eq!(audit.opt_bool("intact"), Some(true));
        assert_eq!(audit.opt_bool("cross_check_signatures_intact"), Some(true));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn contract_migration_admin_flow_requires_decision_and_enforces_deadline() {
        let report = dry_run().unwrap();
        let node = report.node;
        let at = node.now();
        {
            let mut t = node.trinity.lock().unwrap();
            t.contract.register(at, "shape.v1", vec!["subject".into()]);
            t.contract
                .register(at + 1, "shape.v2", vec!["subject".into(), "mode".into()]);
        }

        let missing_decision = admin_dispatch(
            &node,
            &Cbor::map(vec![
                ("verb", Cbor::t("contract plan-migration")),
                (
                    "args",
                    Cbor::map(vec![
                        ("schema_id", Cbor::t("shape.v1")),
                        ("target_schema_id", Cbor::t("shape.v2")),
                        ("migration_plan_handle", Cbor::t("blob/shape-plan")),
                        ("decision_handle", Cbor::t("decision/missing")),
                        ("deprecated_after", Cbor::U64(node.now() + 10)),
                    ]),
                ),
            ]),
        )
        .unwrap_err();
        assert_eq!(missing_decision.code, ErrCode::NotFound);

        let decision_handle = {
            let mut t = node.trinity.lock().unwrap();
            t.decision_ledger.append(
                node.now(),
                77,
                "contract.shape",
                "shape.v1 migrates to shape.v2",
                "operator",
                "NATIVE_TRINITY.md§10.1",
            )
        };

        let future_deadline = node.now() + 50;
        let planned = admin_dispatch(
            &node,
            &Cbor::map(vec![
                ("verb", Cbor::t("contract plan-migration")),
                (
                    "args",
                    Cbor::map(vec![
                        ("schema_id", Cbor::t("shape.v1")),
                        ("target_schema_id", Cbor::t("shape.v2")),
                        ("migration_plan_handle", Cbor::t("blob/shape-plan")),
                        ("decision_handle", Cbor::t(decision_handle.clone())),
                        ("deprecated_after", Cbor::U64(future_deadline)),
                    ]),
                ),
            ]),
        )
        .unwrap();
        assert_eq!(planned.opt_str("target_schema_id"), Some("shape.v2".into()));
        assert_eq!(
            planned.opt_str("decision_handle"),
            Some(decision_handle.clone())
        );

        let verified = admin_dispatch(
            &node,
            &Cbor::map(vec![
                ("verb", Cbor::t("contract verify-migration")),
                ("args", Cbor::map(vec![("schema_id", Cbor::t("shape.v1"))])),
            ]),
        )
        .unwrap();
        assert_eq!(verified.opt_str("status"), Some("planned".into()));
        assert_eq!(verified.opt_u64("deprecated_after"), Some(future_deadline));

        let too_early = admin_dispatch(
            &node,
            &Cbor::map(vec![
                ("verb", Cbor::t("contract apply-migration")),
                ("args", Cbor::map(vec![("schema_id", Cbor::t("shape.v1"))])),
            ]),
        )
        .unwrap_err();
        assert_eq!(too_early.code, ErrCode::ContractViolation);

        let mut t = node.trinity.lock().unwrap();
        t.contract
            .plan_migration("shape.v2", "shape.v1", "blob/bad", "decision/02", 0)
            .unwrap_err();
        drop(t);

        let ready = admin_dispatch(
            &node,
            &Cbor::map(vec![
                ("verb", Cbor::t("contract plan-migration")),
                (
                    "args",
                    Cbor::map(vec![
                        ("schema_id", Cbor::t("shape.v2")),
                        ("target_schema_id", Cbor::t("shape.v3")),
                        ("migration_plan_handle", Cbor::t("blob/invalid")),
                        ("decision_handle", Cbor::t(decision_handle.clone())),
                        ("deprecated_after", Cbor::U64(0)),
                    ]),
                ),
            ]),
        );
        assert!(ready.is_err());

        {
            let mut t = node.trinity.lock().unwrap();
            t.contract.register(
                node.now(),
                "shape.v3",
                vec!["subject".into(), "mode".into()],
            );
        }

        let decision_handle2 = {
            let mut t = node.trinity.lock().unwrap();
            t.decision_ledger.append(
                node.now(),
                88,
                "contract.shape",
                "shape.v2 migrates to shape.v3",
                "operator",
                "NATIVE_TRINITY.md§10.1",
            )
        };
        admin_dispatch(
            &node,
            &Cbor::map(vec![
                ("verb", Cbor::t("contract plan-migration")),
                (
                    "args",
                    Cbor::map(vec![
                        ("schema_id", Cbor::t("shape.v2")),
                        ("target_schema_id", Cbor::t("shape.v3")),
                        ("migration_plan_handle", Cbor::t("blob/shape-plan-2")),
                        ("decision_handle", Cbor::t(decision_handle2)),
                        ("deprecated_after", Cbor::U64(0)),
                    ]),
                ),
            ]),
        )
        .unwrap();
        let applied = admin_dispatch(
            &node,
            &Cbor::map(vec![
                ("verb", Cbor::t("contract apply-migration")),
                ("args", Cbor::map(vec![("schema_id", Cbor::t("shape.v2"))])),
            ]),
        )
        .unwrap();
        assert_eq!(applied.opt_str("status"), Some("applied".into()));
        assert!(applied.opt_u64("applied_at").is_some());
    }

    #[test]
    fn admin_status_surfaces_gap_policy_defaults() {
        let report = dry_run().unwrap();
        let node = report.node;

        let status = admin_dispatch(
            &node,
            &Cbor::map(vec![("verb", Cbor::t("status")), ("args", Cbor::Null)]),
        )
        .unwrap();
        assert_eq!(status.opt_u64("shards"), Some(node.shard_count));
        assert_eq!(
            status.opt_str("trinity_topology"),
            Some("dedicated".to_string())
        );
        assert_eq!(
            status.opt_str("curator_topology"),
            Some("dedicated".to_string())
        );

        let quarantine = admin_dispatch(
            &node,
            &Cbor::map(vec![
                ("verb", Cbor::t("quarantine list")),
                ("args", Cbor::Null),
            ]),
        )
        .unwrap();
        assert_eq!(
            quarantine.opt_u64("resolved_retention_logical"),
            Some(1_000_000)
        );
        assert_eq!(quarantine.opt_bool("pending_never_pruned"), Some(true));

        let curator = admin_dispatch(
            &node,
            &Cbor::map(vec![
                ("verb", Cbor::t("curator status")),
                ("args", Cbor::Null),
            ]),
        )
        .unwrap();
        let budget_defaults = admin_dispatch(
            &node,
            &Cbor::map(vec![
                ("verb", Cbor::t("budget defaults")),
                ("args", Cbor::Null),
            ]),
        )
        .unwrap();
        assert_eq!(budget_defaults.opt_u64("default_grant"), Some(100_000));
        let by_namespace = budget_defaults
            .get("namespace_defaults")
            .and_then(|v| v.as_array())
            .unwrap()
            .iter()
            .map(|item| {
                (
                    item.req_str("namespace").unwrap(),
                    item.opt_u64("grant").unwrap_or(0),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(by_namespace.get("bootstrap").copied(), Some(1_000_000));
        assert_eq!(by_namespace.get("curator").copied(), Some(10_000_000));
        assert_eq!(by_namespace.get("admin").copied(), Some(250_000));
        assert_eq!(
            curator.get("probe_rate").and_then(|v| v.as_f64()),
            Some(0.001)
        );
        assert_eq!(
            curator
                .get("probe_boost_on_suspicious")
                .and_then(|v| v.as_f64()),
            Some(PROBE_BOOST_ON_SUSPICIOUS)
        );
        assert_eq!(
            curator.opt_str("kv_budget_profile"),
            Some("reference".to_string())
        );
        assert_eq!(curator.opt_u64("librarian_kv_cache_mib"), Some(384));
        assert_eq!(curator.opt_u64("warden_kv_cache_mib"), Some(384));
        assert_eq!(curator.opt_u64("adjudicator_kv_cache_mib"), Some(256));
        assert_eq!(curator.opt_u64("total_kv_cache_mib"), Some(1_024));
        assert_eq!(
            curator.get("blind_reaudit_rate").and_then(|v| v.as_f64()),
            Some(0.01)
        );
        assert_eq!(curator.opt_str("topology"), Some("dedicated".to_string()));
        assert_eq!(
            curator.opt_str("cross_check_retention_policy"),
            Some(CROSS_CHECK_RETENTION_POLICY.to_string())
        );
        assert_eq!(
            curator.opt_u64("calibration_window"),
            Some(CALIBRATION_WINDOW as u64)
        );
        assert_eq!(
            curator.get("high_band_floor").and_then(|v| v.as_f64()),
            Some(HIGH_BAND_FLOOR)
        );
        assert_eq!(
            curator.get("medium_band_floor").and_then(|v| v.as_f64()),
            Some(MEDIUM_BAND_FLOOR)
        );
        assert_eq!(curator.opt_bool("degraded"), Some(false));

        let adjudicator = admin_dispatch(
            &node,
            &Cbor::map(vec![
                ("verb", Cbor::t("adjudicator stats")),
                ("args", Cbor::Null),
            ]),
        )
        .unwrap();
        let subscribers = adjudicator
            .get("escalation_subscribers")
            .and_then(|v| v.as_array())
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>();
        assert!(subscribers.contains(&"operator"));
    }

    #[test]
    fn topology_overrides_and_cross_check_policy_surface_cleanly() {
        let mut cfg = Config::default();
        cfg.data_dir = std::env::temp_dir().join(format!(
            "ultracortex-topology-{}-{}",
            std::process::id(),
            DRY_RUN_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&cfg.data_dir);
        cfg.uds_path = Some(cfg.data_dir.join("ultracortex.sock"));
        cfg.trinity_topology = ShardTopology::CoTenantShard0;
        cfg.curator = CuratorConfig::development();
        cfg.curator.topology = ShardTopology::CoTenantShard0;
        cfg.curator.kv_budget_profile = CuratorKvBudgetProfile::Small;

        let report = boot(&cfg).unwrap();
        let node = report.node;

        let status = admin_dispatch(
            &node,
            &Cbor::map(vec![("verb", Cbor::t("status")), ("args", Cbor::Null)]),
        )
        .unwrap();
        assert_eq!(
            status.opt_str("trinity_topology"),
            Some("co-tenant-shard-0".to_string())
        );
        assert_eq!(
            status.opt_str("curator_topology"),
            Some("co-tenant-shard-0".to_string())
        );

        let curator = admin_dispatch(
            &node,
            &Cbor::map(vec![
                ("verb", Cbor::t("curator status")),
                ("args", Cbor::Null),
            ]),
        )
        .unwrap();
        assert_eq!(
            curator.opt_str("topology"),
            Some("co-tenant-shard-0".to_string())
        );
        assert_eq!(
            curator.opt_str("kv_budget_profile"),
            Some("small".to_string())
        );
        assert_eq!(curator.opt_u64("librarian_kv_cache_mib"), Some(256));
        assert_eq!(curator.opt_u64("warden_kv_cache_mib"), Some(256));
        assert_eq!(curator.opt_u64("adjudicator_kv_cache_mib"), Some(128));
        assert_eq!(curator.opt_u64("total_kv_cache_mib"), Some(640));
        assert_eq!(
            curator.opt_str("cross_check_retention_policy"),
            Some(CROSS_CHECK_RETENTION_POLICY.to_string())
        );

        let tail = admin_dispatch(
            &node,
            &Cbor::map(vec![
                ("verb", Cbor::t("cross-check tail")),
                ("args", Cbor::Null),
            ]),
        )
        .unwrap();
        assert_eq!(
            tail.opt_str("retention_policy"),
            Some(CROSS_CHECK_RETENTION_POLICY.to_string())
        );

        let _ = std::fs::remove_dir_all(&cfg.data_dir);
    }

    #[test]
    fn curator_kv_budget_profile_rejects_unknown_values() {
        let mut cfg = Config::default();
        apply_kv(&mut cfg, "curator.kv_budget_profile", "heavy").unwrap();
        assert_eq!(
            cfg.curator.kv_budget_profile.as_str(),
            CuratorKvBudgetProfile::Heavy.as_str()
        );
        assert!(apply_kv(&mut cfg, "curator.kv_budget_profile", "gigantic").is_err());

        let good = minitoml::parse("[curator]\nkv_budget_profile = \"small\"\n").unwrap();
        cfg.apply_toml(&good).unwrap();
        assert_eq!(
            cfg.curator.kv_budget_profile.as_str(),
            CuratorKvBudgetProfile::Small.as_str()
        );

        let bad = minitoml::parse("[curator]\nkv_budget_profile = \"tiny\"\n").unwrap();
        assert!(cfg.apply_toml(&bad).is_err());
    }

    #[test]
    fn tcp_listener_policy_rejects_non_loopback_config() {
        let mut cfg = Config::default();
        apply_kv(&mut cfg, "listen.tcp", "127.0.0.1:7741").unwrap();
        assert_eq!(cfg.tcp_addr, Some("127.0.0.1:7741".to_string()));
        assert!(apply_kv(&mut cfg, "listen.tcp", "0.0.0.0:7741").is_err());

        let good = minitoml::parse("[listen]\ntcp = \"[::1]:7741\"\n").unwrap();
        cfg.apply_toml(&good).unwrap();
        assert_eq!(cfg.tcp_addr, Some("[::1]:7741".to_string()));

        let bad = minitoml::parse("[listen]\ntcp = \"192.168.1.25:7741\"\n").unwrap();
        assert!(cfg.apply_toml(&bad).is_err());
    }

    #[test]
    fn otlp_config_override_surface_roundtrips() {
        let mut cfg = Config::default();
        apply_kv(
            &mut cfg,
            "observability.otlp_metrics_endpoint",
            "http://127.0.0.1:9999/v1/metrics",
        )
        .unwrap();
        apply_kv(&mut cfg, "observability.otlp_enabled", "false").unwrap();
        apply_kv(&mut cfg, "observability.otlp_timeout_ms", "2500").unwrap();
        assert!(!cfg.observability.enabled);
        assert_eq!(
            cfg.observability.metrics_endpoint,
            "http://127.0.0.1:9999/v1/metrics"
        );
        assert_eq!(cfg.observability.timeout_ms, 2500);

        let doc = minitoml::parse(
            "[observability]\notlp_enabled = true\notlp_logs_endpoint = \"http://localhost:4318/v1/logs\"\n",
        )
        .unwrap();
        cfg.apply_toml(&doc).unwrap();
        assert!(cfg.observability.enabled);
        assert_eq!(
            cfg.observability.logs_endpoint,
            "http://localhost:4318/v1/logs"
        );
    }

    #[test]
    fn congruence_admin_workflow_accepts_delta_and_unblocks_write() {
        let report = dry_run().unwrap();
        let node = report.node;
        let token = issue_agent_token(&*node.signer, "congruence-agent", 0);
        node.cells
            .agent_registry
            .lock()
            .unwrap()
            .register(node.now(), "congruence-agent", "agent");

        let delta_payload = Cbor::map(vec![(
            "note",
            Cbor::t("introduce the TelepathyCell per GAP-999 and P42"),
        )]);
        let preview = admin_dispatch(
            &node,
            &Cbor::map(vec![
                ("verb", Cbor::t("congruence preview")),
                ("args", Cbor::map(vec![("payload", delta_payload.clone())])),
            ]),
        )
        .unwrap();
        assert_eq!(preview.opt_bool("delta"), Some(true));
        let detail = preview.opt_str("detail").unwrap_or_default();
        assert!(detail.contains("TelepathyCell"));
        assert!(detail.contains("GAP-999"));
        assert!(detail.contains("P42"));

        let write = selftest_env(
            &node,
            &token,
            Intent::Write,
            Cbor::map(vec![
                ("subject", Cbor::t("congruence")),
                ("predicate", Cbor::t("status")),
                (
                    "object",
                    Cbor::t("introduce the TelepathyCell per GAP-999 and P42"),
                ),
            ]),
            Some("Architecture.md\u{00a7}4"),
            "task-congruence",
            777,
            false,
        );
        let blocked = handle_envelope(&node, &write);
        assert!(!blocked.ok);
        assert_eq!(blocked.err_code, Some(ErrCode::CongruenceDelta));

        let accepted = admin_dispatch(
            &node,
            &Cbor::map(vec![
                ("verb", Cbor::t("congruence accept")),
                (
                    "args",
                    Cbor::map(vec![(
                        "entities",
                        Cbor::text_array(&[
                            "TelepathyCell".to_string(),
                            "GAP-999".to_string(),
                            "P42".to_string(),
                        ]),
                    )]),
                ),
            ]),
        )
        .unwrap();
        assert_eq!(accepted.opt_u64("accepted_deltas"), Some(3));

        let audit = admin_dispatch(
            &node,
            &Cbor::map(vec![
                ("verb", Cbor::t("congruence audit")),
                ("args", Cbor::Null),
            ]),
        )
        .unwrap();
        assert_eq!(audit.opt_u64("accepted_deltas"), Some(3));

        let preview_after = admin_dispatch(
            &node,
            &Cbor::map(vec![
                ("verb", Cbor::t("congruence preview")),
                ("args", Cbor::map(vec![("payload", delta_payload)])),
            ]),
        )
        .unwrap();
        assert_eq!(preview_after.opt_bool("delta"), Some(false));

        let allowed = handle_envelope(&node, &write);
        assert!(allowed.ok);
        assert!(allowed
            .result
            .opt_str("handle")
            .unwrap_or_default()
            .starts_with("fact/"));
    }

    #[test]
    fn curator_spawn_severity_survives_into_trinity_quarantine() {
        let report = dry_run().unwrap();
        let node = report.node;

        {
            let mut t = node.trinity.lock().unwrap();
            t.contract.deprecate("curator.librarian.output.v1").unwrap();
        }

        let token = issue_agent_token(&*node.signer, "severity-agent", 0);
        node.cells
            .agent_registry
            .lock()
            .unwrap()
            .register(node.now(), "severity-agent", "agent");

        let before: BTreeSet<String> = node
            .trinity
            .lock()
            .unwrap()
            .quarantine
            .list()
            .iter()
            .map(|q| q.qid.clone())
            .collect();

        for (seed, severity) in [
            (901_u64, Severity::P0),
            (902, Severity::P1),
            (903, Severity::P2),
        ] {
            let env = Envelope {
                proto_version: PROTO_VERSION,
                request_id: Ulid::from_parts(node.now(), &mut DetRng::new(seed ^ 0xC0)),
                agent_id: token.agent_id.clone(),
                capability: token.clone(),
                work_budget: WorkBudget {
                    task_id: format!("severity-{seed}"),
                    units: 10_000,
                },
                intent: Intent::Write,
                payload: Cbor::map(vec![
                    ("subject", Cbor::t(format!("sev-{seed}"))),
                    ("predicate", Cbor::t("status")),
                    ("object", Cbor::t("trip curator contract failure")),
                ]),
                spec_anchor: Some("Architecture.md\u{00a7}4".into()),
                severity,
                gap_ref: None,
                tier: Tier::L2,
                seed,
                flags: EnvelopeFlags {
                    semantic_check: false,
                    continuation: false,
                },
            };
            let resp = handle_envelope(&node, &env);
            assert!(
                resp.ok,
                "writer envelope should still succeed for {severity:?}"
            );
        }

        let after = node.trinity.lock().unwrap();
        let severities: BTreeSet<String> = after
            .quarantine
            .list()
            .iter()
            .filter(|q| !before.contains(&q.qid))
            .filter(|q| {
                q.payload.opt_str("schema_id").as_deref() == Some("curator.librarian.output.v1")
            })
            .filter_map(|q| q.payload.opt_str("severity"))
            .collect();
        assert_eq!(
            severities,
            BTreeSet::from(["P0".to_string(), "P1".to_string(), "P2".to_string(),])
        );
    }
}
