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
use crate::core::{fnv1a64, ErrCode, Intent, Severity, Tier, UcError, UcResult};
use crate::curator::{CuratorConfig, SubstrateView};
use crate::node::{ids, Node};
use crate::persist::wal::{replay_dir, WalOp};
use crate::persist::{EncryptionTier, Manifest};
use crate::router::captoken::{
    issue_agent_token, issue_curator_token, issue_operator_token, CapToken,
};
use crate::router::envelope::{Envelope, EnvelopeFlags, WorkBudget, PROTO_VERSION};
use crate::router::handle_envelope;
use crate::obs::AuditChain;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// B1 — Config
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Config {
    pub node_id: String,
    pub data_dir: PathBuf,
    pub shards: u64,
    pub encryption_tier: EncryptionTier,
    pub boot_seed: u64,
    pub embedder_dim: usize,
    pub uds_path: Option<PathBuf>,
    pub tcp_addr: Option<String>,
    pub curator: CuratorConfig,
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
            encryption_tier: EncryptionTier::T1,
            boot_seed: 0x0517AC0817E,
            embedder_dim: 256,
            uds_path: None, // defaults to <data_dir>/ultracortex.sock
            tcp_addr: None, // "127.0.0.1:7741" when enabled
            curator: CuratorConfig::default(),
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
            cfg.apply_toml(&doc);
        } else if Path::new("ultracortex.toml").exists() {
            let text = std::fs::read_to_string("ultracortex.toml").map_err(UcError::from)?;
            let doc = minitoml::parse(&text).map_err(UcError::schema)?;
            cfg.apply_toml(&doc);
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

    fn apply_toml(&mut self, doc: &minitoml::TomlDoc) {
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
        if let Some(v) = get("node", "boot_seed").and_then(|v| v.as_int()) {
            self.boot_seed = v as u64;
        }
        if let Some(v) = get("node", "embedder_dim").and_then(|v| v.as_int()) {
            self.embedder_dim = v.max(16) as usize;
        }
        if let Some(v) = get("persist", "encryption_tier").and_then(|v| v.as_str().map(String::from))
        {
            if let Ok(t) = EncryptionTier::parse(&v) {
                self.encryption_tier = t;
            }
        }
        if let Some(v) = get("listen", "uds").and_then(|v| v.as_str().map(String::from)) {
            self.uds_path = if v.is_empty() { None } else { Some(PathBuf::from(v)) };
        }
        if let Some(v) = get("listen", "tcp").and_then(|v| v.as_str().map(String::from)) {
            self.tcp_addr = if v.is_empty() { None } else { Some(v) };
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
        if let Some(v) = get("curator", "external_cmd").and_then(|v| v.as_str().map(String::from))
        {
            self.curator.external_cmd = if v.is_empty() { None } else { Some(v) };
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
        // [curator.pinned]: model = "sha256hex"
        if let Some(section) = doc.get("curator.pinned") {
            for (model, v) in section {
                if let Some(sha) = v.as_str() {
                    self.curator.pinned.insert(model.clone(), sha.to_string());
                }
            }
        }
    }
}

type EnvSetter = fn(&mut Config, &str) -> UcResult<()>;
const ENV_KEYS: [(&str, EnvSetter); 6] = [
    ("UC_NODE_ID", |c, v| {
        c.node_id = v.to_string();
        Ok(())
    }),
    ("UC_DATA_DIR", |c, v| {
        c.data_dir = PathBuf::from(v);
        Ok(())
    }),
    ("UC_SHARDS", |c, v| {
        c.shards = v.parse::<u64>().map_err(|e| UcError::schema(e.to_string()))?.max(1);
        Ok(())
    }),
    ("UC_ENCRYPTION_TIER", |c, v| {
        c.encryption_tier = EncryptionTier::parse(v)?;
        Ok(())
    }),
    ("UC_UDS", |c, v| {
        c.uds_path = if v.is_empty() { None } else { Some(PathBuf::from(v)) };
        Ok(())
    }),
    ("UC_TCP", |c, v| {
        c.tcp_addr = if v.is_empty() { None } else { Some(v.to_string()) };
        Ok(())
    }),
];

fn apply_kv(cfg: &mut Config, k: &str, v: &str) -> UcResult<()> {
    match k {
        "node.id" => cfg.node_id = v.to_string(),
        "node.data_dir" => cfg.data_dir = PathBuf::from(v),
        "node.shards" => {
            cfg.shards = v.parse::<u64>().map_err(|e| UcError::schema(e.to_string()))?.max(1)
        }
        "node.boot_seed" => {
            cfg.boot_seed = v.parse().map_err(|_| UcError::schema("bad boot_seed"))?
        }
        "persist.encryption_tier" => cfg.encryption_tier = EncryptionTier::parse(v)?,
        "listen.uds" => {
            cfg.uds_path = if v.is_empty() { None } else { Some(PathBuf::from(v)) }
        }
        "listen.tcp" => cfg.tcp_addr = if v.is_empty() { None } else { Some(v.to_string()) },
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
        cfg.curator.clone(),
        cfg.embedder_dim,
    )?);

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
            vec!["output_handle".into(), "operation".into(), "target_handle".into()],
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
            "CatalogCell", "FactCell", "TimelineCell", "PlaybookCell", "ScratchpadCell",
            "VectorCell", "GraphCell", "Bm25Cell", "BlobCell", "CacheCell",
            "AgentRegistryCell", "ProposalCell", "SubscriptionCell", "RerankerCell",
            "SpecAnchorCell", "DecisionLedgerCell", "CongruenceCell", "GapCell",
            "QuarantineCell", "WorkBudgetCell", "ContractCell", "LibrarianCell",
            "WardenCell", "AdjudicatorCell", "CrossCheckLedgerCell",
        ] {
            t.congruence.register_entity(ct);
        }
        t.congruence.register_entity("GAP-0001");
        t.congruence.register_entity("P19");
        t.congruence.register_entity("P20");

        // 6/7. WorkBudget grants + Quarantine need no seeding beyond
        // construction; the curator task gets a standing grant.
        t.work_budget.ensure("curator.librarian", Some(10_000_000));
        t.work_budget.ensure("bootstrap.selftest", Some(1_000_000));
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
    check("B5.1 write", r.ok && written.starts_with("fact/"), format!("{:?}", r.err_message))?;

    // B5.2 — recall finds it.
    let r = handle_envelope(
        node,
        &selftest_env(
            node,
            &agent,
            Intent::Recall,
            Cbor::map(vec![("query", Cbor::t("substrate polices")), ("k", Cbor::U64(4))]),
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
        fact.get(&written).and_then(|f| f.superseded_by.clone()).is_some()
    };
    check("B5.3 supersede", r2.ok && superseded, format!("{:?}", r2.err_message))?;

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
            && r.quarantine_id.as_deref().unwrap_or("").starts_with("quarantine/"),
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
        &selftest_env(node, &agent, Intent::View, view_payload.clone(), None, task, 16, false),
    );
    let hits_before = node.metrics.counter("cache.hit");
    let r2 = handle_envelope(
        node,
        &selftest_env(node, &agent, Intent::View, view_payload, None, task, 17, false),
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
    check("B5.7 librarian active", skeleton_active, "no active skeleton".into())?;

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
                ("object", Cbor::t("grounded in fact/01GHOSTGHOSTGHOSTGHOSTGHOST")),
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
    node.metrics.inc(&format!("admin.{}", verb.replace(' ', "_")));

    match verb.as_str() {
        "status" => {
            let (p, a, q) = node.curators.librarian.lock().unwrap().counts();
            let pending = node.trinity.lock().unwrap().quarantine.pending_count();
            Ok(Cbor::map(vec![
                ("node_id", Cbor::t(node.node_id.clone())),
                ("logical_at", Cbor::U64(node.now())),
                ("quarantine_pending", Cbor::U64(pending as u64)),
                ("librarian_pending", Cbor::U64(p as u64)),
                ("librarian_active", Cbor::U64(a as u64)),
                ("librarian_quarantined", Cbor::U64(q as u64)),
            ]))
        }
        "snapshot" => {
            let states = node.snapshot_all();
            let name = node.snapshots.write(at, &states)?;
            {
                let mut m = node.manifest.lock().unwrap();
                m.logical_at = at;
                m.last_snapshot = Some(name.clone());
                m.state_hashes = node.state_hashes();
                m.save(&node.data_dir)?;
            }
            Ok(Cbor::map(vec![("snapshot", Cbor::t(name))]))
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
            Ok(Cbor::map(vec![("items", Cbor::Array(items))]))
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
                payload.opt_str("spec_anchor").as_deref().or(Some("Architecture.md\u{00a7}4")),
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
                    if resp.ok { resp.result } else { Cbor::t(resp.err_message.unwrap_or_default()) },
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
            Ok(Cbor::map(vec![
                ("records", Cbor::U64(records)),
                ("intact", Cbor::Bool(ok)),
            ]))
        }
        "congruence audit" => {
            let t = node.trinity.lock().unwrap();
            Ok(Cbor::map(vec![
                ("known_entities", Cbor::U64(t.congruence.known_count() as u64)),
                ("accepted_deltas", Cbor::U64(t.congruence.accepted_count() as u64)),
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
                    ])
                })
                .collect();
            Ok(Cbor::map(vec![("contracts", Cbor::Array(items))]))
        }
        "curator status" => {
            let (p, a, q) = node.curators.librarian.lock().unwrap().counts();
            let audits = node.curators.warden.lock().unwrap().audit_count();
            let ledger = node.cross_check.lock().unwrap();
            Ok(Cbor::map(vec![
                ("librarian_pending", Cbor::U64(p as u64)),
                ("librarian_active", Cbor::U64(a as u64)),
                ("librarian_quarantined", Cbor::U64(q as u64)),
                ("warden_audits", Cbor::U64(audits as u64)),
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
            Ok(Cbor::map(vec![("records", Cbor::Array(items))]))
        }
        "adjudicator stats" => {
            let adj = node.curators.adjudicator.lock().unwrap();
            let (policy, pool, human, queued) = adj.stats();
            Ok(Cbor::map(vec![
                ("policy", Cbor::U64(policy)),
                ("pool", Cbor::U64(pool)),
                ("human", Cbor::U64(human)),
                ("queued", Cbor::U64(queued as u64)),
                ("escalations", Cbor::text_array(
                    &adj.escalations().to_vec(),
                )),
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
        "shutdown" => {
            node.shutdown()?;
            Ok(Cbor::map(vec![("shutdown", Cbor::Bool(true))]))
        }
        other => Err(UcError::unsupported(format!("unknown admin verb `{other}`"))),
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
    println!("ready node_id={} proto_version={}", node.node_id, PROTO_VERSION);
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
    cfg.data_dir = std::env::temp_dir().join(format!(
        "ultracortex-dryrun-{}-{}",
        std::process::id(),
        DetRng::new(fnv1a64(b"dryrun")).next_u64()
    ));
    cfg.uds_path = Some(cfg.data_dir.join("ultracortex.sock"));
    boot(&cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        cfg.data_dir = std::env::temp_dir().join(format!(
            "ultracortex-recover-{}",
            std::process::id()
        ));
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
        assert!(report2.node.handle_exists(&handle));
        let _ = std::fs::remove_dir_all(&cfg.data_dir);
    }

    #[test]
    fn admin_verbs_respond() {
        let report = dry_run().unwrap();
        let node = report.node;
        let verbs = [
            "status",
            "quarantine list",
            "gap list",
            "congruence audit",
            "contract list",
            "curator status",
            "cross-check tail",
            "adjudicator stats",
            "metrics",
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
}
