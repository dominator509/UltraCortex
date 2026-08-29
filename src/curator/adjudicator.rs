//! AdjudicatorCell — SPEC-DERIVED-§3–§10 (AdjudicatorCell.md).
//!
//! Resolves Librarian↔Warden disputes in three tiers:
//!
//! 1. **Policy table** (~70–80% of cases): pure substrate-evidence rules —
//!    e.g. a hallucination flag is upheld iff the disputed handle truly
//!    doesn't exist. No model involved.
//! 2. **Rotating judge pool** (ambiguous cases): judge = `pool[seed % len]`
//!    with a per-judge tie-break salt, so the same dispute under the same
//!    envelope seed always lands on the same judge and yields the same
//!    verdict, while different disputes rotate across the pool
//!    (spec pool: Phi-3.5-mini / Llama-3.2-3B / SmolLM2-1.7B).
//! 3. **Human escalation** (~1–2%): pool verdict `Uncertain` queues the
//!    dispute for an operator (`adjudicator stats`, admin resolve).
//!
//! **Structural prior-blindness**: nothing in this module takes a
//! CrossCheckLedger reference — the adjudicator cannot consult historical
//! agreement rates even by accident (AdjudicatorCell.md §7: priors would
//! let curators launder collusion through the referee). Conformance test
//! C7 asserts identical disputes resolve identically regardless of ledger
//! history.

use super::{CuratorBackend, CuratorOperation, CuratorPublic, SubstrateView, Verdict};
use crate::cells::{CellBehavior, CellType};
use crate::core::cbor::Cbor;
use crate::core::ulid::{DetRng, Ulid};
use crate::core::{CellId, SchemaId, UcError, UcResult};
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// Librarian (the flagged output's producer) prevails; flag dismissed.
    InitiatorUpheld,
    /// Warden's flag prevails; output quarantined.
    AuditorUpheld,
    /// Queued for a human operator.
    HumanEscalation,
}

impl Resolution {
    pub fn as_str(self) -> &'static str {
        match self {
            Resolution::InitiatorUpheld => "initiator_upheld",
            Resolution::AuditorUpheld => "auditor_upheld",
            Resolution::HumanEscalation => "human_escalation",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolutionPath {
    Policy,
    Pool,
    Human,
}

impl ResolutionPath {
    pub fn as_str(self) -> &'static str {
        match self {
            ResolutionPath::Policy => "policy",
            ResolutionPath::Pool => "pool",
            ResolutionPath::Human => "human",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Adjudication {
    pub handle: String, // "adjudication/<ulid>"
    pub initiator_output: String,
    pub auditor_flag: String,
    pub kind: CuratorOperation,
    pub resolution: Resolution,
    pub path: ResolutionPath,
    pub judge: Option<String>, // pool member id, if path == Pool
    pub logical_at: u64,
    pub human_resolved: bool,
}

/// A dispute handed to the Adjudicator: the Warden flagged a Librarian
/// output (or a write), the Librarian's sanity check disagreed.
pub struct Dispute<'a> {
    pub initiator_output: &'a CuratorPublic, // the flagged Librarian output
    pub auditor_flag: &'a CuratorPublic,     // the Warden flag
    pub logical_at: u64,
    pub seed: u64,
}

pub struct AdjudicatorCell {
    pub id: CellId,
    pool: Vec<(String, Arc<dyn CuratorBackend>)>,
    records: BTreeMap<String, Adjudication>,
    escalation_queue: Vec<String>, // adjudication handles awaiting a human
    // Stats.
    policy_count: u64,
    pool_count: u64,
    human_count: u64,
}

impl AdjudicatorCell {
    pub fn new(id: CellId, pool: Vec<(String, Arc<dyn CuratorBackend>)>) -> Self {
        assert!(!pool.is_empty(), "adjudicator pool must be non-empty");
        AdjudicatorCell {
            id,
            pool,
            records: BTreeMap::new(),
            escalation_queue: Vec::new(),
            policy_count: 0,
            pool_count: 0,
            human_count: 0,
        }
    }

    pub fn pool_names(&self) -> Vec<String> {
        self.pool.iter().map(|(n, _)| n.clone()).collect()
    }

    /// Resolve a dispute. Note the signature: substrate view + the two
    /// public outputs + seed. No ledger, no history, no agreement rates.
    pub fn adjudicate(&mut self, view: &dyn SubstrateView, d: &Dispute<'_>) -> Adjudication {
        let mut rng = DetRng::new(d.seed ^ d.logical_at ^ 0xAD5);
        let handle = format!("adjudication/{}", Ulid::from_parts(d.logical_at, &mut rng));

        // Tier 1 — policy table.
        let policy = self.policy_table(view, d);
        let (resolution, path, judge) = match policy {
            Some(r) => {
                self.policy_count += 1;
                (r, ResolutionPath::Policy, None)
            }
            None => {
                // Tier 2 — rotating pool judge.
                let idx = (d.seed % self.pool.len() as u64) as usize;
                let (judge_name, backend) = &self.pool[idx];
                // Per-judge tie-break salt (AdjudicatorCell.md §5.3).
                let judge_salt = crate::core::fnv1a64(judge_name.as_bytes());
                let evidence = self.assemble_evidence(view, d);
                let summary = format!(
                    "{} vs {} over {}",
                    d.initiator_output.output_handle,
                    d.auditor_flag.output_handle,
                    d.auditor_flag.operation.as_str()
                );
                match backend.adjudicate(d.seed ^ judge_salt, &summary, &evidence) {
                    Verdict::InitiatorCorrect => {
                        self.pool_count += 1;
                        (
                            Resolution::InitiatorUpheld,
                            ResolutionPath::Pool,
                            Some(judge_name.clone()),
                        )
                    }
                    Verdict::AuditorCorrect => {
                        self.pool_count += 1;
                        (
                            Resolution::AuditorUpheld,
                            ResolutionPath::Pool,
                            Some(judge_name.clone()),
                        )
                    }
                    Verdict::Uncertain => {
                        // Tier 3 — human escalation.
                        self.human_count += 1;
                        (
                            Resolution::HumanEscalation,
                            ResolutionPath::Human,
                            Some(judge_name.clone()),
                        )
                    }
                }
            }
        };

        let rec = Adjudication {
            handle: handle.clone(),
            initiator_output: d.initiator_output.output_handle.clone(),
            auditor_flag: d.auditor_flag.output_handle.clone(),
            kind: d.auditor_flag.operation,
            resolution,
            path,
            judge,
            logical_at: d.logical_at,
            human_resolved: false,
        };
        if resolution == Resolution::HumanEscalation {
            self.escalation_queue.push(handle.clone());
        }
        self.records.insert(handle, rec.clone());
        rec
    }

    /// Evaluate a dispute without changing the live adjudicator state. This
    /// is used by the Router to persist the resulting record before making it
    /// visible, while retaining the same single backend evaluation.
    pub fn adjudicate_preview(&self, view: &dyn SubstrateView, d: &Dispute<'_>) -> Adjudication {
        let mut scratch = AdjudicatorCell::new(self.id, self.pool.clone());
        scratch.adjudicate(view, d)
    }

    /// Replay or commit a previously computed adjudication without invoking a
    /// model. The operation is idempotent for snapshot/WAL overlap.
    pub fn replay_adjudication(&mut self, rec: Adjudication) {
        if self.records.contains_key(&rec.handle) {
            return;
        }
        match rec.path {
            ResolutionPath::Policy => self.policy_count += 1,
            ResolutionPath::Pool => self.pool_count += 1,
            ResolutionPath::Human => self.human_count += 1,
        }
        if rec.resolution == Resolution::HumanEscalation
            && !rec.human_resolved
            && !self.escalation_queue.contains(&rec.handle)
        {
            self.escalation_queue.push(rec.handle.clone());
        }
        self.records.insert(rec.handle.clone(), rec);
    }

    /// The deterministic policy table (AdjudicatorCell.md §4). Returns
    /// `None` when the evidence is genuinely ambiguous and the pool must
    /// decide.
    fn policy_table(&self, view: &dyn SubstrateView, d: &Dispute<'_>) -> Option<Resolution> {
        match d.auditor_flag.operation {
            CuratorOperation::FlagHallucination => {
                // The flag's grounded_in carries the disputed handles. If
                // every disputed handle exists, the flag is simply wrong; if
                // any is truly absent, the flag is simply right. (Empty
                // grounding is malformed → ambiguous → pool.)
                if d.auditor_flag.grounded_in.is_empty() {
                    return None;
                }
                let any_missing = d
                    .auditor_flag
                    .grounded_in
                    .iter()
                    .any(|h| !view.handle_exists(h));
                Some(if any_missing {
                    Resolution::AuditorUpheld
                } else {
                    Resolution::InitiatorUpheld
                })
            }
            CuratorOperation::FlagDrift => {
                // Drift is real iff ≥2 of the flagged handles are live
                // (the conflicting pair both active). If fewer than 2 are
                // live the conflict evaporated (supersession already
                // happened) → initiator upheld. If exactly the boundary
                // (grounding smaller than 2) → malformed → pool.
                if d.auditor_flag.grounded_in.len() < 2 {
                    return None;
                }
                let live = d
                    .auditor_flag
                    .grounded_in
                    .iter()
                    .filter(|h| view.handle_exists(h))
                    .count();
                Some(if live >= 2 {
                    Resolution::AuditorUpheld
                } else {
                    Resolution::InitiatorUpheld
                })
            }
            // Audit-fail vs sanity-disagree on quality judgments (empty
            // skeletons etc.) has no crisp substrate rule → pool.
            _ => None,
        }
    }

    fn assemble_evidence(&self, view: &dyn SubstrateView, d: &Dispute<'_>) -> Vec<String> {
        let mut ev = Vec::new();
        for h in &d.initiator_output.grounded_in {
            if view.handle_exists(h) {
                ev.push(format!("supports_initiator: cited {h} exists"));
            } else {
                ev.push(format!("supports_auditor: cited {h} absent"));
            }
        }
        for h in &d.auditor_flag.grounded_in {
            if view.handle_exists(h) {
                ev.push(format!("supports_auditor: flagged evidence {h} live"));
            } else {
                ev.push(format!("supports_initiator: flagged evidence {h} gone"));
            }
        }
        ev.sort();
        ev
    }

    /// Operator resolution of an escalated dispute.
    pub fn validate_human_resolution(&self, handle: &str) -> UcResult<&Adjudication> {
        let rec = self
            .records
            .get(handle)
            .ok_or_else(|| UcError::not_found(format!("adjudication {handle}")))?;
        if rec.resolution != Resolution::HumanEscalation || rec.human_resolved {
            return Err(UcError::schema(format!(
                "adjudication {handle} is not awaiting human resolution"
            )));
        }
        Ok(rec)
    }

    pub fn resolve_human(&mut self, handle: &str, uphold_auditor: bool) -> UcResult<&Adjudication> {
        self.validate_human_resolution(handle)?;
        let rec = self
            .records
            .get_mut(handle)
            .expect("validated record exists");
        rec.resolution = if uphold_auditor {
            Resolution::AuditorUpheld
        } else {
            Resolution::InitiatorUpheld
        };
        rec.human_resolved = true;
        self.escalation_queue.retain(|h| h != handle);
        Ok(self.records.get(handle).unwrap())
    }

    pub fn stats(&self) -> (u64, u64, u64, usize) {
        (
            self.policy_count,
            self.pool_count,
            self.human_count,
            self.escalation_queue.len(),
        )
    }

    pub fn get(&self, handle: &str) -> Option<&Adjudication> {
        self.records.get(handle)
    }

    pub fn escalations(&self) -> &[String] {
        &self.escalation_queue
    }
}

impl CellBehavior for AdjudicatorCell {
    fn cell_id(&self) -> CellId {
        self.id
    }
    fn cell_type(&self) -> CellType {
        CellType::Adjudicator
    }
    fn schema_id(&self) -> SchemaId {
        SchemaId::new("curator.adjudicator.v1")
    }

    fn on_query(&self, _at: u64, query: &Cbor) -> UcResult<Cbor> {
        match query.opt_str("op").as_deref() {
            Some("stats") | None => {
                let (policy, pool, human, queued) = self.stats();
                let total = (policy + pool + human).max(1);
                Ok(Cbor::map(vec![
                    ("policy", Cbor::U64(policy)),
                    ("pool", Cbor::U64(pool)),
                    ("human", Cbor::U64(human)),
                    ("queued", Cbor::U64(queued as u64)),
                    ("policy_share_pct", Cbor::U64(policy * 100 / total)),
                    ("pool_members", Cbor::text_array(&self.pool_names())),
                ]))
            }
            Some("get") => {
                let h = query.req_str("handle")?;
                self.get(&h)
                    .map(adjudication_to_cbor)
                    .ok_or_else(|| UcError::not_found(format!("adjudication {h}")))
            }
            _ => Err(UcError::schema("adjudicator: unknown op")),
        }
    }

    fn on_update(&mut self, _at: u64, update: &Cbor) -> UcResult<Cbor> {
        // Only human resolution flows through direct update.
        let h = update.req_str("handle")?;
        let uphold = update.opt_bool("uphold_auditor").unwrap_or(false);
        let rec = self.resolve_human(&h, uphold)?;
        Ok(adjudication_to_cbor(rec))
    }

    fn snapshot_state(&self) -> Cbor {
        let items: Vec<Cbor> = self.records.values().map(adjudication_to_cbor).collect();
        Cbor::map(vec![
            ("records", Cbor::Array(items)),
            ("escalation_queue", Cbor::text_array(&self.escalation_queue)),
            ("policy_count", Cbor::U64(self.policy_count)),
            ("pool_count", Cbor::U64(self.pool_count)),
            ("human_count", Cbor::U64(self.human_count)),
        ])
    }

    fn restore_state(&mut self, state: &Cbor) -> UcResult<()> {
        self.records.clear();
        self.escalation_queue.clear();
        self.policy_count = state.opt_u64("policy_count").unwrap_or(0);
        self.pool_count = state.opt_u64("pool_count").unwrap_or(0);
        self.human_count = state.opt_u64("human_count").unwrap_or(0);
        if let Some(arr) = state.get("records").and_then(|v| v.as_array()) {
            for item in arr {
                let resolution = match item.opt_str("resolution").as_deref() {
                    Some("initiator_upheld") => Resolution::InitiatorUpheld,
                    Some("auditor_upheld") => Resolution::AuditorUpheld,
                    _ => Resolution::HumanEscalation,
                };
                let path = match item.opt_str("path").as_deref() {
                    Some("policy") => ResolutionPath::Policy,
                    Some("pool") => ResolutionPath::Pool,
                    _ => ResolutionPath::Human,
                };
                let rec = Adjudication {
                    handle: item.req_str("handle")?,
                    initiator_output: item.opt_str("initiator_output").unwrap_or_default(),
                    auditor_flag: item.opt_str("auditor_flag").unwrap_or_default(),
                    kind: item
                        .opt_str("kind")
                        .and_then(|s| CuratorOperation::parse(&s))
                        .unwrap_or(CuratorOperation::FlagDrift),
                    resolution,
                    path,
                    judge: item.opt_str("judge"),
                    logical_at: item.opt_u64("logical_at").unwrap_or(0),
                    human_resolved: item.opt_bool("human_resolved").unwrap_or(false),
                };
                self.records.insert(rec.handle.clone(), rec);
            }
        }
        if let Some(arr) = state.get("escalation_queue").and_then(|v| v.as_array()) {
            for item in arr {
                if let Some(s) = item.as_str() {
                    self.escalation_queue.push(s.to_string());
                }
            }
        }
        Ok(())
    }
}

pub(crate) fn adjudication_from_cbor(item: &Cbor) -> UcResult<Adjudication> {
    let resolution = match item.opt_str("resolution").as_deref() {
        Some("initiator_upheld") => Resolution::InitiatorUpheld,
        Some("auditor_upheld") => Resolution::AuditorUpheld,
        _ => Resolution::HumanEscalation,
    };
    let path = match item.opt_str("path").as_deref() {
        Some("policy") => ResolutionPath::Policy,
        Some("pool") => ResolutionPath::Pool,
        _ => ResolutionPath::Human,
    };
    Ok(Adjudication {
        handle: item.req_str("handle")?,
        initiator_output: item.opt_str("initiator_output").unwrap_or_default(),
        auditor_flag: item.opt_str("auditor_flag").unwrap_or_default(),
        kind: item
            .opt_str("kind")
            .and_then(|s| CuratorOperation::parse(&s))
            .unwrap_or(CuratorOperation::FlagDrift),
        resolution,
        path,
        judge: item.opt_str("judge"),
        logical_at: item.opt_u64("logical_at").unwrap_or(0),
        human_resolved: item.opt_bool("human_resolved").unwrap_or(false),
    })
}

pub(crate) fn adjudication_to_cbor(a: &Adjudication) -> Cbor {
    Cbor::map(vec![
        ("handle", Cbor::t(a.handle.clone())),
        ("initiator_output", Cbor::t(a.initiator_output.clone())),
        ("auditor_flag", Cbor::t(a.auditor_flag.clone())),
        ("kind", Cbor::t(a.kind.as_str())),
        ("resolution", Cbor::t(a.resolution.as_str())),
        ("path", Cbor::t(a.path.as_str())),
        (
            "judge",
            a.judge
                .as_ref()
                .map(|j| Cbor::t(j.clone()))
                .unwrap_or(Cbor::Null),
        ),
        ("logical_at", Cbor::U64(a.logical_at)),
        ("human_resolved", Cbor::Bool(a.human_resolved)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curator::librarian::test_support::FakeView;
    use crate::curator::{ConfidenceBand, DeterministicBackend};

    fn pool3() -> Vec<(String, Arc<dyn CuratorBackend>)> {
        vec![
            (
                "phi-3.5-mini".to_string(),
                Arc::new(DeterministicBackend) as Arc<dyn CuratorBackend>,
            ),
            ("llama-3.2-3b".to_string(), Arc::new(DeterministicBackend)),
            ("smollm2-1.7b".to_string(), Arc::new(DeterministicBackend)),
        ]
    }

    fn output(handle: &str, grounded: Vec<&str>) -> CuratorPublic {
        CuratorPublic {
            output_handle: handle.into(),
            operation: CuratorOperation::Skeleton,
            target_handle: "fact/T".into(),
            grounded_in: grounded.into_iter().map(str::to_string).collect(),
            confidence_band: ConfidenceBand::Medium,
            schema_id: "curator.librarian.output.v1".into(),
            spec_anchor: "LibrarianCell.md\u{00a7}3".into(),
            logical_at: 5,
            body: "body".into(),
        }
    }

    fn flag(handle: &str, op: CuratorOperation, grounded: Vec<&str>) -> CuratorPublic {
        CuratorPublic {
            output_handle: handle.into(),
            operation: op,
            target_handle: "librarian/output/01I".into(),
            grounded_in: grounded.into_iter().map(str::to_string).collect(),
            confidence_band: ConfidenceBand::High,
            schema_id: "curator.warden.judgment.v1".into(),
            spec_anchor: "WardenCell.md\u{00a7}4".into(),
            logical_at: 6,
            body: "flag".into(),
        }
    }

    #[test]
    fn policy_resolves_hallucination_disputes() {
        let mut adj = AdjudicatorCell::new(CellId(32), pool3());
        let view = FakeView::default().with_handle("fact/REAL");
        let init = output("librarian/output/01I", vec!["fact/REAL"]);

        // Warden flagged a truly-absent handle → auditor upheld by policy.
        let f1 = flag(
            "warden/judgment/01F",
            CuratorOperation::FlagHallucination,
            vec!["fact/GHOST"],
        );
        let r1 = adj.adjudicate(
            &view,
            &Dispute {
                initiator_output: &init,
                auditor_flag: &f1,
                logical_at: 10,
                seed: 42,
            },
        );
        assert_eq!(r1.resolution, Resolution::AuditorUpheld);
        assert_eq!(r1.path, ResolutionPath::Policy);

        // Warden flagged an existing handle → initiator upheld by policy.
        let f2 = flag(
            "warden/judgment/02F",
            CuratorOperation::FlagHallucination,
            vec!["fact/REAL"],
        );
        let r2 = adj.adjudicate(
            &view,
            &Dispute {
                initiator_output: &init,
                auditor_flag: &f2,
                logical_at: 11,
                seed: 42,
            },
        );
        assert_eq!(r2.resolution, Resolution::InitiatorUpheld);
        let (policy, _, _, _) = adj.stats();
        assert_eq!(policy, 2);
    }

    #[test]
    fn pool_rotation_is_seed_deterministic() {
        let mut adj = AdjudicatorCell::new(CellId(32), pool3());
        let view = FakeView::default();
        let init = output("librarian/output/01I", vec![]);
        // AuditFail kind → no policy rule → pool.
        let f = flag("warden/judgment/03F", CuratorOperation::AuditFail, vec![]);
        let mk = |seed| Dispute {
            initiator_output: &init,
            auditor_flag: &f,
            logical_at: 20,
            seed,
        };
        let a = adj.adjudicate(&view, &mk(0));
        let b = adj.adjudicate(&view, &mk(1));
        let c = adj.adjudicate(&view, &mk(2));
        assert_eq!(a.judge.as_deref(), Some("phi-3.5-mini"));
        assert_eq!(b.judge.as_deref(), Some("llama-3.2-3b"));
        assert_eq!(c.judge.as_deref(), Some("smollm2-1.7b"));
        // Same dispute + same seed on a fresh adjudicator → same everything.
        let mut adj2 = AdjudicatorCell::new(CellId(32), pool3());
        let a2 = adj2.adjudicate(&view, &mk(0));
        assert_eq!(a.resolution, a2.resolution);
        assert_eq!(a.judge, a2.judge);
    }

    #[test]
    fn human_escalation_flow() {
        let mut adj = AdjudicatorCell::new(CellId(32), pool3());
        let view = FakeView::default();
        let init = output("librarian/output/01I", vec![]);
        let f = flag("warden/judgment/04F", CuratorOperation::AuditFail, vec![]);
        // Hunt a seed whose dead-tie hash escalates (h & 0b111 == 0 in the
        // deterministic backend) — bounded search keeps the test stable.
        let mut escalated = None;
        for seed in 0..512u64 {
            let r = adj.adjudicate(
                &view,
                &Dispute {
                    initiator_output: &init,
                    auditor_flag: &f,
                    logical_at: 30,
                    seed,
                },
            );
            if r.resolution == Resolution::HumanEscalation {
                escalated = Some(r);
                break;
            }
        }
        let rec = escalated.expect("some seed must escalate");
        assert_eq!(adj.escalations().len(), 1);
        let resolved = adj.resolve_human(&rec.handle, true).unwrap();
        assert_eq!(resolved.resolution, Resolution::AuditorUpheld);
        assert!(resolved.human_resolved);
        assert!(adj.escalations().is_empty());
        // Double-resolve rejected.
        assert!(adj.resolve_human(&rec.handle, false).is_err());
    }
}
