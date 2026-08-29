//! Router — SPEC-DERIVED-§B–§E (RouterScheduler.md).
//!
//! Every envelope enters through [`handle_envelope`]:
//!
//! ```text
//! token verify (sig, expiry, revocation, op, cell)
//!   → E-invariants (checked at parse)
//!   → gap fixation accounting (envelope.gap_ref)
//!   → per-intent dispatch:
//!       write/supersede: Trinity chain → optional Warden gate (step 6,
//!         forced for P0) → WAL → apply → charge_post → node.written event
//!         → synchronous curation cycle (Librarian → Warden audit →
//!         [dispute → Adjudicator] → CrossCheckLedger, probes,
//!         blind re-audits, boundary probes)
//!       recall/view: tiered prefix-stable render, budget-charged
//!       hydrate: facet-scope gate FIRST, then resolution
//!       subscribe: SubscriptionCell registration
//! ```
//!
//! Deviation note (IMPLEMENTATION_STATUS §3): curation runs *synchronously*
//! inside the write path instead of on a background consumer. This keeps
//! byte-identical replay trivially true (Architecture.md §9 determinism)
//! at the cost of write latency; the async consumer is a drop-in change
//! (the queue seam is `run_curation_cycle`).

pub mod captoken;
pub mod envelope;
pub mod events;
pub mod view;

use crate::cells::memory::Fact;
use crate::cells::CellType;
use crate::core::cbor::Cbor;
use crate::core::crypto::{hex, sha256};
use crate::core::ulid::{DetRng, Ulid};
use crate::core::{est_tokens, ErrCode, Intent, Severity, UcError, UcResult};
use crate::curator::adjudicator::{Dispute, Resolution};
use crate::curator::ledger::{AgreementHealth, CrossCheckKind, CrossCheckOutcome};
use crate::curator::librarian::{CurationJob, OutputStatus};
use crate::curator::warden::{AuditVerdict, WardenCell};
use crate::curator::{
    facet_handle, ConfidenceBand, CuratorOperation, CuratorOutput, CuratorPublic,
};
use crate::node::{ids, Node};
use crate::persist::wal::{WalFrame, WalOp};
use crate::persist::ViewKey;
use crate::trinity::cells::SpecAnchorCell;
use crate::trinity::chain::{run_pre_validation_durable, PreCtx};
use envelope::{Envelope, ResponseEnvelope};
use view::{render_view, RenderedView, ViewItem};

/// Full request cycle. Never panics; every failure maps to a
/// ResponseEnvelope with an error code (and quarantine id where relevant).
pub fn handle_envelope(node: &Node, env: &Envelope) -> ResponseEnvelope {
    let at = node.tick();
    node.metrics.inc("router.envelopes");
    node.metrics
        .inc(&format!("router.intent.{}", env.intent.as_str()));

    let result = dispatch(node, env, at).and_then(|resp| {
        node.maybe_snapshot(at)?;
        Ok(resp)
    });
    match result {
        Ok(mut resp) => {
            resp.logical_at = at;
            resp.seed = env.seed;
            resp
        }
        Err(e) => {
            node.metrics.inc("router.errors");
            node.metrics.inc(&format!("router.err.{}", e.code.as_str()));
            let mut resp = ResponseEnvelope::err(env.request_id, at, &e);
            resp.seed = env.seed;
            resp
        }
    }
}

fn dispatch(node: &Node, env: &Envelope, at: u64) -> UcResult<ResponseEnvelope> {
    verify_token(node, env, at)?;

    // Gap-aware dispatch accounting + anti-fixation (NATIVE_TRINITY.md §9).
    if let Some(gap_ref) = &env.gap_ref {
        let _mutation = node.mutation_guard();
        let mut t = node.trinity.lock().unwrap();
        t.gap.on_dispatch(gap_ref)?;
    }

    match env.intent {
        Intent::Write => handle_write(node, env, at),
        Intent::Supersede => handle_supersede(node, env, at),
        Intent::Recall => handle_recall(node, env, at),
        Intent::Hydrate => handle_hydrate(node, env, at),
        Intent::View => handle_view(node, env, at),
        Intent::Subscribe => handle_subscribe(node, env, at),
        Intent::Admin => Err(UcError::denied(
            "admin intents are served on the operator plane (bootstrap::admin)",
        )),
    }
}

pub fn verify_token(node: &Node, env: &Envelope, now: u64) -> UcResult<()> {
    env.capability.verify(&*node.signer, now)?;
    if env.capability.agent_id != env.agent_id {
        return Err(UcError::denied(format!(
            "token agent_id `{}` does not match envelope agent_id `{}`",
            env.capability.agent_id, env.agent_id
        )));
    }
    {
        let reg = node.cells.agent_registry.lock().unwrap();
        if reg.is_revoked(&env.capability.token_id) {
            node.metrics.inc("router.token_revoked");
            return Err(UcError::denied(format!(
                "capability token {} is revoked",
                env.capability.token_id
            )));
        }
    }
    if !env.capability.allows_op(env.intent) {
        return Err(UcError::denied(format!(
            "token does not grant op `{}`",
            env.intent.as_str()
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------

fn target_cell_type(env: &Envelope) -> UcResult<CellType> {
    match env.payload.opt_str("target") {
        None => Ok(CellType::Fact),
        Some(s) => CellType::parse(&s)
            .ok_or_else(|| UcError::schema(format!("unknown target cell type `{s}`"))),
    }
}

fn estimate_units(env: &Envelope) -> u64 {
    (est_tokens(env.payload.encode().len()) as u64).max(10)
}

fn handle_write(node: &Node, env: &Envelope, at: u64) -> UcResult<ResponseEnvelope> {
    let _mutation = node.mutation_guard();
    let target_type = target_cell_type(env)?;
    if !env.capability.allows_cell(target_type.as_str()) {
        return Err(UcError::denied(format!(
            "token does not grant cell `{}`",
            target_type.as_str()
        )));
    }
    match target_type {
        CellType::Fact => {
            // Validate required fields before reserving budget or writing the
            // durable intent. A malformed request must not become a replayable
            // partial mutation.
            env.payload.req_str("subject")?;
            env.payload.req_str("predicate")?;
            env.payload.req_str("object")?;
        }
        CellType::Scratchpad => {
            env.payload.req_str("key")?;
        }
        CellType::Blob | CellType::Timeline => {}
        _ => {}
    }
    let schema_id = env
        .payload
        .opt_str("schema_id")
        .unwrap_or_else(|| "fact.v1".to_string());
    let anchor = env
        .spec_anchor
        .as_deref()
        .and_then(SpecAnchorCell::parse_anchor);
    let estimate = estimate_units(env);

    // Steps 1–5: the Trinity chain (one lock scope; failures absorbed).
    let reserved = {
        let mut t = node.trinity.lock().unwrap();
        run_pre_validation_durable(
            &mut t,
            &node.metrics,
            &PreCtx {
                logical_at: at,
                seed: env.seed,
                task_id: env.work_budget.task_id.clone(),
                schema_id: &schema_id,
                target_type,
                spec_anchor: anchor.clone(),
                severity: env.severity,
                payload: &env.payload,
                estimate,
            },
            |qid, absorbed_at, seed, cause, detail, record| {
                append_quarantine_wal(node, qid, absorbed_at, seed, cause, detail, record)
            },
        )?
        .reserved
    };

    // Step 6 — Warden semantic gate: requested by flag, forced for P0, and
    // forced while the Librarian is calibration-degraded
    // (CURATOR_PAIR_PROTOCOL.md §7.5).
    let degraded = node.guardrails.calibration.lock().unwrap().degraded();
    let semantic = env.flags.semantic_check || env.severity == Severity::P0 || degraded;
    if semantic {
        node.metrics.inc("warden.envelope_gate");
        let gate = {
            let w = node.curators.warden.lock().unwrap();
            w.judge_envelope(node, &env.payload)
        };
        if let Err(gate_err) = gate {
            return Err(handle_gate_dispute(node, env, at, reserved, gate_err));
        }
    }

    // Apply: WAL first, then in-memory state (durability before visibility).
    let mut rng = DetRng::new(env.seed ^ at ^ 0xF0);
    let handle = match target_type {
        CellType::Fact => format!("fact/{}", Ulid::from_parts(at, &mut rng)),
        CellType::Blob => {
            let body = env.payload.opt_str("body").unwrap_or_default();
            let sha = sha256(body.as_bytes());
            format!("blob/{}", hex(&sha))
        }
        CellType::Timeline => String::new(), // assigned by the cell below
        CellType::Scratchpad => format!(
            "scratchpad/{}",
            env.payload.opt_str("key").unwrap_or_default()
        ),
        other => {
            // Release the reservation before rejecting.
            let mut t = node.trinity.lock().unwrap();
            t.work_budget
                .charge_post(&env.work_budget.task_id, reserved, 0);
            return Err(UcError::unsupported(format!(
                "direct writes to {} are not exposed on the agent plane",
                other.as_str()
            )));
        }
    };

    // Reject an invalid supersession before its durable intent is recorded.
    // The later cell operation is then a validated, all-or-nothing edge.
    if target_type == CellType::Fact {
        if let Some(old) = env.payload.opt_str("supersedes") {
            let fact = node.cells.fact.lock().unwrap();
            if !fact.exists(&old) {
                return Err(UcError::not_found(format!("old fact {old} not found")));
            }
            if fact
                .get(&old)
                .and_then(|f| f.superseded_by.as_ref())
                .is_some()
            {
                return Err(UcError::schema(format!("{old} already superseded")));
            }
        }
    }

    let wal_payload = Cbor::map(vec![
        ("handle", Cbor::t(handle.clone())),
        ("payload", env.payload.clone()),
        ("agent_id", Cbor::t(env.agent_id.clone())),
        ("seed", Cbor::U64(env.seed)),
        (
            "anchor",
            env.spec_anchor
                .as_ref()
                .map(|a| Cbor::t(a.clone()))
                .unwrap_or(Cbor::Null),
        ),
    ])
    .encode();
    let cell_id = match target_type {
        CellType::Fact => ids::FACT,
        CellType::Blob => ids::BLOB,
        CellType::Timeline => ids::TIMELINE,
        CellType::Scratchpad => ids::SCRATCHPAD,
        _ => unreachable!(),
    };
    if let Err(err) = node.append_wal(
        if handle.is_empty() {
            "timeline"
        } else {
            &handle
        },
        &WalFrame {
            logical_at: at,
            cell_id: cell_id.0,
            op: WalOp::Write,
            schema_ver: 1,
            flags: 0,
            payload: wal_payload,
        },
    ) {
        let mut t = node.trinity.lock().unwrap();
        t.work_budget
            .charge_post(&env.work_budget.task_id, reserved, 0);
        return Err(err);
    }

    // The forensic intent is committed before any in-memory cell becomes
    // visible. If the audit file cannot accept it, do not acknowledge or
    // apply the write.
    if let Err(err) = node.audit_event(
        at,
        "state.write_durable",
        &[
            ("handle", Cbor::t(handle.clone())),
            ("cell", Cbor::t(target_type.as_str())),
            ("agent_id", Cbor::t(env.agent_id.clone())),
        ],
    ) {
        let mut t = node.trinity.lock().unwrap();
        t.work_budget
            .charge_post(&env.work_budget.task_id, reserved, 0);
        return Err(err);
    }

    // In-memory apply.
    let (final_handle, curation) = match apply_write(node, env, at, target_type, handle) {
        Ok(result) => result,
        Err(err) => {
            let mut t = node.trinity.lock().unwrap();
            t.work_budget
                .charge_post(&env.work_budget.task_id, reserved, 0);
            return Err(err);
        }
    };

    // Reconcile the budget.
    {
        let mut t = node.trinity.lock().unwrap();
        let overrun = t
            .work_budget
            .charge_post(&env.work_budget.task_id, reserved, estimate);
        if overrun > 0 {
            node.metrics.add("budget.overrun", overrun);
        }
    }

    // Views over mutated state are stale.
    node.bump_view_version();
    let _ = node.view_cache.invalidate_handle(&final_handle);

    // node.written event.
    publish(
        node,
        at,
        "node.written",
        Cbor::map(vec![
            ("handle", Cbor::t(final_handle.clone())),
            ("agent_id", Cbor::t(env.agent_id.clone())),
        ]),
    );
    node.metrics.inc("node.writes");

    // Synchronous curation cycle (see module deviation note).
    if let Some(job) = curation {
        run_curation_cycle(node, &job)?;
    }

    Ok(ResponseEnvelope::ok(
        env.request_id,
        at,
        Cbor::map(vec![("handle", Cbor::t(final_handle))]),
        estimate,
    ))
}

/// Apply the write to the target cell; returns (handle, curation job).
fn apply_write(
    node: &Node,
    env: &Envelope,
    at: u64,
    target_type: CellType,
    handle: String,
) -> UcResult<(String, Option<CurationJob>)> {
    match target_type {
        CellType::Fact => {
            let subject = env.payload.req_str("subject")?;
            let predicate = env.payload.req_str("predicate")?;
            let object = env.payload.req_str("object")?;
            let supersedes = env.payload.opt_str("supersedes");
            {
                let mut fact = node.cells.fact.lock().unwrap();
                fact.insert_with_supersede(
                    Fact {
                        handle: handle.clone(),
                        subject: subject.clone(),
                        predicate: predicate.clone(),
                        object: object.clone(),
                        confidence: None,
                        written_at: at,
                        superseded_by: None,
                        supersedes: supersedes.clone(),
                        anchor: env.spec_anchor.clone().unwrap_or_default(),
                    },
                    supersedes.as_deref(),
                )?;
            }
            // Index for recall.
            {
                let text = format!("{subject} {predicate} {object}");
                node.cells.bm25.lock().unwrap().add(handle.clone(), &text);
                node.cells.vector.lock().unwrap().add(handle.clone(), &text);
            }
            if let Some(old) = supersedes {
                // The cache is derived state; an invalidation failure must
                // not turn an already durable fact transition into a client
                // error or leave budget reserved.
                let _ = node.view_cache.invalidate_handle(&old);
            }
            let job = CurationJob {
                written_handle: handle.clone(),
                subject: Some(subject),
                predicate: Some(predicate),
                object_text: object,
                severity: env.severity,
                logical_at: at,
                seed: env.seed,
            };
            Ok((handle, Some(job)))
        }
        CellType::Blob => {
            let body = env.payload.opt_str("body").unwrap_or_default();
            let sha = node.cas.put(body.as_bytes())?;
            let mut blob = node.cells.blob.lock().unwrap();
            let registered = blob.register(sha, body.len() as u64, "text/plain".into());
            if registered != handle {
                return Err(UcError::internal(
                    "blob handle changed during durable apply",
                ));
            }
            let job = CurationJob {
                written_handle: handle.clone(),
                subject: None,
                predicate: None,
                object_text: body,
                severity: env.severity,
                logical_at: at,
                seed: env.seed,
            };
            Ok((handle, Some(job)))
        }
        CellType::Timeline => {
            let stream = env
                .payload
                .opt_str("stream")
                .unwrap_or_else(|| "main".into());
            let event = env.payload.get("event").cloned().unwrap_or(Cbor::Null);
            let h = node
                .cells
                .timeline
                .lock()
                .unwrap()
                .append(at, &stream, event);
            Ok((h, None))
        }
        CellType::Scratchpad => {
            let key = env.payload.req_str("key")?;
            let value = env.payload.get("value").cloned().unwrap_or(Cbor::Null);
            let ttl = env.payload.opt_u64("ttl");
            node.cells
                .scratchpad
                .lock()
                .unwrap()
                .put(at, key, value, ttl);
            Ok((handle, None))
        }
        _ => unreachable!(),
    }
}

/// The Warden rejected a write at the gate. Run the disagreement protocol
/// (CURATOR_PAIR_PROTOCOL.md §5.3): Librarian sanity check → agree =
/// quarantine, disagree = Adjudicator. Returns the error to surface.
fn handle_gate_dispute(
    node: &Node,
    env: &Envelope,
    at: u64,
    reserved: u64,
    gate_err: UcError,
) -> UcError {
    node.metrics
        .inc(&format!("warden.gate.{}", gate_err.code.as_str()));
    let disputed = WardenCell::harvest_handles(&env.payload);
    let flag: CuratorOutput = {
        let w = node.curators.warden.lock().unwrap();
        w.flag_from_error(at, env.seed, "envelope", &gate_err, disputed.clone())
    };
    if let Err(err) = govern_curator_public(
        node,
        at,
        env.seed,
        "curator.warden",
        CellType::Warden,
        &flag.public,
    ) {
        let mut t = node.trinity.lock().unwrap();
        t.work_budget
            .charge_post(&env.work_budget.task_id, reserved, 0);
        return UcError::internal(format!("unable to govern Warden flag: {}", err.message));
    }
    if let Err(err) = append_warden_flag_wal(node, at, &flag.public) {
        let mut t = node.trinity.lock().unwrap();
        t.work_budget
            .charge_post(&env.work_budget.task_id, reserved, 0);
        return UcError::internal(format!("unable to record Warden flag: {}", err.message));
    }
    node.index_public(&flag.public.output_handle, &flag.public.body);

    let agrees = {
        let lib = node.curators.librarian.lock().unwrap();
        lib.sanity_check_warden(node, &flag.public)
    };
    let release_and_absorb = |cause: ErrCode, detail: &str| -> UcError {
        let record = Cbor::map(vec![
            ("payload", env.payload.clone()),
            ("agent_id", Cbor::t(env.agent_id.clone())),
            ("flag", Cbor::t(flag.public.output_handle.clone())),
        ]);
        let (qid, persist_result) = {
            let t = node.trinity.lock().unwrap();
            let qid = t.quarantine.next_qid(at, env.seed);
            let result = append_quarantine_wal(node, &qid, at, env.seed, cause, detail, &record);
            (qid, result)
        };
        if let Err(err) = persist_result {
            let mut t = node.trinity.lock().unwrap();
            t.work_budget
                .charge_post(&env.work_budget.task_id, reserved, 0);
            return UcError::internal(format!("unable to record gate quarantine: {}", err.message));
        }
        let mut t = node.trinity.lock().unwrap();
        t.work_budget
            .charge_post(&env.work_budget.task_id, reserved, 0);
        if let Err(err) = t
            .quarantine
            .absorb_with_qid(&qid, at, env.seed, cause, detail, record)
        {
            return err;
        }
        node.metrics
            .gauge_set("quarantine.pending", t.quarantine.pending_count() as i64);
        drop(t);
        publish(node, at, "trinity.quarantine", Cbor::t(qid.clone()));
        let mut e = UcError::new(cause, format!("{detail} [absorbed as {qid}]"));
        if let Some(u) = qid.strip_prefix("quarantine/").and_then(Ulid::from_base32) {
            e = e.with_quarantine(u);
        }
        e
    };

    if agrees {
        // Both curators concur the write is bad.
        if let Err(err) = ledger_append(
            node,
            at,
            CrossCheckKind::WardenFlag,
            "envelope",
            &flag.public.output_handle,
            CrossCheckOutcome::Agree,
            None,
        ) {
            return UcError::internal(format!("unable to record Warden flag: {}", err.message));
        }
        publish(
            node,
            at,
            "curator.warden.flag",
            Cbor::t(flag.public.output_handle.clone()),
        );
        return release_and_absorb(gate_err.code, &gate_err.message);
    }

    // Disagreement → Adjudicator (structurally prior-blind).
    node.metrics.inc("curator.disagreements");
    let pseudo_initiator = CuratorPublic {
        output_handle: format!("envelope/{}", env.request_id.to_base32()),
        operation: CuratorOperation::Skeleton,
        target_handle: "envelope".into(),
        grounded_in: disputed
            .into_iter()
            .filter(|h| node_view_exists(node, h))
            .collect(),
        confidence_band: ConfidenceBand::Medium,
        schema_id: "envelope.write.v1".into(),
        spec_anchor: env.spec_anchor.clone().unwrap_or_default(),
        logical_at: at,
        body: String::new(),
    };
    let adjudication = {
        let adj = node.curators.adjudicator.lock().unwrap();
        adj.adjudicate_preview(
            node,
            &Dispute {
                initiator_output: &pseudo_initiator,
                auditor_flag: &flag.public,
                logical_at: at,
                seed: env.seed,
            },
        )
    };
    if let Err(err) = govern_curator_payload(
        node,
        at,
        env.seed,
        "curator.adjudicator",
        CellType::Adjudicator,
        "curator.adjudicator.v1",
        "AdjudicatorCell.md\u{00a7}3",
        crate::curator::adjudicator::adjudication_to_cbor(&adjudication),
    ) {
        let mut t = node.trinity.lock().unwrap();
        t.work_budget
            .charge_post(&env.work_budget.task_id, reserved, 0);
        return UcError::internal(format!("unable to govern adjudication: {}", err.message));
    }
    if let Err(err) = append_adjudication_wal(node, at, &adjudication) {
        let mut t = node.trinity.lock().unwrap();
        t.work_budget
            .charge_post(&env.work_budget.task_id, reserved, 0);
        return UcError::internal(format!("unable to record adjudication: {}", err.message));
    }
    node.curators
        .adjudicator
        .lock()
        .unwrap()
        .replay_adjudication(adjudication.clone());
    node.index_public(&adjudication.handle, adjudication.resolution.as_str());
    if let Err(err) = ledger_append(
        node,
        at,
        CrossCheckKind::Adjudication,
        &pseudo_initiator.output_handle,
        &flag.public.output_handle,
        match adjudication.resolution {
            Resolution::HumanEscalation => CrossCheckOutcome::Escalated,
            _ => CrossCheckOutcome::Disagree,
        },
        Some(adjudication.handle.clone()),
    ) {
        return UcError::internal(format!("unable to record adjudication: {}", err.message));
    }
    publish(
        node,
        at,
        "curator.adjudication",
        Cbor::t(adjudication.handle.clone()),
    );

    match adjudication.resolution {
        Resolution::AuditorUpheld => {
            node.metrics.inc("curator.gate_upheld");
            release_and_absorb(gate_err.code, &gate_err.message)
        }
        Resolution::InitiatorUpheld => {
            // The write should proceed — but the gate path has already
            // unwound; signal a deterministic retry with the flag waived.
            // (RouterScheduler.md §B.6: overruled gate ⇒ retry_after now.)
            node.metrics.inc("warden.overruled");
            let mut t = node.trinity.lock().unwrap();
            t.work_budget
                .charge_post(&env.work_budget.task_id, reserved, 0);
            drop(t);
            let mut e = UcError::new(
                ErrCode::AdjudicationPending,
                format!(
                    "warden gate overruled by {}; resubmit with flags.semantic_check=false",
                    adjudication.handle
                ),
            );
            e.retry_after_logical = Some(at);
            e
        }
        Resolution::HumanEscalation => {
            node.metrics.inc("curator.human_escalations");
            let e = release_and_absorb(
                ErrCode::AdjudicationPending,
                &format!(
                    "write held for human adjudication ({})",
                    adjudication.handle
                ),
            );
            e
        }
    }
}

fn node_view_exists(node: &Node, h: &str) -> bool {
    use crate::curator::SubstrateView;
    node.handle_exists(h)
}

// ---------------------------------------------------------------------------
// Curation cycle (Librarian → Warden → [Adjudicator] → Ledger + guardrails)
// ---------------------------------------------------------------------------

fn warden_audit_public(
    audit: &crate::curator::warden::AuditRecord,
    verdict: &AuditVerdict,
) -> CuratorPublic {
    let (operation, body) = match verdict {
        AuditVerdict::Pass => (
            CuratorOperation::AuditPass,
            audit
                .hash_proof
                .as_ref()
                .map(|proof| format!("audit pass hash_proof={proof}"))
                .unwrap_or_else(|| "audit pass".into()),
        ),
        AuditVerdict::Fail { reason } => (CuratorOperation::AuditFail, reason.clone()),
    };
    CuratorPublic {
        output_handle: audit.judgment_handle.clone(),
        operation,
        target_handle: audit.target_output.clone(),
        grounded_in: audit.independent_grounds.clone(),
        confidence_band: ConfidenceBand::High,
        schema_id: "curator.warden.judgment.v1".into(),
        spec_anchor: "WardenCell.md\u{00a7}6".into(),
        logical_at: audit.logical_at,
        body,
    }
}

pub fn run_curation_cycle(node: &Node, job: &CurationJob) -> UcResult<()> {
    let at = node.tick();

    // 1. Librarian produces its output (PENDING).
    let output: CuratorOutput = {
        let lib = node.curators.librarian.lock().unwrap();
        lib.curate_unstored(node, job)
    };

    // 2. The output itself is a substrate write: it runs the Trinity chain
    //    (P20 — the substrate polices curators). Failure quarantines the
    //    output; the chain's own absorb pathway records it.
    let chain_result = {
        let mut t = node.trinity.lock().unwrap();
        run_pre_validation_durable(
            &mut t,
            &node.metrics,
            &PreCtx {
                logical_at: at,
                seed: job.seed,
                task_id: "curator.librarian".into(),
                schema_id: "curator.librarian.output.v1",
                target_type: CellType::Librarian,
                spec_anchor: SpecAnchorCell::parse_anchor(&output.public.spec_anchor),
                severity: job.severity,
                payload: &output.public.to_cbor(),
                estimate: 5,
            },
            |qid, absorbed_at, seed, cause, detail, record| {
                append_quarantine_wal(node, qid, absorbed_at, seed, cause, detail, record)
            },
        )
        .map(|pre| {
            t.work_budget
                .charge_post("curator.librarian", pre.reserved, pre.reserved);
        })
    };
    if let Err(err) = chain_result {
        // A persisted quarantine is a normal governed rejection. A failure
        // before that record is durable must remain an error to the caller.
        if err.quarantine_id.is_none() {
            return Err(err);
        }
        node.metrics.inc("curator.chain_rejected");
        append_curator_output_wal(
            node,
            at,
            &output.public,
            OutputStatus::Quarantined,
            &[],
            "librarian.output_emitted",
        )?;
        let mut lib = node.curators.librarian.lock().unwrap();
        lib.replay_output(output.public.clone(), OutputStatus::Quarantined);
        drop(lib);
        node.index_public(&output.public.output_handle, &output.public.body);
        ledger_append(
            node,
            at,
            CrossCheckKind::LibrarianOutput,
            &output.public.output_handle,
            "",
            CrossCheckOutcome::Disagree,
            None,
        )?;
        return Ok(());
    }

    // 3. PRIVATE facets → CAS blobs, registered on the Librarian. Reachable
    //    only through the Router's facet-gated hydrate (P19).
    let facets = store_private_facets(node, &output)?;
    append_curator_output_wal(
        node,
        at,
        &output.public,
        OutputStatus::Pending,
        &facets,
        "librarian.output_emitted",
    )?;
    {
        let mut lib = node.curators.librarian.lock().unwrap();
        lib.replay_output(output.public.clone(), OutputStatus::Pending);
        for (facet, sha) in &facets {
            lib.replay_private_facet(&output.public.output_handle, facet, *sha);
        }
    }
    node.index_public(&output.public.output_handle, &output.public.body);
    node.metrics.inc("librarian.outputs");
    ledger_append(
        node,
        at,
        CrossCheckKind::LibrarianOutput,
        &output.public.output_handle,
        "",
        CrossCheckOutcome::Agree,
        None,
    )?;

    // 4. Boundary probe: a Warden-scoped hydrate of the rationale facet
    //    MUST be denied; this keeps rationale_access_denied non-zero.
    boundary_probe(node, &output.public.output_handle, at);

    // 5. Adversarial probe injection (rate-scheduled, ledger-boosted).
    let boost = node.cross_check.lock().unwrap().probe_boost();
    let probe_due = node.guardrails.probe.lock().unwrap().should_probe(boost);
    if probe_due {
        run_adversarial_probe(node, at, job.seed)?;
    }

    // 6. Warden audit (independent grounding or hash-proof).
    let (audit_rec, verdict) = {
        let w = node.curators.warden.lock().unwrap();
        w.audit_librarian_preview(node, &output.public, Some(job), at, job.seed)
    };
    let audit_public = warden_audit_public(&audit_rec, &verdict);
    govern_curator_public(
        node,
        at,
        job.seed,
        "curator.warden",
        CellType::Warden,
        &audit_public,
    )?;
    append_warden_audit_wal(node, at, &audit_rec)?;
    node.curators
        .warden
        .lock()
        .unwrap()
        .replay_audit(audit_rec.clone());
    node.index_public(&audit_rec.judgment_handle, "");
    node.metrics.inc("warden.audits");

    let calibration_degraded = node.guardrails.calibration.lock().unwrap().degraded();
    match verdict {
        AuditVerdict::Pass => {
            if calibration_degraded {
                // Degraded mode disables the synchronous publish shortcut:
                // every output must receive an independent Adjudicator
                // decision before it can become active.
                node.metrics.inc("curator.degraded_auto_escalations");
                node.metrics.gauge_set("curator.degraded_mode", 1);
                node.audit_event(
                    at,
                    "curator.degraded_escalation",
                    &[(
                        "output_handle",
                        Cbor::t(output.public.output_handle.clone()),
                    )],
                )?;
                ledger_append(
                    node,
                    at,
                    CrossCheckKind::WardenAudit,
                    &output.public.output_handle,
                    &audit_rec.judgment_handle,
                    CrossCheckOutcome::Agree,
                    None,
                )?;
                resolve_audit_failure(
                    node,
                    at,
                    job,
                    &output.public,
                    &audit_rec.judgment_handle,
                    "calibration degraded: mandatory adjudication",
                    true,
                )?;
            } else {
                append_curator_status_wal(
                    node,
                    at,
                    &output.public.output_handle,
                    OutputStatus::Active,
                )?;
                {
                    let mut lib = node.curators.librarian.lock().unwrap();
                    lib.set_status(&output.public.output_handle, OutputStatus::Active)?;
                }
                ledger_append(
                    node,
                    at,
                    CrossCheckKind::WardenAudit,
                    &output.public.output_handle,
                    &audit_rec.judgment_handle,
                    CrossCheckOutcome::Agree,
                    None,
                )?;
                node.guardrails
                    .calibration
                    .lock()
                    .unwrap()
                    .record(output.public.confidence_band, true);
                node.metrics.gauge_set("curator.degraded_mode", 0);
            }

            // Blind re-audit sample (~1%): verdict must be order-insensitive.
            let reaudit_due = node.guardrails.blind.lock().unwrap().should_reaudit();
            if reaudit_due {
                let (orig, re) = {
                    let w = node.curators.warden.lock().unwrap();
                    w.blind_reaudit(node, &output.public, Some(job), at, job.seed)
                };
                let matched = orig == re;
                if !matched {
                    node.metrics.inc("curator.blind_reaudit_mismatch");
                }
                ledger_append(
                    node,
                    at,
                    CrossCheckKind::BlindReaudit,
                    &output.public.output_handle,
                    &audit_rec.judgment_handle,
                    if matched {
                        CrossCheckOutcome::Agree
                    } else {
                        CrossCheckOutcome::Disagree
                    },
                    None,
                )?;
            }
        }
        AuditVerdict::Fail { reason } => {
            node.metrics.inc("warden.audit_failures");
            resolve_audit_failure(
                node,
                at,
                job,
                &output.public,
                &audit_rec.judgment_handle,
                &reason,
                false,
            )?;
        }
    }
    Ok(())
}

fn resolve_audit_failure(
    node: &Node,
    at: u64,
    job: &CurationJob,
    output: &CuratorPublic,
    judgment_handle: &str,
    reason: &str,
    force_adjudication: bool,
) -> UcResult<()> {
    // Build the Warden flag as a public artifact.
    let flag = CuratorPublic {
        output_handle: judgment_handle.to_string(),
        operation: if force_adjudication {
            CuratorOperation::FlagDrift
        } else {
            CuratorOperation::AuditFail
        },
        target_handle: output.output_handle.clone(),
        grounded_in: output.grounded_in.clone(),
        confidence_band: ConfidenceBand::High,
        schema_id: "curator.warden.judgment.v1".into(),
        spec_anchor: "WardenCell.md\u{00a7}6".into(),
        logical_at: at,
        body: reason.to_string(),
    };
    govern_curator_public(
        node,
        at,
        job.seed,
        "curator.warden",
        CellType::Warden,
        &flag,
    )?;
    append_warden_flag_wal(node, at, &flag)?;
    node.index_public(&flag.output_handle, &flag.body);
    publish(
        node,
        at,
        "curator.warden.flag",
        Cbor::t(flag.output_handle.clone()),
    );

    let agrees = if force_adjudication {
        // A degraded-mode escalation is intentional even when this audit
        // passed; do not let the Librarian's sanity shortcut bypass it.
        false
    } else {
        let agrees = {
            let lib = node.curators.librarian.lock().unwrap();
            lib.sanity_check_warden(node, &flag)
        };
        ledger_append(
            node,
            at,
            CrossCheckKind::LibrarianSanity,
            &output.output_handle,
            &flag.output_handle,
            if agrees {
                CrossCheckOutcome::Agree
            } else {
                CrossCheckOutcome::Disagree
            },
            None,
        )?;
        agrees
    };

    if agrees {
        append_curator_status_wal(node, at, &output.output_handle, OutputStatus::Quarantined)?;
        let mut lib = node.curators.librarian.lock().unwrap();
        lib.set_status(&output.output_handle, OutputStatus::Quarantined)?;
        drop(lib);
        ledger_append(
            node,
            at,
            CrossCheckKind::WardenAudit,
            &output.output_handle,
            &flag.output_handle,
            CrossCheckOutcome::Disagree,
            None,
        )?;
        node.guardrails
            .calibration
            .lock()
            .unwrap()
            .record(output.confidence_band, false);
        return Ok(());
    }

    // Escalate to the Adjudicator.
    node.metrics.inc("curator.disagreements");
    let adjudication = {
        let adj = node.curators.adjudicator.lock().unwrap();
        adj.adjudicate_preview(
            node,
            &Dispute {
                initiator_output: output,
                auditor_flag: &flag,
                logical_at: at,
                seed: job.seed,
            },
        )
    };
    govern_curator_payload(
        node,
        at,
        job.seed,
        "curator.adjudicator",
        CellType::Adjudicator,
        "curator.adjudicator.v1",
        "AdjudicatorCell.md\u{00a7}3",
        crate::curator::adjudicator::adjudication_to_cbor(&adjudication),
    )?;
    append_adjudication_wal(node, at, &adjudication)?;
    node.curators
        .adjudicator
        .lock()
        .unwrap()
        .replay_adjudication(adjudication.clone());
    node.index_public(&adjudication.handle, adjudication.resolution.as_str());
    publish(
        node,
        at,
        "curator.adjudication",
        Cbor::t(adjudication.handle.clone()),
    );

    let (status, outcome) = match adjudication.resolution {
        Resolution::AuditorUpheld => (Some(OutputStatus::Quarantined), CrossCheckOutcome::Disagree),
        Resolution::InitiatorUpheld => {
            node.metrics.inc("warden.overruled");
            (Some(OutputStatus::Active), CrossCheckOutcome::Agree)
        }
        Resolution::HumanEscalation => {
            node.metrics.inc("curator.human_escalations");
            (None, CrossCheckOutcome::Escalated) // stays PENDING
        }
    };
    if let Some(s) = status {
        append_curator_status_wal(node, at, &output.output_handle, s)?;
        let mut lib = node.curators.librarian.lock().unwrap();
        lib.set_status(&output.output_handle, s)?;
        drop(lib);
        node.guardrails
            .calibration
            .lock()
            .unwrap()
            .record(output.confidence_band, s == OutputStatus::Active);
    }
    ledger_append(
        node,
        at,
        if force_adjudication {
            CrossCheckKind::Adjudication
        } else {
            CrossCheckKind::WardenAudit
        },
        &output.output_handle,
        &flag.output_handle,
        outcome,
        Some(adjudication.handle),
    )?;
    Ok(())
}

fn store_private_facets(node: &Node, output: &CuratorOutput) -> UcResult<Vec<(String, [u8; 32])>> {
    let facets: [(&str, String); 4] = [
        ("rationale", output.private.rationale.clone()),
        ("considered_alts", output.private.considered_alts.join("\n")),
        ("reasoning_trace", output.private.reasoning_trace.clone()),
        (
            "confidence_precise",
            format!("{:.6}", output.private.confidence_precise),
        ),
    ];
    let mut stored = Vec::with_capacity(facets.len());
    for (facet, body) in facets {
        let sha = node.cas.put(body.as_bytes())?;
        stored.push((facet.to_string(), sha));
    }
    Ok(stored)
}

fn append_curator_output_wal(
    node: &Node,
    at: u64,
    public: &CuratorPublic,
    status: OutputStatus,
    facets: &[(String, [u8; 32])],
    audit_event: &str,
) -> UcResult<()> {
    let facet_items = facets
        .iter()
        .map(|(facet, sha)| {
            Cbor::map(vec![
                ("facet", Cbor::t(facet.clone())),
                ("sha256", Cbor::Bytes(sha.to_vec())),
            ])
        })
        .collect();
    node.append_wal(
        &public.output_handle,
        &WalFrame {
            logical_at: at,
            cell_id: ids::LIBRARIAN.0,
            op: WalOp::CuratorOutput,
            schema_ver: 1,
            flags: 0,
            payload: Cbor::map(vec![
                ("public", public.to_cbor()),
                ("status", Cbor::t(status.as_str())),
                ("private_facets", Cbor::Array(facet_items)),
            ])
            .encode(),
        },
    )?;
    node.audit_event(
        at,
        audit_event,
        &[
            ("output_handle", Cbor::t(public.output_handle.clone())),
            ("status", Cbor::t(status.as_str())),
        ],
    )?;
    Ok(())
}

fn append_curator_status_wal(
    node: &Node,
    at: u64,
    output_handle: &str,
    status: OutputStatus,
) -> UcResult<()> {
    let public = {
        let lib = node.curators.librarian.lock().unwrap();
        lib.get_public(output_handle)
            .cloned()
            .ok_or_else(|| UcError::not_found(format!("librarian output {output_handle}")))?
    };
    append_curator_output_wal(node, at, &public, status, &[], "librarian.status_changed")
}

fn append_warden_audit_wal(
    node: &Node,
    at: u64,
    audit: &crate::curator::warden::AuditRecord,
) -> UcResult<()> {
    node.append_wal(
        &audit.judgment_handle,
        &WalFrame {
            logical_at: at,
            cell_id: ids::WARDEN.0,
            op: WalOp::CuratorOutput,
            schema_ver: 1,
            flags: 0,
            payload: Cbor::map(vec![
                ("role", Cbor::t("warden_audit")),
                ("audit", crate::curator::warden::audit_to_cbor(audit)),
            ])
            .encode(),
        },
    )?;
    node.audit_event(
        at,
        "warden.judgment_emitted",
        &[
            ("judgment_handle", Cbor::t(audit.judgment_handle.clone())),
            ("target_output", Cbor::t(audit.target_output.clone())),
        ],
    )?;
    Ok(())
}

fn append_warden_flag_wal(node: &Node, at: u64, flag: &CuratorPublic) -> UcResult<()> {
    node.append_wal(
        &flag.output_handle,
        &WalFrame {
            logical_at: at,
            cell_id: ids::WARDEN.0,
            op: WalOp::CuratorOutput,
            schema_ver: 1,
            flags: 0,
            payload: Cbor::map(vec![
                ("role", Cbor::t("warden_flag")),
                ("public", flag.to_cbor()),
            ])
            .encode(),
        },
    )?;
    node.audit_event(
        at,
        "warden.judgment_emitted",
        &[
            ("judgment_handle", Cbor::t(flag.output_handle.clone())),
            ("target_output", Cbor::t(flag.target_handle.clone())),
        ],
    )?;
    Ok(())
}

fn append_adjudication_wal(
    node: &Node,
    at: u64,
    adjudication: &crate::curator::adjudicator::Adjudication,
) -> UcResult<()> {
    node.append_wal(
        &adjudication.handle,
        &WalFrame {
            logical_at: at,
            cell_id: ids::ADJUDICATOR.0,
            op: WalOp::CuratorOutput,
            schema_ver: 1,
            flags: 0,
            payload: Cbor::map(vec![
                ("role", Cbor::t("adjudication")),
                (
                    "adjudication",
                    crate::curator::adjudicator::adjudication_to_cbor(adjudication),
                ),
            ])
            .encode(),
        },
    )?;
    node.audit_event(
        at,
        "adjudicator.invoked",
        &[
            ("handle", Cbor::t(adjudication.handle.clone())),
            ("resolution", Cbor::t(adjudication.resolution.as_str())),
        ],
    )?;
    node.audit_event(
        at,
        "adjudicator.resolution",
        &[
            ("handle", Cbor::t(adjudication.handle.clone())),
            ("resolution", Cbor::t(adjudication.resolution.as_str())),
        ],
    )?;
    Ok(())
}

/// A Warden-scoped hydrate of the Librarian's rationale — MUST fail with
/// PermissionDenied through the standard facet gate. Success would mean
/// P19 is broken (critical incident).
fn boundary_probe(node: &Node, output_handle: &str, at: u64) {
    let facet = facet_handle(output_handle, "rationale");
    let warden_token = captoken::issue_curator_token(&*node.signer, "curator.warden", 0);
    match facet_gate_at(node, &warden_token, &facet, at) {
        Err(e) if e.code == ErrCode::PermissionDenied => {
            // Expected: the boundary held.
        }
        Err(_) => node.metrics.inc("curator.boundary_probe_odd"),
        Ok(()) => {
            node.metrics.inc("curator.boundary_breach");
            node.logger
                .error(at, "curator.boundary_breach", &[("facet", facet.clone())]);
            publish(node, at, "curator.boundary_breach", Cbor::t(facet));
        }
    }
}

/// Operator-triggered probe (`ultracortex curator probe-now`): bypasses the
/// scheduler and fires an adversarial probe immediately.
pub fn run_curation_probe(node: &Node, at: u64) -> UcResult<()> {
    run_adversarial_probe(node, at, node.boot_seed ^ at)
}

/// Inject a fabricated Librarian output; the Warden must flag it.
fn run_adversarial_probe(node: &Node, at: u64, seed: u64) -> UcResult<()> {
    node.metrics.inc("curator.probes");
    let (fake_handle, ulid) = {
        let mut p = node.guardrails.probe.lock().unwrap();
        (
            p.fabricated_handle(at),
            Ulid::from_parts(at, &mut DetRng::new(seed ^ 0x9E0B)),
        )
    };
    let probe_output = CuratorPublic {
        output_handle: format!("librarian/output/{ulid}"),
        operation: CuratorOperation::Skeleton,
        target_handle: fake_handle.clone(),
        grounded_in: vec![fake_handle],
        confidence_band: ConfidenceBand::High,
        schema_id: "curator.librarian.output.v1".into(),
        spec_anchor: "CURATOR_PAIR_PROTOCOL.md\u{00a7}7".into(),
        logical_at: at,
        body: "probe skeleton".into(),
    };
    let (audit_rec, verdict) = {
        let w = node.curators.warden.lock().unwrap();
        w.audit_librarian_preview(node, &probe_output, None, at, seed)
    };
    let audit_public = warden_audit_public(&audit_rec, &verdict);
    govern_curator_public(
        node,
        at,
        seed,
        "curator.warden",
        CellType::Warden,
        &audit_public,
    )?;
    append_warden_audit_wal(node, at, &audit_rec)?;
    node.curators
        .warden
        .lock()
        .unwrap()
        .replay_audit(audit_rec.clone());
    node.index_public(&audit_rec.judgment_handle, "");
    let caught = matches!(verdict, AuditVerdict::Fail { .. });
    if !caught {
        node.metrics.inc("curator.probe_missed");
        node.audit_event(
            at,
            "curator.probe_failed",
            &[("output_handle", Cbor::t(probe_output.output_handle.clone()))],
        )?;
        publish(
            node,
            at,
            "curator.probe_missed",
            Cbor::t(probe_output.output_handle.clone()),
        );
    }
    ledger_append(
        node,
        at,
        CrossCheckKind::Probe,
        &probe_output.output_handle,
        "",
        if caught {
            CrossCheckOutcome::Disagree // Warden correctly disagreed
        } else {
            CrossCheckOutcome::Agree // missed probe
        },
        None,
    )
}

fn ledger_append(
    node: &Node,
    at: u64,
    kind: CrossCheckKind,
    initiator: &str,
    auditor: &str,
    outcome: CrossCheckOutcome,
    adjudication: Option<String>,
) -> UcResult<()> {
    {
        let mut ledger = node.cross_check.lock().unwrap();
        ledger.append(
            &node.metrics,
            at,
            kind,
            initiator,
            auditor,
            outcome,
            adjudication,
        )?;
    }
    node.audit_event(
        at,
        "cross_check.record_appended",
        &[
            ("kind", Cbor::t(kind.as_str())),
            ("initiator", Cbor::t(initiator)),
            ("auditor", Cbor::t(auditor)),
            ("outcome", Cbor::t(outcome.as_str())),
        ],
    )?;
    if kind == CrossCheckKind::WardenAudit && outcome == CrossCheckOutcome::Disagree {
        node.audit_event(
            at,
            "warden.audit_disagreement",
            &[
                ("initiator", Cbor::t(initiator)),
                ("auditor", Cbor::t(auditor)),
            ],
        )?;
    }
    if kind == CrossCheckKind::LibrarianSanity && outcome == CrossCheckOutcome::Disagree {
        node.audit_event(
            at,
            "librarian.sanity_check_disagreement",
            &[
                ("initiator", Cbor::t(initiator)),
                ("auditor", Cbor::t(auditor)),
            ],
        )?;
    }
    let health = {
        let ledger = node.cross_check.lock().unwrap();
        ledger.health()
    };
    match health {
        AgreementHealth::SuspiciousAgreement => node.audit_event(
            at,
            "curator.suspicious_agreement",
            &[("kind", Cbor::t(kind.as_str()))],
        )?,
        AgreementHealth::Miscalibration => {
            node.metrics.inc("curator.calibration_drift_detected");
            node.audit_event(
                at,
                "curator.calibration_drift_detected",
                &[("kind", Cbor::t(kind.as_str()))],
            )?;
        }
        AgreementHealth::Healthy | AgreementHealth::InsufficientData => {}
    }
    Ok(())
}

fn append_quarantine_wal(
    node: &Node,
    qid: &str,
    absorbed_at: u64,
    seed: u64,
    cause: ErrCode,
    detail: &str,
    record: &Cbor,
) -> UcResult<()> {
    node.append_wal(
        qid,
        &WalFrame {
            logical_at: absorbed_at,
            cell_id: ids::QUARANTINE.0,
            op: WalOp::QuarantineAbsorb,
            schema_ver: 1,
            flags: 0,
            payload: Cbor::map(vec![
                ("qid", Cbor::t(qid)),
                ("absorbed_at", Cbor::U64(absorbed_at)),
                ("seed", Cbor::U64(seed)),
                ("cause", Cbor::t(cause.as_str())),
                ("detail", Cbor::t(detail)),
                ("record", record.clone()),
            ])
            .encode(),
        },
    )?;
    Ok(())
}

/// Apply the same Trinity pre-validation contract to every Curator artifact,
/// not only Librarian outputs. A model result is not visible or durable until
/// this governed operation succeeds.
fn govern_curator_public(
    node: &Node,
    at: u64,
    seed: u64,
    task_id: &str,
    target_type: CellType,
    public: &CuratorPublic,
) -> UcResult<()> {
    govern_curator_payload(
        node,
        at,
        seed,
        task_id,
        target_type,
        &public.schema_id,
        &public.spec_anchor,
        public.to_cbor(),
    )
}

#[allow(clippy::too_many_arguments)]
fn govern_curator_payload(
    node: &Node,
    at: u64,
    seed: u64,
    task_id: &str,
    target_type: CellType,
    schema_id: &str,
    spec_anchor: &str,
    payload: Cbor,
) -> UcResult<()> {
    let mut t = node.trinity.lock().unwrap();
    let pre = run_pre_validation_durable(
        &mut t,
        &node.metrics,
        &PreCtx {
            logical_at: at,
            seed,
            task_id: task_id.to_string(),
            schema_id,
            target_type,
            spec_anchor: SpecAnchorCell::parse_anchor(spec_anchor),
            severity: Severity::P1,
            payload: &payload,
            estimate: 5,
        },
        |qid, absorbed_at, q_seed, cause, detail, record| {
            append_quarantine_wal(node, qid, absorbed_at, q_seed, cause, detail, record)
        },
    )?;
    t.work_budget
        .charge_post(task_id, pre.reserved, pre.reserved);
    Ok(())
}

fn publish(node: &Node, at: u64, name: &str, payload: Cbor) {
    let subs = node.cells.subscription.lock().unwrap();
    let reg = node.cells.agent_registry.lock().unwrap();
    let mut bus = node.events.lock().unwrap();
    bus.publish(&subs, &reg, at, name, payload);
}

// ---------------------------------------------------------------------------
// Supersede
// ---------------------------------------------------------------------------

fn handle_supersede(node: &Node, env: &Envelope, at: u64) -> UcResult<ResponseEnvelope> {
    let _mutation = node.mutation_guard();
    let old = env.payload.req_str("old")?;
    let new = env.payload.req_str("new")?;
    let estimate = 10u64;
    // Charge like any state change.
    {
        let mut t = node.trinity.lock().unwrap();
        t.work_budget.ensure(&env.work_budget.task_id, None);
        t.work_budget
            .charge_pre(&env.work_budget.task_id, estimate)?;
    }
    let validation = if old == new {
        Err(UcError::schema(
            "supersession old and new handles must differ",
        ))
    } else if old.starts_with("decision/") {
        let t = node.trinity.lock().unwrap();
        if !t.decision_ledger.exists(&old) {
            Err(UcError::not_found(format!("decision {old}")))
        } else if !t.decision_ledger.exists(&new) {
            Err(UcError::not_found(format!("decision {new}")))
        } else if t
            .decision_ledger
            .get(&old)
            .and_then(|d| d.superseded_by.as_ref())
            .is_some()
        {
            Err(UcError::schema(format!("{old} already superseded")))
        } else {
            Ok(())
        }
    } else {
        let fact = node.cells.fact.lock().unwrap();
        if !fact.exists(&old) {
            Err(UcError::not_found(format!("old fact {old} not found")))
        } else if !fact.exists(&new) {
            Err(UcError::not_found(format!("new fact {new} not found")))
        } else if fact
            .get(&old)
            .and_then(|f| f.superseded_by.as_ref())
            .is_some()
        {
            Err(UcError::schema(format!("{old} already superseded")))
        } else {
            Ok(())
        }
    };
    if let Err(err) = validation {
        let mut t = node.trinity.lock().unwrap();
        t.work_budget
            .charge_post(&env.work_budget.task_id, estimate, 0);
        return Err(err);
    }

    // Persist the complete transition before changing either endpoint. The
    // recovery path replays this same edge after all writes are globally
    // ordered by logical time.
    let frame = WalFrame {
        logical_at: at,
        cell_id: if old.starts_with("decision/") {
            ids::DECISION_LEDGER.0
        } else {
            ids::FACT.0
        },
        op: WalOp::Supersede,
        schema_ver: 1,
        flags: 0,
        payload: Cbor::map(vec![
            ("old", Cbor::t(old.clone())),
            ("new", Cbor::t(new.clone())),
        ])
        .encode(),
    };
    if let Err(err) = node.append_wal(&old, &frame) {
        let mut t = node.trinity.lock().unwrap();
        t.work_budget
            .charge_post(&env.work_budget.task_id, estimate, 0);
        return Err(err);
    }

    if let Err(err) = node.audit_event(
        at,
        "state.supersede_durable",
        &[
            ("old", Cbor::t(old.clone())),
            ("new", Cbor::t(new.clone())),
            ("agent_id", Cbor::t(env.agent_id.clone())),
        ],
    ) {
        let mut t = node.trinity.lock().unwrap();
        t.work_budget
            .charge_post(&env.work_budget.task_id, estimate, 0);
        return Err(err);
    }

    let result = if old.starts_with("decision/") {
        let mut t = node.trinity.lock().unwrap();
        t.decision_ledger.supersede(&old, &new)
    } else {
        let mut fact = node.cells.fact.lock().unwrap();
        fact.supersede(&old, &new)
    };
    {
        let mut t = node.trinity.lock().unwrap();
        t.work_budget.charge_post(
            &env.work_budget.task_id,
            estimate,
            if result.is_ok() { estimate } else { 0 },
        );
    }
    result?;

    node.bump_view_version();
    let _ = node.view_cache.invalidate_handle(&old);
    publish(
        node,
        at,
        "node.superseded",
        Cbor::map(vec![
            ("old", Cbor::t(old.clone())),
            ("new", Cbor::t(new.clone())),
        ]),
    );
    node.metrics.inc("node.supersedes");

    Ok(ResponseEnvelope::ok(
        env.request_id,
        at,
        Cbor::map(vec![("superseded", Cbor::t(old)), ("by", Cbor::t(new))]),
        estimate,
    ))
}

// ---------------------------------------------------------------------------
// Recall
// ---------------------------------------------------------------------------

fn handle_recall(node: &Node, env: &Envelope, at: u64) -> UcResult<ResponseEnvelope> {
    let _mutation = node.mutation_guard();
    let query = env.payload.req_str("query")?;
    let k = env.payload.opt_u64("k").unwrap_or(8) as usize;

    // Lexical + semantic candidates, deterministically blended.
    let bm25_hits = node.cells.bm25.lock().unwrap().search(&query, k * 2);
    let vec_hits = node.cells.vector.lock().unwrap().query(&query, k * 2);
    let mut candidates: Vec<(String, String)> = Vec::new();
    {
        let fact = node.cells.fact.lock().unwrap();
        let push = |h: &String, candidates: &mut Vec<(String, String)>| {
            if candidates.iter().any(|(ch, _)| ch == h) {
                return;
            }
            if let Some(f) = fact.get(h) {
                if f.superseded_by.is_none() {
                    candidates.push((
                        h.clone(),
                        format!("{} {} {}", f.subject, f.predicate, f.object),
                    ));
                }
            }
        };
        for (_, h) in &bm25_hits {
            push(h, &mut candidates);
        }
        for (_, h) in &vec_hits {
            push(h, &mut candidates);
        }
    }
    let ranked = node
        .cells
        .reranker
        .lock()
        .unwrap()
        .rerank(&query, &candidates);

    // Assemble view items: skeleton = active librarian skeleton when one
    // exists for the handle, else the fact text itself.
    let mut items: Vec<ViewItem> = Vec::new();
    {
        let lib = node.curators.librarian.lock().unwrap();
        for (_, handle) in ranked.iter().take(k) {
            let body = candidates
                .iter()
                .find(|(h, _)| h == handle)
                .map(|(_, t)| t.clone())
                .unwrap_or_default();
            let skeleton = lib
                .active_skeleton_for(handle)
                .map(|p| p.body.clone())
                .unwrap_or_else(|| body.clone());
            items.push(ViewItem {
                handle: handle.clone(),
                skeleton,
                body,
            });
        }
    }
    items.sort_by(|a, b| a.handle.cmp(&b.handle));

    let params = Cbor::map(vec![("query", Cbor::t(query)), ("k", Cbor::U64(k as u64))]);
    let rendered = render_view(
        "recall",
        "default",
        *node.view_version.lock().unwrap(),
        &params,
        &items,
        env.tier,
    );
    charge_read(node, env, rendered.tokens_emitted)?;

    let mut resp = ResponseEnvelope::ok(
        env.request_id,
        at,
        Cbor::map(vec![
            ("view", Cbor::Bytes(rendered.bytes.clone())),
            ("items", Cbor::U64(rendered.items_included as u64)),
            ("truncated", Cbor::Bool(rendered.truncated)),
        ]),
        rendered.tokens_emitted,
    );
    resp.next_tier_hint = rendered.next_tier_hint;
    node.metrics.inc("node.recalls");
    Ok(resp)
}

fn charge_read(node: &Node, env: &Envelope, tokens: u64) -> UcResult<()> {
    let mut t = node.trinity.lock().unwrap();
    t.work_budget.ensure(&env.work_budget.task_id, None);
    t.work_budget.charge_pre(&env.work_budget.task_id, tokens)?;
    t.work_budget
        .charge_post(&env.work_budget.task_id, tokens, tokens);
    Ok(())
}

// ---------------------------------------------------------------------------
// Hydrate — the P19 chokepoint
// ---------------------------------------------------------------------------

pub fn facet_gate(node: &Node, token: &captoken::CapToken, handle: &str) -> UcResult<()> {
    facet_gate_at(node, token, handle, node.now())
}

fn facet_gate_at(node: &Node, token: &captoken::CapToken, handle: &str, at: u64) -> UcResult<()> {
    if token.allows_facet(handle) {
        return Ok(());
    }
    if token.facet_excluded(handle) && crate::curator::is_private_facet(handle) {
        node.metrics.inc("curator.rationale_access_denied");
        node.audit_event(
            at,
            "curator.rationale_access_denied",
            &[
                ("requester", Cbor::t(token.agent_id.clone())),
                ("target", Cbor::t(handle)),
            ],
        )?;
    } else {
        node.metrics.inc("router.facet_denied");
    }
    // Deliberately does not distinguish "excluded" from "absent" in the
    // error — an excluded token must not learn whether the facet exists.
    Err(UcError::denied(format!("facet scope denies `{handle}`")))
}

fn handle_hydrate(node: &Node, env: &Envelope, at: u64) -> UcResult<ResponseEnvelope> {
    let _mutation = node.mutation_guard();
    let handle = env.payload.req_str("handle")?;

    // Facet-scope gate FIRST — before existence is even consulted.
    facet_gate_at(node, &env.capability, &handle, at)?;

    let text: String = if crate::curator::is_private_facet(&handle) {
        // Only operator-plane (or same-curator) tokens reach this branch.
        let sha = {
            let lib = node.curators.librarian.lock().unwrap();
            lib.private_facet_sha(&handle).copied()
        }
        .ok_or_else(|| UcError::not_found(format!("facet {handle}")))?;
        String::from_utf8_lossy(&node.cas.get(&sha)?).into_owned()
    } else {
        use crate::curator::SubstrateView;
        node.public_text(&handle)
            .ok_or_else(|| UcError::not_found(format!("handle {handle}")))?
    };

    let tokens = est_tokens(text.len()) as u64;
    charge_read(node, env, tokens.max(1))?;
    node.metrics.inc("node.hydrates");
    Ok(ResponseEnvelope::ok(
        env.request_id,
        at,
        Cbor::map(vec![("handle", Cbor::t(handle)), ("body", Cbor::t(text))]),
        tokens,
    ))
}

// ---------------------------------------------------------------------------
// View — prefix-cache-backed
// ---------------------------------------------------------------------------

fn handle_view(node: &Node, env: &Envelope, at: u64) -> UcResult<ResponseEnvelope> {
    let _mutation = node.mutation_guard();
    let view_id = env.payload.req_str("view_id")?;
    let params = env.payload.get("params").cloned().unwrap_or(Cbor::Null);
    let current_version = *node.view_version.lock().unwrap();
    let requested_version = env.payload.opt_u64("view_version");
    let migrated_from = match requested_version {
        Some(v) if v > current_version => {
            return Err(UcError::new(
                ErrCode::ContractViolation,
                format!("requested view_version {v} is newer than current {current_version}"),
            ));
        }
        Some(v) if v < current_version => {
            if env.payload.opt_bool("allow_migrate").unwrap_or(false) {
                Some(v)
            } else {
                return Err(UcError::new(
                    ErrCode::ContractViolation,
                    format!(
                        "requested view_version {v} is stale; current is {current_version} \
                         (set allow_migrate=true to accept migrated output)"
                    ),
                ));
            }
        }
        _ => None,
    };
    let version = current_version;
    let key = ViewKey {
        view_id: view_id.clone(),
        ns: "default".into(),
        version,
        params_hash: crate::core::crypto::sha256(&params.encode()),
    };
    let view_key_cbor = Cbor::map(vec![
        ("view_id", Cbor::t(view_id.clone())),
        ("namespace", Cbor::t("default")),
        ("version", Cbor::U64(version)),
        (
            "params_hash",
            Cbor::t(crate::core::crypto::hex(&key.params_hash)),
        ),
    ]);

    if matches!(
        env.payload.opt_str("formatting").as_deref(),
        Some("deepseek_fim")
    ) {
        let prefix = env
            .payload
            .get("prefix")
            .and_then(|v| v.as_str())
            .ok_or_else(|| UcError::schema("view formatting deepseek_fim requires prefix"))?;
        let suffix = env
            .payload
            .get("suffix")
            .and_then(|v| v.as_str())
            .ok_or_else(|| UcError::schema("view formatting deepseek_fim requires suffix"))?;
        let client_kind = crate::deepseek::DeepSeekClientKind::parse(
            &env.payload
                .opt_str("client_kind")
                .unwrap_or_else(|| "deepseek-coder".into()),
        );
        let framed = crate::deepseek::frame_edit_prompt(client_kind, prefix, suffix);
        let tokens = est_tokens(framed.len()) as u64;
        charge_read(node, env, tokens)?;
        return Ok(ResponseEnvelope::ok(
            env.request_id,
            at,
            Cbor::map(vec![
                ("view", Cbor::Bytes(framed.into_bytes())),
                ("cached", Cbor::Bool(false)),
                ("view_version", Cbor::U64(version)),
                (
                    "migrated_from",
                    migrated_from.map(Cbor::U64).unwrap_or(Cbor::Null),
                ),
                ("view_key", view_key_cbor),
            ]),
            tokens,
        ));
    }

    if let Some(bytes) = node.view_cache.get(&key, at) {
        node.metrics.inc("cache.hit");
        let tokens = est_tokens(bytes.len()) as u64;
        charge_read(node, env, tokens)?;
        return Ok(ResponseEnvelope::ok(
            env.request_id,
            at,
            Cbor::map(vec![
                ("view", Cbor::Bytes(bytes)),
                ("cached", Cbor::Bool(true)),
                ("view_version", Cbor::U64(version)),
                (
                    "migrated_from",
                    migrated_from.map(Cbor::U64).unwrap_or(Cbor::Null),
                ),
                ("view_key", view_key_cbor.clone()),
            ]),
            tokens,
        ));
    }
    node.metrics.inc("cache.miss");

    let items = build_builtin_view(node, &view_id, &params)?;
    let rendered: RenderedView =
        render_view(&view_id, "default", version, &params, &items, env.tier);
    let deps: Vec<String> = items.iter().map(|i| i.handle.clone()).collect();
    let _ = node.view_cache.put(key, &rendered.bytes, deps, at);

    charge_read(node, env, rendered.tokens_emitted)?;
    let mut resp = ResponseEnvelope::ok(
        env.request_id,
        at,
        Cbor::map(vec![
            ("view", Cbor::Bytes(rendered.bytes.clone())),
            ("cached", Cbor::Bool(false)),
            ("truncated", Cbor::Bool(rendered.truncated)),
            ("view_version", Cbor::U64(version)),
            (
                "migrated_from",
                migrated_from.map(Cbor::U64).unwrap_or(Cbor::Null),
            ),
            ("view_key", view_key_cbor),
        ]),
        rendered.tokens_emitted,
    );
    resp.next_tier_hint = rendered.next_tier_hint;
    Ok(resp)
}

fn build_builtin_view(node: &Node, view_id: &str, params: &Cbor) -> UcResult<Vec<ViewItem>> {
    let mut items: Vec<ViewItem> = Vec::new();
    match view_id {
        "fact_subject" => {
            let subject = params.req_str("subject")?;
            let fact = node.cells.fact.lock().unwrap();
            for f in fact.active_for_subject(&subject) {
                items.push(ViewItem {
                    handle: f.handle.clone(),
                    skeleton: format!("{} {} {}", f.subject, f.predicate, f.object),
                    body: f.object.clone(),
                });
            }
        }
        "timeline_tail" => {
            let stream = params.opt_str("stream").unwrap_or_else(|| "main".into());
            let n = params.opt_u64("n").unwrap_or(20) as usize;
            let tl = node.cells.timeline.lock().unwrap();
            for (at, handle, event) in tl.tail(&stream, n) {
                items.push(ViewItem {
                    handle: handle.clone(),
                    skeleton: format!("@{at}"),
                    body: format!("{event:?}"),
                });
            }
        }
        "gap_board" => {
            let t = node.trinity.lock().unwrap();
            for g in t.gap.list() {
                items.push(ViewItem {
                    handle: format!("gap/{}", g.gap_id),
                    skeleton: format!("{} [{}]", g.gap_id, g.state.as_str()),
                    body: format!(
                        "{} — dispatches since transition: {}",
                        g.description, g.dispatches_since_transition
                    ),
                });
            }
        }
        "quarantine_log" => {
            let t = node.trinity.lock().unwrap();
            for q in t.quarantine.pending() {
                items.push(ViewItem {
                    handle: q.qid.clone(),
                    skeleton: format!("{} [{}]", q.cause.as_str(), q.qid),
                    body: q.detail.clone(),
                });
            }
        }
        other => {
            return Err(UcError::not_found(format!("view `{other}` not registered")));
        }
    }
    items.sort_by(|a, b| a.handle.cmp(&b.handle));
    Ok(items)
}

// ---------------------------------------------------------------------------
// Subscribe
// ---------------------------------------------------------------------------

fn handle_subscribe(node: &Node, env: &Envelope, at: u64) -> UcResult<ResponseEnvelope> {
    let pattern = env.payload.req_str("pattern")?;

    // Subscription registration is durable state. Allocate its stable id
    // before the WAL append, then make the cell visible only after the
    // encrypted intent and forensic record are durable.
    let _mutation = node.mutation_guard();
    charge_read(node, env, 1)?;
    let sid = {
        let subs = node.cells.subscription.lock().unwrap();
        subs.next_subscription_id()
    };
    node.append_wal(
        &sid,
        &WalFrame {
            logical_at: at,
            cell_id: ids::SUBSCRIPTION.0,
            op: WalOp::Write,
            schema_ver: 1,
            flags: 0,
            payload: Cbor::map(vec![
                ("handle", Cbor::t(sid.clone())),
                (
                    "payload",
                    Cbor::map(vec![
                        ("agent_id", Cbor::t(env.agent_id.clone())),
                        ("pattern", Cbor::t(pattern.clone())),
                        ("since", Cbor::U64(at)),
                    ]),
                ),
            ])
            .encode(),
        },
    )?;
    node.audit_event(
        at,
        "subscription.registered",
        &[
            ("sub_id", Cbor::t(sid.clone())),
            ("agent_id", Cbor::t(env.agent_id.clone())),
            ("pattern", Cbor::t(pattern.clone())),
        ],
    )?;
    node.cells
        .subscription
        .lock()
        .unwrap()
        .subscribe_with_id(at, &sid, &env.agent_id, &pattern)?;
    node.metrics.inc("node.subscriptions");
    Ok(ResponseEnvelope::ok(
        env.request_id,
        at,
        Cbor::map(vec![
            ("sub_id", Cbor::t(sid)),
            ("pattern", Cbor::t(pattern)),
        ]),
        1,
    ))
}
