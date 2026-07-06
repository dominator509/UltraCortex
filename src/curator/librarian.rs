//! LibrarianCell — SPEC-DERIVED-§2–§6 (LibrarianCell.md).
//!
//! The Librarian (spec model: Gemma-2-2B; v0 default: deterministic
//! backend) consumes `node.written` events and produces organization
//! outputs: **Skeleton** (≤80-token summary of a written body),
//! **SupersedeProposal** (a new fact appears to replace an old one),
//! **ArchiveTag** (stale content marked for archival). Every output is
//! born `PENDING` and only becomes `Active` after the Warden's audit
//! passes (CURATOR_PAIR_PROTOCOL.md §5.1 — no unaudited curator output
//! is ever served).
//!
//! The Librarian also holds *escalation power, not veto power* over the
//! Warden: [`LibrarianCell::sanity_check_warden`] reviews Warden flags;
//! agreement quarantines the flagged output, disagreement escalates to
//! the Adjudicator (§5.3).

use super::{
    facet_handle, ConfidenceBand, CuratorBackend, CuratorOperation, CuratorOutput, CuratorPrivate,
    CuratorPublic, SubstrateView,
};
use crate::cells::{CellBehavior, CellType};
use crate::core::cbor::Cbor;
use crate::core::ulid::{DetRng, Ulid};
use crate::core::{CellId, SchemaId, UcError, UcResult};
use std::collections::BTreeMap;
use std::sync::Arc;

pub const SKELETON_MAX_TOKENS: usize = 80;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputStatus {
    Pending,
    Active,
    Quarantined,
}

impl OutputStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            OutputStatus::Pending => "pending",
            OutputStatus::Active => "active",
            OutputStatus::Quarantined => "quarantined",
        }
    }
    pub fn parse(s: &str) -> Option<OutputStatus> {
        Some(match s {
            "pending" => OutputStatus::Pending,
            "active" => OutputStatus::Active,
            "quarantined" => OutputStatus::Quarantined,
            _ => return None,
        })
    }
}

/// A curation job derived from a `node.written` event.
#[derive(Clone, Debug)]
pub struct CurationJob {
    pub written_handle: String,
    pub subject: Option<String>,
    pub predicate: Option<String>,
    pub object_text: String,
    pub logical_at: u64,
    pub seed: u64,
}

pub struct LibrarianCell {
    pub id: CellId,
    backend: Arc<dyn CuratorBackend>,
    /// output_handle -> (public, status)
    outputs: BTreeMap<String, (CuratorPublic, OutputStatus)>,
    /// Private facet store: facet_handle -> CAS sha (bytes live in CasStore).
    /// The Router resolves hydrates against this map *after* the facet-scope
    /// check — so an excluded token never even learns whether the facet
    /// exists (P19).
    private_facets: BTreeMap<String, [u8; 32]>,
}

impl LibrarianCell {
    pub fn new(id: CellId, backend: Arc<dyn CuratorBackend>) -> Self {
        LibrarianCell {
            id,
            backend,
            outputs: BTreeMap::new(),
            private_facets: BTreeMap::new(),
        }
    }

    /// Process one curation job. Returns the produced output (for the
    /// Router to WAL-persist, store private facets in CAS, and enqueue the
    /// Warden audit).
    pub fn curate(&mut self, view: &dyn SubstrateView, job: &CurationJob) -> CuratorOutput {
        let mut rng = DetRng::new(job.seed ^ job.logical_at ^ 0x11B);
        let ulid = Ulid::from_parts(job.logical_at, &mut rng);
        let output_handle = format!("librarian/output/{ulid}");

        // Decide the operation deterministically from substrate evidence.
        let (operation, body, grounded, precise, rationale, alts) = self.decide(view, job);

        let public = CuratorPublic {
            output_handle: output_handle.clone(),
            operation,
            target_handle: job.written_handle.clone(),
            grounded_in: grounded,
            confidence_band: ConfidenceBand::from_precise(precise),
            schema_id: "curator.librarian.output.v1".into(),
            spec_anchor: "LibrarianCell.md\u{00a7}3".into(),
            logical_at: job.logical_at,
            body,
        };
        let private = CuratorPrivate {
            rationale,
            considered_alts: alts,
            confidence_precise: precise,
            reasoning_trace: format!(
                "job={} op={} backend={}",
                job.written_handle,
                operation.as_str(),
                self.backend.backend_id()
            ),
            private_seed: rng.next_u64(),
        };
        self.outputs
            .insert(output_handle, (public.clone(), OutputStatus::Pending));
        CuratorOutput { public, private }
    }

    #[allow(clippy::type_complexity)]
    fn decide(
        &self,
        view: &dyn SubstrateView,
        job: &CurationJob,
    ) -> (
        CuratorOperation,
        String,
        Vec<String>,
        f64,
        String,
        Vec<String>,
    ) {
        // SupersedeProposal: the written fact's (s,p) slot already holds a
        // different active object — propose linking them (LibrarianCell.md §4.2).
        if let (Some(s), Some(p)) = (&job.subject, &job.predicate) {
            let existing: Vec<(String, String)> = view
                .active_sp(s, p)
                .into_iter()
                .filter(|(h, _)| h != &job.written_handle)
                .collect();
            if let Some((old_handle, old_obj)) = existing.first() {
                if old_obj != &job.object_text {
                    return (
                        CuratorOperation::SupersedeProposal,
                        format!(
                            "propose supersede: {} (\"{}\") -> {} (\"{}\") for ({s}, {p})",
                            old_handle, old_obj, job.written_handle, job.object_text
                        ),
                        vec![old_handle.clone(), job.written_handle.clone()],
                        0.7,
                        format!(
                            "slot ({s},{p}) held \"{old_obj}\" at {old_handle}; new write \
                             \"{}\" at {} has no supersedes link",
                            job.object_text, job.written_handle
                        ),
                        vec!["skeleton-only".into(), "archive old".into()],
                    );
                }
            }
        }

        // Default: Skeleton of the written body.
        let body = self
            .backend
            .skeleton(&job.object_text, SKELETON_MAX_TOKENS);
        let precise = if body.is_empty() { 0.3 } else { 0.85 };
        (
            CuratorOperation::Skeleton,
            body,
            vec![job.written_handle.clone()],
            precise,
            format!(
                "extractive skeleton of {} ({} chars source)",
                job.written_handle,
                job.object_text.len()
            ),
            vec!["archive_tag".into(), "no-op".into()],
        )
    }

    /// Record a private facet's CAS location (Router calls this after
    /// storing the blob).
    pub fn register_private_facet(&mut self, output_handle: &str, facet: &str, sha: [u8; 32]) {
        self.private_facets
            .insert(facet_handle(output_handle, facet), sha);
    }

    pub fn private_facet_sha(&self, facet_handle: &str) -> Option<&[u8; 32]> {
        self.private_facets.get(facet_handle)
    }

    /// Warden audit verdict lands here: pass → Active, fail → Quarantined
    /// (only if the Librarian agrees or the Adjudicator upholds the flag).
    pub fn set_status(&mut self, output_handle: &str, status: OutputStatus) -> UcResult<()> {
        let entry = self
            .outputs
            .get_mut(output_handle)
            .ok_or_else(|| UcError::not_found(format!("librarian output {output_handle}")))?;
        entry.1 = status;
        Ok(())
    }

    pub fn status(&self, output_handle: &str) -> Option<OutputStatus> {
        self.outputs.get(output_handle).map(|(_, s)| *s)
    }

    pub fn get_public(&self, output_handle: &str) -> Option<&CuratorPublic> {
        self.outputs.get(output_handle).map(|(p, _)| p)
    }

    /// The ACTIVE skeleton output targeting `handle`, if one exists —
    /// recall serves audited skeletons only (CURATOR_PAIR_PROTOCOL.md §5.1).
    pub fn active_skeleton_for(&self, target: &str) -> Option<&CuratorPublic> {
        self.outputs.values().find_map(|(p, s)| {
            (*s == OutputStatus::Active
                && p.operation == CuratorOperation::Skeleton
                && p.target_handle == target)
                .then_some(p)
        })
    }

    /// Sanity-check a Warden flag: does substrate evidence support it?
    /// Returns `true` (agree — the flagged output really is bad) or `false`
    /// (disagree — escalate to the Adjudicator). Escalation power, not veto
    /// (CURATOR_PAIR_PROTOCOL.md §5.3).
    pub fn sanity_check_warden(
        &self,
        view: &dyn SubstrateView,
        flag: &CuratorPublic,
    ) -> bool {
        match flag.operation {
            CuratorOperation::FlagHallucination => {
                // The flag claims some grounded_in handle doesn't exist.
                // Agree only if at least one truly is absent.
                flag.grounded_in.iter().any(|h| !view.handle_exists(h))
            }
            CuratorOperation::FlagDrift => {
                // The flag body carries "(s, p)" context in grounded_in as
                // the conflicting handles; agree if ≥2 of them are live
                // (i.e., the conflict is real).
                let live = flag
                    .grounded_in
                    .iter()
                    .filter(|h| view.handle_exists(h))
                    .count();
                live >= 2
            }
            _ => true, // non-flag operations: nothing to dispute
        }
    }

    pub fn pending(&self) -> Vec<&CuratorPublic> {
        self.outputs
            .values()
            .filter(|(_, s)| *s == OutputStatus::Pending)
            .map(|(p, _)| p)
            .collect()
    }

    pub fn counts(&self) -> (usize, usize, usize) {
        let mut p = 0;
        let mut a = 0;
        let mut q = 0;
        for (_, s) in self.outputs.values() {
            match s {
                OutputStatus::Pending => p += 1,
                OutputStatus::Active => a += 1,
                OutputStatus::Quarantined => q += 1,
            }
        }
        (p, a, q)
    }
}

impl CellBehavior for LibrarianCell {
    fn cell_id(&self) -> CellId {
        self.id
    }
    fn cell_type(&self) -> CellType {
        CellType::Librarian
    }
    fn schema_id(&self) -> SchemaId {
        SchemaId::new("curator.librarian.v1")
    }

    fn on_query(&self, _at: u64, query: &Cbor) -> UcResult<Cbor> {
        match query.opt_str("op").as_deref() {
            Some("get") => {
                let h = query.req_str("output_handle")?;
                let (public, status) = self
                    .outputs
                    .get(&h)
                    .ok_or_else(|| UcError::not_found(format!("librarian output {h}")))?;
                let mut c = public.to_cbor();
                if let Cbor::Map(pairs) = &mut c {
                    pairs.push((Cbor::Text("status".into()), Cbor::t(status.as_str())));
                }
                Ok(c)
            }
            Some("status") | None => {
                let (p, a, q) = self.counts();
                Ok(Cbor::map(vec![
                    ("pending", Cbor::U64(p as u64)),
                    ("active", Cbor::U64(a as u64)),
                    ("quarantined", Cbor::U64(q as u64)),
                    ("backend", Cbor::t(self.backend.backend_id())),
                ]))
            }
            _ => Err(UcError::schema("librarian: unknown op")),
        }
    }

    fn on_update(&mut self, _at: u64, update: &Cbor) -> UcResult<Cbor> {
        // Status transitions arrive via the Router's audit flow.
        let h = update.req_str("output_handle")?;
        let status = OutputStatus::parse(&update.req_str("status")?)
            .ok_or_else(|| UcError::schema("librarian: bad status"))?;
        self.set_status(&h, status)?;
        Ok(Cbor::map(vec![
            ("output_handle", Cbor::t(h)),
            ("status", Cbor::t(status.as_str())),
        ]))
    }

    fn snapshot_state(&self) -> Cbor {
        let outputs: Vec<Cbor> = self
            .outputs
            .values()
            .map(|(p, s)| {
                let mut c = p.to_cbor();
                if let Cbor::Map(pairs) = &mut c {
                    pairs.push((Cbor::Text("status".into()), Cbor::t(s.as_str())));
                }
                c
            })
            .collect();
        let facets: Vec<Cbor> = self
            .private_facets
            .iter()
            .map(|(h, sha)| {
                Cbor::map(vec![
                    ("facet", Cbor::t(h.clone())),
                    ("sha256", Cbor::Bytes(sha.to_vec())),
                ])
            })
            .collect();
        Cbor::map(vec![
            ("outputs", Cbor::Array(outputs)),
            ("private_facets", Cbor::Array(facets)),
        ])
    }

    fn restore_state(&mut self, state: &Cbor) -> UcResult<()> {
        self.outputs.clear();
        self.private_facets.clear();
        if let Some(arr) = state.get("outputs").and_then(|v| v.as_array()) {
            for item in arr {
                let public = CuratorPublic::from_cbor(item)?;
                let status = item
                    .opt_str("status")
                    .and_then(|s| OutputStatus::parse(&s))
                    .unwrap_or(OutputStatus::Pending);
                self.outputs
                    .insert(public.output_handle.clone(), (public, status));
            }
        }
        if let Some(arr) = state.get("private_facets").and_then(|v| v.as_array()) {
            for item in arr {
                let facet = item.req_str("facet")?;
                if let Some(b) = item.get("sha256").and_then(|v| v.as_bytes()) {
                    if b.len() == 32 {
                        let mut sha = [0u8; 32];
                        sha.copy_from_slice(b);
                        self.private_facets.insert(facet, sha);
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::collections::BTreeMap;

    /// Minimal substrate double for curator tests.
    #[derive(Default)]
    pub struct FakeView {
        pub handles: std::collections::BTreeSet<String>,
        pub sp: BTreeMap<(String, String), Vec<(String, String)>>,
        pub texts: BTreeMap<String, String>,
    }

    impl FakeView {
        pub fn with_handle(mut self, h: &str) -> Self {
            self.handles.insert(h.to_string());
            self
        }
        pub fn with_fact(mut self, h: &str, s: &str, p: &str, o: &str) -> Self {
            self.handles.insert(h.to_string());
            self.sp
                .entry((s.to_string(), p.to_string()))
                .or_default()
                .push((h.to_string(), o.to_string()));
            self.texts.insert(h.to_string(), o.to_string());
            self
        }
    }

    impl SubstrateView for FakeView {
        fn handle_exists(&self, handle: &str) -> bool {
            self.handles.contains(handle)
        }
        fn active_sp(&self, subject: &str, predicate: &str) -> Vec<(String, String)> {
            self.sp
                .get(&(subject.to_string(), predicate.to_string()))
                .cloned()
                .unwrap_or_default()
        }
        fn public_text(&self, handle: &str) -> Option<String> {
            self.texts.get(handle).cloned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::FakeView;
    use super::*;
    use crate::curator::DeterministicBackend;

    fn librarian() -> LibrarianCell {
        LibrarianCell::new(CellId(30), Arc::new(DeterministicBackend))
    }

    #[test]
    fn skeleton_output_is_pending_and_deterministic() {
        let mut lib = librarian();
        let view = FakeView::default().with_handle("fact/01A");
        let job = CurationJob {
            written_handle: "fact/01A".into(),
            subject: Some("svc.auth".into()),
            predicate: Some("design".into()),
            object_text: "The auth service uses capability tokens. Tokens carry facet scopes. \
                          Scopes are enforced by the router before dispatch."
                .into(),
            logical_at: 100,
            seed: 7,
        };
        let out1 = lib.curate(&view, &job);
        assert_eq!(out1.public.operation, CuratorOperation::Skeleton);
        assert_eq!(lib.status(&out1.public.output_handle), Some(OutputStatus::Pending));
        assert!(!out1.public.body.is_empty());
        // Determinism: same job in a fresh librarian => identical output.
        let mut lib2 = librarian();
        let out2 = lib2.curate(&view, &job);
        assert_eq!(out1.public.output_handle, out2.public.output_handle);
        assert_eq!(out1.public.body, out2.public.body);
        assert_eq!(out1.private.private_seed, out2.private.private_seed);
    }

    #[test]
    fn supersede_proposal_when_sp_slot_occupied() {
        let mut lib = librarian();
        let view = FakeView::default()
            .with_fact("fact/OLD", "svc.auth", "owner", "team-x")
            .with_handle("fact/NEW");
        let job = CurationJob {
            written_handle: "fact/NEW".into(),
            subject: Some("svc.auth".into()),
            predicate: Some("owner".into()),
            object_text: "team-y".into(),
            logical_at: 200,
            seed: 7,
        };
        let out = lib.curate(&view, &job);
        assert_eq!(out.public.operation, CuratorOperation::SupersedeProposal);
        assert!(out.public.grounded_in.contains(&"fact/OLD".to_string()));
        assert!(out.public.grounded_in.contains(&"fact/NEW".to_string()));
    }

    #[test]
    fn sanity_check_agrees_with_true_hallucination_flag() {
        let lib = librarian();
        let view = FakeView::default().with_handle("fact/REAL");
        let flag = CuratorPublic {
            output_handle: "warden/judgment/01Z".into(),
            operation: CuratorOperation::FlagHallucination,
            target_handle: "librarian/output/01Q".into(),
            grounded_in: vec!["fact/GHOST".into()],
            confidence_band: ConfidenceBand::High,
            schema_id: "curator.warden.judgment.v1".into(),
            spec_anchor: "WardenCell.md\u{00a7}4".into(),
            logical_at: 5,
            body: "cites nonexistent fact/GHOST".into(),
        };
        assert!(lib.sanity_check_warden(&view, &flag)); // agree
        // But a flag citing a real handle draws disagreement -> escalation.
        let bogus_flag = CuratorPublic {
            grounded_in: vec!["fact/REAL".into()],
            ..flag
        };
        assert!(!lib.sanity_check_warden(&view, &bogus_flag));
    }

    #[test]
    fn status_transitions_and_snapshot() {
        let mut lib = librarian();
        let view = FakeView::default().with_handle("fact/X");
        let job = CurationJob {
            written_handle: "fact/X".into(),
            subject: None,
            predicate: None,
            object_text: "Body text to skeletonize. It has two sentences.".into(),
            logical_at: 9,
            seed: 1,
        };
        let out = lib.curate(&view, &job);
        lib.register_private_facet(&out.public.output_handle, "rationale", [9u8; 32]);
        lib.set_status(&out.public.output_handle, OutputStatus::Active)
            .unwrap();
        let snap = lib.snapshot_state();
        let mut lib2 = librarian();
        lib2.restore_state(&snap).unwrap();
        assert_eq!(lib2.status(&out.public.output_handle), Some(OutputStatus::Active));
        assert_eq!(
            lib2.private_facet_sha(&facet_handle(&out.public.output_handle, "rationale")),
            Some(&[9u8; 32])
        );
        assert_eq!(snap.encode(), lib2.snapshot_state().encode());
    }
}
