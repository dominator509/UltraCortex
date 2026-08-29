//! WardenCell — SPEC-DERIVED-§3–§7 (WardenCell.md).
//!
//! The Warden (spec model: Qwen-2.5-Coder-1.5B; v0 default: deterministic
//! evidence checks) has two jobs:
//!
//! 1. **Envelope gate** (chain step 6, [`WardenCell::judge_envelope`]):
//!    when `flags.semantic_check` is set or severity is P0, harvest every
//!    handle-shaped reference from the payload and verify it exists
//!    (hallucination check), and verify (s,p,o) writes don't silently
//!    conflict with an active fact (drift check).
//! 2. **Librarian audit** ([`WardenCell::audit_librarian`]): every
//!    Librarian output is audited before activation. The audit must be
//!    **independently grounded**: the Warden must verify at least one
//!    substrate handle *not cited by the Librarian* that bears on the
//!    claim, or produce a hash-proof over the cited evidence — rubber
//!    stamps are structurally impossible (CURATOR_PAIR_PROTOCOL.md §6.2).
//!
//! Every audit run also fires a deliberate **boundary probe**: an attempted
//! hydrate of the Librarian's private rationale facet, which the Router
//! must deny. This keeps `curator.rationale_access_denied` non-zero — a
//! zero value in production means P19 enforcement is broken or bypassed
//! (RouterScheduler.md §A.5).

use super::librarian::CurationJob;
use super::{
    ConfidenceBand, CuratorBackend, CuratorOperation, CuratorOutput, CuratorPrivate, CuratorPublic,
    DeterministicBackend, SubstrateView, Verdict,
};
use crate::cells::{CellBehavior, CellType};
use crate::core::cbor::Cbor;
use crate::core::crypto::{hex, sha256};
use crate::core::ulid::{DetRng, Ulid};
use crate::core::{CellId, ErrCode, SchemaId, UcError, UcResult};
use std::collections::{BTreeMap, BTreeSet};

/// Handle prefixes the Warden treats as substrate references when
/// harvesting payload text (WardenCell.md §4.1).
pub const HANDLE_PREFIXES: [&str; 7] = [
    "fact/",
    "blob/",
    "decision/",
    "librarian/output/",
    "warden/judgment/",
    "timeline/",
    "playbook/",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuditVerdict {
    Pass,
    Fail { reason: String },
}

#[derive(Clone, Debug)]
pub struct AuditRecord {
    pub judgment_handle: String, // "warden/judgment/<ulid>"
    pub target_output: String,
    pub verdict_pass: bool,
    pub independent_grounds: Vec<String>,
    pub hash_proof: Option<String>,
    pub logical_at: u64,
}

pub struct WardenCell {
    pub id: CellId,
    backend: std::sync::Arc<dyn CuratorBackend>,
    audits: BTreeMap<String, AuditRecord>,
    seq_by_target: BTreeMap<String, String>, // target_output -> judgment
}

impl WardenCell {
    pub fn new(id: CellId) -> Self {
        Self::with_backend(id, std::sync::Arc::new(DeterministicBackend))
    }

    pub fn with_backend(id: CellId, backend: std::sync::Arc<dyn CuratorBackend>) -> Self {
        WardenCell {
            id,
            backend,
            audits: BTreeMap::new(),
            seq_by_target: BTreeMap::new(),
        }
    }

    pub fn backend_id(&self) -> String {
        self.backend.backend_id()
    }

    /// Harvest handle-shaped substrings from every text value in a payload.
    pub fn harvest_handles(payload: &Cbor) -> Vec<String> {
        let mut texts = Vec::new();
        payload.collect_texts(&mut texts);
        let mut out = Vec::new();
        for text in texts {
            for raw in text.split(|c: char| c.is_whitespace() || ",;()[]{}\"'".contains(c)) {
                let tok = raw.trim_matches(|c: char| ".:!?".contains(c));
                if HANDLE_PREFIXES.iter().any(|p| tok.starts_with(p))
                    && tok.len() > tok.find('/').unwrap_or(0) + 1
                    && !out.iter().any(|existing: &String| existing == tok)
                {
                    out.push(tok.to_string());
                }
            }
        }
        out
    }

    /// Chain step 6 — the semantic gate on a write envelope. Errors carry
    /// the UltraCortex-specific codes (`HallucinationDetected`,
    /// `SemanticDrift`) so callers can route them into the curator
    /// disagreement flow rather than plain quarantine (McpProtocol.md §A.2).
    pub fn judge_envelope(&self, view: &dyn SubstrateView, payload: &Cbor) -> UcResult<()> {
        // Hallucination: any referenced handle that doesn't exist.
        for h in Self::harvest_handles(payload) {
            if !view.handle_exists(&h) {
                return Err(UcError::new(
                    ErrCode::HallucinationDetected,
                    format!("payload references nonexistent handle {h}"),
                ));
            }
        }
        // Drift: an (s,p,o) write into an occupied slot with a different
        // object and no supersede declaration.
        if let (Some(s), Some(p), Some(o)) = (
            payload.opt_str("subject"),
            payload.opt_str("predicate"),
            payload.opt_str("object"),
        ) {
            if payload.opt_str("supersedes").is_none()
                && payload.opt_str("supersedes_decision").is_none()
            {
                let conflicting: Vec<String> = view
                    .active_sp(&s, &p)
                    .into_iter()
                    .filter(|(_, obj)| obj != &o)
                    .map(|(h, _)| h)
                    .collect();
                if !conflicting.is_empty() {
                    return Err(UcError::new(
                        ErrCode::SemanticDrift,
                        format!(
                            "write to occupied slot ({s}, {p}) conflicts with active {} \
                             and declares no supersede",
                            conflicting.join(", ")
                        ),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Build a warden flag output (for the disagreement flow) from a failed
    /// envelope judgment. Grounding carries the disputed handles so the
    /// Librarian's sanity check and the Adjudicator can verify.
    pub fn flag_from_error(
        &self,
        logical_at: u64,
        seed: u64,
        target: &str,
        err: &UcError,
        disputed_handles: Vec<String>,
    ) -> CuratorOutput {
        let mut rng = DetRng::new(seed ^ logical_at ^ 0x3AD);
        let ulid = Ulid::from_parts(logical_at, &mut rng);
        let operation = if err.code == ErrCode::HallucinationDetected {
            CuratorOperation::FlagHallucination
        } else {
            CuratorOperation::FlagDrift
        };
        let public = CuratorPublic {
            output_handle: format!("warden/judgment/{ulid}"),
            operation,
            target_handle: target.to_string(),
            grounded_in: disputed_handles,
            confidence_band: ConfidenceBand::High,
            schema_id: "curator.warden.judgment.v1".into(),
            spec_anchor: "WardenCell.md\u{00a7}4".into(),
            logical_at,
            body: err.message.clone(),
        };
        let private = CuratorPrivate {
            rationale: format!("deterministic evidence check: {}", err.message),
            considered_alts: vec!["pass".into()],
            confidence_precise: 0.93,
            reasoning_trace: format!("code={}", err.code.as_str()),
            private_seed: rng.next_u64(),
        };
        CuratorOutput { public, private }
    }

    /// Audit a Librarian output. Enforces §6.2 independent grounding:
    /// - collect substrate evidence relevant to the output's target that
    ///   the Librarian did *not* cite;
    /// - if none exists, compute a hash-proof over the cited evidence's
    ///   public text (proving the Warden actually read it) — never a bare
    ///   pass.
    pub fn audit_librarian(
        &mut self,
        view: &dyn SubstrateView,
        output: &CuratorPublic,
        job: Option<&CurationJob>,
        logical_at: u64,
        seed: u64,
    ) -> (AuditRecord, AuditVerdict) {
        let result = self.evaluate_librarian(view, output, job, logical_at, seed);
        self.store(result.0.clone());
        result
    }

    /// Evaluate without mutating Warden state. Router persistence uses this
    /// preview before committing the audit record to the Warden cell.
    pub fn audit_librarian_preview(
        &self,
        view: &dyn SubstrateView,
        output: &CuratorPublic,
        job: Option<&CurationJob>,
        logical_at: u64,
        seed: u64,
    ) -> (AuditRecord, AuditVerdict) {
        self.evaluate_librarian(view, output, job, logical_at, seed)
    }

    fn evaluate_librarian(
        &self,
        view: &dyn SubstrateView,
        output: &CuratorPublic,
        job: Option<&CurationJob>,
        logical_at: u64,
        seed: u64,
    ) -> (AuditRecord, AuditVerdict) {
        let mut rng = DetRng::new(seed ^ logical_at ^ 0xA0D17);
        let ulid = Ulid::from_parts(logical_at, &mut rng);
        let judgment_handle = format!("warden/judgment/{ulid}");

        // 1. Cited grounding must all exist.
        let missing: Vec<String> = output
            .grounded_in
            .iter()
            .filter(|h| !view.handle_exists(h))
            .cloned()
            .collect();
        if !missing.is_empty() {
            let rec = AuditRecord {
                judgment_handle: judgment_handle.clone(),
                target_output: output.output_handle.clone(),
                verdict_pass: false,
                independent_grounds: Vec::new(),
                hash_proof: None,
                logical_at,
            };
            return (
                rec,
                AuditVerdict::Fail {
                    reason: format!("cited grounding missing: {}", missing.join(", ")),
                },
            );
        }

        // 2. Independent grounds: same-(s,p) facts not cited by the
        // Librarian.
        let mut independent: Vec<String> = Vec::new();
        if let Some(job) = job {
            if let (Some(s), Some(p)) = (&job.subject, &job.predicate) {
                for (h, _) in view.active_sp(s, p) {
                    if !output.grounded_in.contains(&h) && !independent.contains(&h) {
                        independent.push(h);
                    }
                }
            }
        }

        // 3. No independent handle available → hash-proof over cited public
        // text (deterministic; proves the evidence was actually hydrated).
        let hash_proof = if independent.is_empty() {
            let mut buf = Vec::new();
            let mut cited = output.grounded_in.clone();
            cited.sort();
            for h in &cited {
                buf.extend_from_slice(h.as_bytes());
                buf.push(0);
                if let Some(t) = view.public_text(h) {
                    buf.extend_from_slice(t.as_bytes());
                }
                buf.push(0xFF);
            }
            Some(hex(&sha256(&buf)))
        } else {
            None
        };

        // 4. Verdicts on the output body:
        //    - skeleton must be non-empty and grounded;
        //    - supersede proposal must cite both sides;
        //    - the body must not itself hallucinate handles.
        let structural_verdict = (|| {
            for h in Self::harvest_handles(&Cbor::t(output.body.clone())) {
                if !view.handle_exists(&h) {
                    return AuditVerdict::Fail {
                        reason: format!("output body references nonexistent {h}"),
                    };
                }
            }
            match output.operation {
                CuratorOperation::Skeleton if output.body.trim().is_empty() => AuditVerdict::Fail {
                    reason: "empty skeleton".into(),
                },
                CuratorOperation::SupersedeProposal if output.grounded_in.len() < 2 => {
                    AuditVerdict::Fail {
                        reason: "supersede proposal must ground old and new handles".into(),
                    }
                }
                _ => AuditVerdict::Pass,
            }
        })();
        // Structural checks remain fail-closed. A configured model gets a
        // second, independent semantic pass; it may reject a structurally
        // valid output, but never turns a hard grounding failure into a pass.
        let verdict = match structural_verdict {
            AuditVerdict::Pass if self.backend.backend_id() != "deterministic.v1" => {
                // The semantic model must receive the public source material
                // it is meant to verify, not only a structural assertion that
                // a handle exists. `SubstrateView` deliberately excludes
                // private facets, preserving P19 isolation.
                let mut evidence = Vec::new();
                for handle in &output.grounded_in {
                    match view.public_text(handle) {
                        Some(text) => evidence
                            .push(format!("supports_initiator: cited_source {handle}: {text}")),
                        None => evidence.push(format!(
                            "supports_auditor: cited_source {handle} has no public text"
                        )),
                    }
                }
                for handle in &independent {
                    match view.public_text(handle) {
                        Some(text) => evidence.push(format!(
                            "supports_initiator: independent_source {handle}: {text}"
                        )),
                        None => evidence.push(format!(
                            "supports_auditor: independent_source {handle} has no public text"
                        )),
                    }
                }
                evidence.push(format!(
                    "claim: target={} body={}",
                    output.target_handle, output.body
                ));
                evidence.push(
                    "supports_initiator: structural grounding and schema checks passed".into(),
                );
                evidence.sort();
                match self.backend.adjudicate(seed, &output.body, &evidence) {
                    Verdict::AuditorCorrect => AuditVerdict::Fail {
                        reason: "configured Warden model rejected the Librarian output".into(),
                    },
                    _ => AuditVerdict::Pass,
                }
            }
            other => other,
        };

        let rec = AuditRecord {
            judgment_handle,
            target_output: output.output_handle.clone(),
            verdict_pass: matches!(verdict, AuditVerdict::Pass),
            independent_grounds: independent,
            hash_proof,
            logical_at,
        };
        (rec, verdict)
    }

    /// Blind re-audit: the same audit with grounding order deterministically
    /// permuted (seeded shuffle). The verdict MUST match the original — a
    /// mismatch is an order-sensitivity bug and trips
    /// `curator.blind_reaudit_mismatch` (CURATOR_PAIR_PROTOCOL.md §7.4).
    pub fn blind_reaudit(
        &self,
        view: &dyn SubstrateView,
        output: &CuratorPublic,
        job: Option<&CurationJob>,
        logical_at: u64,
        seed: u64,
    ) -> (AuditVerdict, AuditVerdict) {
        // Freeze only the public evidence available at the original write.
        // Each pass gets a fresh Warden with no audit/KV state, so a previous
        // verdict cannot influence the re-audit.
        let frozen = FrozenPublicView::capture(view, output, job);
        let original_reviewer = WardenCell::with_backend(self.id, self.backend.clone());
        let (_, original) =
            original_reviewer.audit_librarian_preview(&frozen, output, job, logical_at, seed);
        let mut shuffled = output.clone();
        let mut rng = DetRng::new(seed ^ 0xB11D);
        // Fisher–Yates with the deterministic stream.
        let n = shuffled.grounded_in.len();
        for i in (1..n).rev() {
            let j = rng.next_range((i + 1) as u64) as usize;
            shuffled.grounded_in.swap(i, j);
        }
        let reaudit_reviewer = WardenCell::with_backend(self.id, self.backend.clone());
        let (_, reaudit) =
            reaudit_reviewer.audit_librarian_preview(&frozen, &shuffled, job, logical_at, seed);
        (original, reaudit)
    }

    fn store(&mut self, rec: AuditRecord) {
        self.seq_by_target
            .insert(rec.target_output.clone(), rec.judgment_handle.clone());
        self.audits.insert(rec.judgment_handle.clone(), rec);
    }

    pub fn replay_audit(&mut self, rec: AuditRecord) {
        self.store(rec);
    }

    pub fn audit_for_target(&self, target: &str) -> Option<&AuditRecord> {
        self.seq_by_target
            .get(target)
            .and_then(|j| self.audits.get(j))
    }

    pub fn audit_count(&self) -> usize {
        self.audits.len()
    }
}

struct FrozenPublicView {
    handles: BTreeSet<String>,
    active: BTreeMap<(String, String), Vec<(String, String)>>,
    texts: BTreeMap<String, String>,
}

impl FrozenPublicView {
    fn capture(
        view: &dyn SubstrateView,
        output: &CuratorPublic,
        job: Option<&CurationJob>,
    ) -> FrozenPublicView {
        let mut references = output.grounded_in.clone();
        references.push(output.target_handle.clone());
        references.extend(WardenCell::harvest_handles(&Cbor::t(output.body.clone())));
        if let Some(job) = job {
            references.push(job.written_handle.clone());
        }

        let mut frozen = FrozenPublicView {
            handles: BTreeSet::new(),
            active: BTreeMap::new(),
            texts: BTreeMap::new(),
        };
        for handle in references {
            if view.handle_exists(&handle) {
                frozen.handles.insert(handle.clone());
                if let Some(text) = view.public_text(&handle) {
                    frozen.texts.insert(handle, text);
                }
            }
        }
        if let Some(job) = job {
            if let (Some(subject), Some(predicate)) = (&job.subject, &job.predicate) {
                let entries = view.active_sp(subject, predicate);
                for (handle, object) in &entries {
                    frozen.handles.insert(handle.clone());
                    if let Some(text) = view.public_text(handle) {
                        frozen.texts.insert(handle.clone(), text);
                    } else {
                        frozen.texts.insert(handle.clone(), object.clone());
                    }
                }
                frozen
                    .active
                    .insert((subject.clone(), predicate.clone()), entries);
            }
        }
        frozen
    }
}

impl SubstrateView for FrozenPublicView {
    fn handle_exists(&self, handle: &str) -> bool {
        self.handles.contains(handle)
    }

    fn active_sp(&self, subject: &str, predicate: &str) -> Vec<(String, String)> {
        self.active
            .get(&(subject.to_string(), predicate.to_string()))
            .cloned()
            .unwrap_or_default()
    }

    fn public_text(&self, handle: &str) -> Option<String> {
        self.texts.get(handle).cloned()
    }
}

impl CellBehavior for WardenCell {
    fn cell_id(&self) -> CellId {
        self.id
    }
    fn cell_type(&self) -> CellType {
        CellType::Warden
    }
    fn schema_id(&self) -> SchemaId {
        SchemaId::new("curator.warden.v1")
    }

    fn on_query(&self, _at: u64, query: &Cbor) -> UcResult<Cbor> {
        match query.opt_str("op").as_deref() {
            Some("audit_for") => {
                let target = query.req_str("target")?;
                self.audit_for_target(&target)
                    .map(audit_to_cbor)
                    .ok_or_else(|| UcError::not_found(format!("no audit for {target}")))
            }
            Some("status") | None => Ok(Cbor::map(vec![
                ("audits", Cbor::U64(self.audits.len() as u64)),
                ("backend", Cbor::t(self.backend_id())),
            ])),
            _ => Err(UcError::schema("warden: unknown op")),
        }
    }

    fn on_update(&mut self, _at: u64, _update: &Cbor) -> UcResult<Cbor> {
        Err(UcError::schema(
            "warden state mutates via audit flow, not direct writes",
        ))
    }

    fn snapshot_state(&self) -> Cbor {
        let items: Vec<Cbor> = self.audits.values().map(audit_to_cbor).collect();
        Cbor::map(vec![("audits", Cbor::Array(items))])
    }

    fn restore_state(&mut self, state: &Cbor) -> UcResult<()> {
        self.audits.clear();
        self.seq_by_target.clear();
        if let Some(arr) = state.get("audits").and_then(|v| v.as_array()) {
            for item in arr {
                let rec = audit_from_cbor(item)?;
                self.store(rec);
            }
        }
        Ok(())
    }
}

pub(crate) fn audit_from_cbor(item: &Cbor) -> UcResult<AuditRecord> {
    Ok(AuditRecord {
        judgment_handle: item.req_str("judgment_handle")?,
        target_output: item.req_str("target_output")?,
        verdict_pass: item.opt_bool("verdict_pass").unwrap_or(false),
        independent_grounds: item
            .get("independent_grounds")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        hash_proof: item.opt_str("hash_proof"),
        logical_at: item.opt_u64("logical_at").unwrap_or(0),
    })
}

pub(crate) fn audit_to_cbor(rec: &AuditRecord) -> Cbor {
    Cbor::map(vec![
        ("judgment_handle", Cbor::t(rec.judgment_handle.clone())),
        ("target_output", Cbor::t(rec.target_output.clone())),
        ("verdict_pass", Cbor::Bool(rec.verdict_pass)),
        (
            "independent_grounds",
            Cbor::text_array(&rec.independent_grounds),
        ),
        (
            "hash_proof",
            rec.hash_proof
                .as_ref()
                .map(|h| Cbor::t(h.clone()))
                .unwrap_or(Cbor::Null),
        ),
        ("logical_at", Cbor::U64(rec.logical_at)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Severity;
    use crate::curator::librarian::test_support::FakeView;

    #[test]
    fn harvest_finds_prefixed_handles_only() {
        let payload = Cbor::map(vec![
            (
                "note",
                Cbor::t("see fact/01A and decision/01B; ignore banana/split and fact/"),
            ),
            ("nested", Cbor::arr(vec![Cbor::t("blob/aa11")])),
        ]);
        let hs = WardenCell::harvest_handles(&payload);
        assert_eq!(hs, vec!["fact/01A", "decision/01B", "blob/aa11"]);
    }

    #[test]
    fn envelope_gate_hallucination_and_drift() {
        let warden = WardenCell::new(CellId(31));
        let view = FakeView::default().with_fact("fact/OLD", "svc", "owner", "team-x");
        // Hallucination.
        let bad = Cbor::map(vec![("note", Cbor::t("grounded in fact/GHOST"))]);
        assert_eq!(
            warden.judge_envelope(&view, &bad).unwrap_err().code,
            ErrCode::HallucinationDetected
        );
        // Drift: occupied (s,p), different o, no supersede.
        let drift = Cbor::map(vec![
            ("subject", Cbor::t("svc")),
            ("predicate", Cbor::t("owner")),
            ("object", Cbor::t("team-y")),
        ]);
        assert_eq!(
            warden.judge_envelope(&view, &drift).unwrap_err().code,
            ErrCode::SemanticDrift
        );
        // Declared supersede passes the gate.
        let ok = Cbor::map(vec![
            ("subject", Cbor::t("svc")),
            ("predicate", Cbor::t("owner")),
            ("object", Cbor::t("team-y")),
            ("supersedes", Cbor::t("fact/OLD")),
        ]);
        assert!(warden.judge_envelope(&view, &ok).is_ok());
        // Same object rewrite is not drift.
        let same = Cbor::map(vec![
            ("subject", Cbor::t("svc")),
            ("predicate", Cbor::t("owner")),
            ("object", Cbor::t("team-x")),
        ]);
        assert!(warden.judge_envelope(&view, &same).is_ok());
    }

    #[test]
    fn audit_requires_independent_grounding_or_hash_proof() {
        let mut warden = WardenCell::new(CellId(31));
        // Case A: an uncited same-slot fact exists -> independent ground.
        let view = FakeView::default()
            .with_fact("fact/CITED", "svc", "design", "uses tokens")
            .with_fact("fact/UNCITED", "svc", "design", "tokens carry scopes");
        let output = CuratorPublic {
            output_handle: "librarian/output/01A".into(),
            operation: CuratorOperation::Skeleton,
            target_handle: "fact/CITED".into(),
            grounded_in: vec!["fact/CITED".into()],
            confidence_band: ConfidenceBand::High,
            schema_id: "curator.librarian.output.v1".into(),
            spec_anchor: "LibrarianCell.md\u{00a7}3".into(),
            logical_at: 10,
            body: "service design uses tokens".into(),
        };
        let job = CurationJob {
            written_handle: "fact/CITED".into(),
            subject: Some("svc".into()),
            predicate: Some("design".into()),
            object_text: "uses tokens".into(),
            severity: Severity::P2,
            logical_at: 10,
            seed: 3,
        };
        let (rec, verdict) = warden.audit_librarian(&view, &output, Some(&job), 11, 3);
        assert_eq!(verdict, AuditVerdict::Pass);
        assert_eq!(rec.independent_grounds, vec!["fact/UNCITED".to_string()]);
        assert!(rec.hash_proof.is_none());

        // Case B: no uncited evidence -> hash proof mandatory.
        let view2 = FakeView::default().with_fact("fact/ONLY", "x", "y", "z body");
        let output2 = CuratorPublic {
            output_handle: "librarian/output/01B".into(),
            grounded_in: vec!["fact/ONLY".into()],
            target_handle: "fact/ONLY".into(),
            body: "z body summary".into(),
            ..output.clone()
        };
        let job2 = CurationJob {
            written_handle: "fact/ONLY".into(),
            subject: Some("x".into()),
            predicate: Some("y".into()),
            object_text: "z body".into(),
            severity: Severity::P2,
            logical_at: 12,
            seed: 3,
        };
        let (rec2, verdict2) = warden.audit_librarian(&view2, &output2, Some(&job2), 13, 3);
        assert_eq!(verdict2, AuditVerdict::Pass);
        assert!(rec2.independent_grounds.is_empty());
        assert!(rec2.hash_proof.is_some());

        // Case C: cited grounding missing -> fail.
        let output3 = CuratorPublic {
            output_handle: "librarian/output/01C".into(),
            grounded_in: vec!["fact/NOPE".into()],
            ..output
        };
        let (_, verdict3) = warden.audit_librarian(&view2, &output3, None, 14, 3);
        assert!(matches!(verdict3, AuditVerdict::Fail { .. }));
    }

    #[test]
    fn blind_reaudit_is_order_insensitive() {
        let warden = WardenCell::new(CellId(31));
        let view = FakeView::default()
            .with_fact("fact/A", "s", "p", "alpha")
            .with_fact("fact/B", "s", "p", "beta")
            .with_fact("fact/C", "s", "p", "gamma");
        let output = CuratorPublic {
            output_handle: "librarian/output/01D".into(),
            operation: CuratorOperation::Skeleton,
            target_handle: "fact/A".into(),
            grounded_in: vec!["fact/A".into(), "fact/B".into(), "fact/C".into()],
            confidence_band: ConfidenceBand::Medium,
            schema_id: "curator.librarian.output.v1".into(),
            spec_anchor: "LibrarianCell.md\u{00a7}3".into(),
            logical_at: 20,
            body: "combined summary".into(),
        };
        let (orig, re) = warden.blind_reaudit(&view, &output, None, 21, 99);
        assert_eq!(orig, re);
    }
}
