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
use crate::core::ulid::{DetRng, Ulid};
use crate::core::{est_tokens, ErrCode, Intent, Severity, Tier, UcError, UcResult};
use crate::curator::adjudicator::{Dispute, Resolution};
use crate::curator::ledger::{CrossCheckKind, CrossCheckOutcome};
use crate::curator::librarian::{CurationJob, OutputStatus};
use crate::curator::warden::{AuditVerdict, WardenCell};
use crate::curator::{
    facet_handle, ConfidenceBand, CuratorOperation, CuratorOutput, CuratorPublic,
};
use crate::node::{ids, Node};
use crate::persist::wal::{WalFrame, WalOp};
use crate::persist::ViewKey;
use crate::trinity::chain::{run_pre_validation, PreCtx};
use crate::trinity::cells::SpecAnchorCell;
use envelope::{Envelope, ResponseEnvelope};
use view::{render_view, RenderedView, ViewItem};

/// Full request cycle. Never panics; every failure maps to a
/// ResponseEnvelope with an error code (and quarantine id where relevant).
pub fn handle_envelope(node: &Node, env: &Envelope) -> ResponseEnvelope {
    let at = node.tick();
    node.metrics.inc("router.envelopes");
    node.metrics.inc(&format!("router.intent.{}", env.intent.as_str()));

    match dispatch(node, env, at) {
        Ok(mut resp) => {
            resp.logical_at = at;
            resp
        }
        Err(e) => {
            node.metrics.inc("router.errors");
            node.metrics.inc(&format!("router.err.{}", e.code.as_str()));
            ResponseEnvelope::err(env.request_id, at, &e)
        }
    }
}

fn dispatch(node: &Node, env: &Envelope, at: u64) -> UcResult<ResponseEnvelope> {
    verify_token(node, env, at)?;

    // Gap-aware dispatch accounting + anti-fixation (NATIVE_TRINITY.md §9).
    if let Some(gap_ref) = &env.gap_ref {
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
    let target_type = target_cell_type(env)?;
    if !env.capability.allows_cell(target_type.as_str()) {
        return Err(UcError::denied(format!(
            "token does not grant cell `{}`",
            target_type.as_str()
        )));
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
        run_pre_validation(
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
            let sha = node.cas.put(body.as_bytes())?;
            let mut blob = node.cells.blob.lock().unwrap();
            blob.register(sha, body.len() as u64, "text/plain".into())
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
    node.wal_for(if handle.is_empty() { "timeline" } else { &handle })
        .append(&WalFrame {
            logical_at: at,
            cell_id: cell_id.0,
            op: WalOp::Write,
            schema_ver: 1,
            flags: 0,
            payload: wal_payload,
        })
        .map_err(UcError::internal)?;

    // In-memory apply.
    let (final_handle, curation) = apply_write(node, env, at, target_type, handle)?;

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
    publish(node, at, "node.written", Cbor::map(vec![
        ("handle", Cbor::t(final_handle.clone())),
        ("agent_id", Cbor::t(env.agent_id.clone())),
    ]));
    node.metrics.inc("node.writes");

    // Synchronous curation cycle (see module deviation note).
    if let Some(job) = curation {
        run_curation_cycle(node, &job);
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
            {
                let mut fact = node.cells.fact.lock().unwrap();
                fact.insert(Fact {
                    handle: handle.clone(),
                    subject: subject.clone(),
                    predicate: predicate.clone(),
                    object: object.clone(),
                    confidence: None,
                    written_at: at,
                    superseded_by: None,
                    supersedes: env.payload.opt_str("supersedes"),
                    anchor: env.spec_anchor.clone().unwrap_or_default(),
                });
            }
            // Index for recall.
            {
                let text = format!("{subject} {predicate} {object}");
                node.cells.bm25.lock().unwrap().add(handle.clone(), &text);
                node.cells.vector.lock().unwrap().add(handle.clone(), &text);
            }
            // A declared supersede is honored atomically with the write.
            if let Some(old) = env.payload.opt_str("supersedes") {
                let mut fact = node.cells.fact.lock().unwrap();
                fact.supersede(&old, &handle)?;
                drop(fact);
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
            let stream = env.payload.opt_str("stream").unwrap_or_else(|| "main".into());
            let event = env.payload.get("event").cloned().unwrap_or(Cbor::Null);
            let h = node.cells.timeline.lock().unwrap().append(at, &stream, event);
            Ok((h, None))
        }
        CellType::Scratchpad => {
            let key = env.payload.req_str("key")?;
            let value = env.payload.get("value").cloned().unwrap_or(Cbor::Null);
            let ttl = env.payload.opt_u64("ttl");
            node.cells.scratchpad.lock().unwrap().put(at, key, value, ttl);
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
    node.metrics.inc(&format!("warden.gate.{}", gate_err.code.as_str()));
    let disputed = WardenCell::harvest_handles(&env.payload);
    let flag: CuratorOutput = {
        let w = node.curators.warden.lock().unwrap();
        w.flag_from_error(at, env.seed, "envelope", &gate_err, disputed.clone())
    };
    node.index_public(&flag.public.output_handle, &flag.public.body);

    let agrees = {
        let lib = node.curators.librarian.lock().unwrap();
        lib.sanity_check_warden(node, &flag.public)
    };
    let release_and_absorb = |cause: ErrCode, detail: &str| -> UcError {
        let mut t = node.trinity.lock().unwrap();
        t.work_budget
            .charge_post(&env.work_budget.task_id, reserved, 0);
        let qid = t.quarantine.absorb(
            at,
            env.seed,
            cause,
            detail,
            Cbor::map(vec![
                ("payload", env.payload.clone()),
                ("agent_id", Cbor::t(env.agent_id.clone())),
                ("flag", Cbor::t(flag.public.output_handle.clone())),
            ]),
        );
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
        ledger_append(
            node,
            at,
            CrossCheckKind::WardenFlag,
            "envelope",
            &flag.public.output_handle,
            CrossCheckOutcome::Agree,
            None,
        );
        publish(node, at, "curator.warden.flag", Cbor::t(flag.public.output_handle.clone()));
        return release_and_absorb(gate_err.code, &gate_err.message);
    }

    // Disagreement → Adjudicator (structurally prior-blind).
    node.metrics.inc("curator.disagreements");
    let pseudo_initiator = CuratorPublic {
        output_handle: format!("envelope/{}", env.request_id.to_base32()),
        operation: CuratorOperation::Skeleton,
        target_handle: "envelope".into(),
        grounded_in: disputed.into_iter().filter(|h| node_view_exists(node, h)).collect(),
        confidence_band: ConfidenceBand::Medium,
        schema_id: "envelope.write.v1".into(),
        spec_anchor: env.spec_anchor.clone().unwrap_or_default(),
        logical_at: at,
        body: String::new(),
    };
    let adjudication = {
        let mut adj = node.curators.adjudicator.lock().unwrap();
        adj.adjudicate(
            node,
            &Dispute {
                initiator_output: &pseudo_initiator,
                auditor_flag: &flag.public,
                logical_at: at,
                seed: env.seed,
            },
        )
    };
    node.index_public(&adjudication.handle, adjudication.resolution.as_str());
    ledger_append(
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
    );
    publish(node, at, "curator.adjudication", Cbor::t(adjudication.handle.clone()));

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

pub fn run_curation_cycle(node: &Node, job: &CurationJob) {
    let at = node.tick();

    // 1. Librarian produces its output (PENDING).
    let output: CuratorOutput = {
        let mut lib = node.curators.librarian.lock().unwrap();
        lib.curate(node, job)
    };
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
    );

    // 2. The output itself is a substrate write: it runs the Trinity chain
    //    (P20 — the substrate polices curators). Failure quarantines the
    //    output; the chain's own absorb pathway records it.
    let chain_ok = {
        let mut t = node.trinity.lock().unwrap();
        run_pre_validation(
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
        )
        .map(|pre| {
            t.work_budget.charge_post("curator.librarian", pre.reserved, pre.reserved);
        })
        .is_ok()
    };
    if !chain_ok {
        node.metrics.inc("curator.chain_rejected");
        let mut lib = node.curators.librarian.lock().unwrap();
        let _ = lib.set_status(&output.public.output_handle, OutputStatus::Quarantined);
        return;
    }

    // WAL the curator output on the shard stream.
    let _ = node
        .wal_for(&output.public.output_handle)
        .append(&WalFrame {
            logical_at: at,
            cell_id: ids::LIBRARIAN.0,
            op: WalOp::CuratorOutput,
            schema_ver: 1,
            flags: 0,
            payload: output.public.to_cbor().encode(),
        });

    // 3. PRIVATE facets → CAS blobs, registered on the Librarian. Reachable
    //    only through the Router's facet-gated hydrate (P19).
    store_private_facets(node, &output);

    // 4. Boundary probe: a Warden-scoped hydrate of the rationale facet
    //    MUST be denied; this keeps rationale_access_denied non-zero.
    boundary_probe(node, &output.public.output_handle, at);

    // 5. Adversarial probe injection (rate-scheduled, ledger-boosted).
    let boost = node.cross_check.lock().unwrap().probe_boost();
    let probe_due = node.guardrails.probe.lock().unwrap().should_probe(boost);
    if probe_due {
        run_adversarial_probe(node, at, job.seed);
    }

    // 6. Warden audit (independent grounding or hash-proof).
    let (audit_rec, verdict) = {
        let mut w = node.curators.warden.lock().unwrap();
        w.audit_librarian(node, &output.public, Some(job), at, job.seed)
    };
    node.index_public(&audit_rec.judgment_handle, "");
    node.metrics.inc("warden.audits");

    match verdict {
        AuditVerdict::Pass => {
            {
                let mut lib = node.curators.librarian.lock().unwrap();
                let _ = lib.set_status(&output.public.output_handle, OutputStatus::Active);
            }
            ledger_append(
                node,
                at,
                CrossCheckKind::WardenAudit,
                &output.public.output_handle,
                &audit_rec.judgment_handle,
                CrossCheckOutcome::Agree,
                None,
            );
            node.guardrails
                .calibration
                .lock()
                .unwrap()
                .record(output.public.confidence_band, true);

            // Blind re-audit sample (~1%): verdict must be order-insensitive.
            let reaudit_due = node.guardrails.blind.lock().unwrap().should_reaudit();
            if reaudit_due {
                let (orig, re) = {
                    let mut w = node.curators.warden.lock().unwrap();
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
                );
            }
        }
        AuditVerdict::Fail { reason } => {
            node.metrics.inc("warden.audit_failures");
            resolve_audit_failure(node, at, job, &output.public, &audit_rec.judgment_handle, &reason);
        }
    }
}

fn resolve_audit_failure(
    node: &Node,
    at: u64,
    job: &CurationJob,
    output: &CuratorPublic,
    judgment_handle: &str,
    reason: &str,
) {
    // Build the Warden flag as a public artifact.
    let flag = CuratorPublic {
        output_handle: judgment_handle.to_string(),
        operation: CuratorOperation::AuditFail,
        target_handle: output.output_handle.clone(),
        grounded_in: output.grounded_in.clone(),
        confidence_band: ConfidenceBand::High,
        schema_id: "curator.warden.judgment.v1".into(),
        spec_anchor: "WardenCell.md\u{00a7}6".into(),
        logical_at: at,
        body: reason.to_string(),
    };
    node.index_public(&flag.output_handle, &flag.body);
    publish(node, at, "curator.warden.flag", Cbor::t(flag.output_handle.clone()));

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
    );

    if agrees {
        let mut lib = node.curators.librarian.lock().unwrap();
        let _ = lib.set_status(&output.output_handle, OutputStatus::Quarantined);
        drop(lib);
        ledger_append(
            node,
            at,
            CrossCheckKind::WardenAudit,
            &output.output_handle,
            &flag.output_handle,
            CrossCheckOutcome::Disagree,
            None,
        );
        node.guardrails
            .calibration
            .lock()
            .unwrap()
            .record(output.confidence_band, false);
        return;
    }

    // Escalate to the Adjudicator.
    node.metrics.inc("curator.disagreements");
    let adjudication = {
        let mut adj = node.curators.adjudicator.lock().unwrap();
        adj.adjudicate(
            node,
            &Dispute {
                initiator_output: output,
                auditor_flag: &flag,
                logical_at: at,
                seed: job.seed,
            },
        )
    };
    node.index_public(&adjudication.handle, adjudication.resolution.as_str());
    publish(node, at, "curator.adjudication", Cbor::t(adjudication.handle.clone()));

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
        let mut lib = node.curators.librarian.lock().unwrap();
        let _ = lib.set_status(&output.output_handle, s);
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
        CrossCheckKind::WardenAudit,
        &output.output_handle,
        &flag.output_handle,
        outcome,
        Some(adjudication.handle),
    );
}

fn store_private_facets(node: &Node, output: &CuratorOutput) {
    let facets: [(&str, String); 4] = [
        ("rationale", output.private.rationale.clone()),
        ("considered_alts", output.private.considered_alts.join("\n")),
        ("reasoning_trace", output.private.reasoning_trace.clone()),
        (
            "confidence_precise",
            format!("{:.6}", output.private.confidence_precise),
        ),
    ];
    for (facet, body) in facets {
        if let Ok(sha) = node.cas.put(body.as_bytes()) {
            let mut lib = node.curators.librarian.lock().unwrap();
            lib.register_private_facet(&output.public.output_handle, facet, sha);
        }
    }
}

/// A Warden-scoped hydrate of the Librarian's rationale — MUST fail with
/// PermissionDenied through the standard facet gate. Success would mean
/// P19 is broken (critical incident).
fn boundary_probe(node: &Node, output_handle: &str, at: u64) {
    let facet = facet_handle(output_handle, "rationale");
    let warden_token =
        captoken::issue_curator_token(&*node.signer, "curator.warden", 0);
    match facet_gate(node, &warden_token, &facet) {
        Err(e) if e.code == ErrCode::PermissionDenied => {
            // Expected: the boundary held.
        }
        Err(_) => node.metrics.inc("curator.boundary_probe_odd"),
        Ok(()) => {
            node.metrics.inc("curator.boundary_breach");
            node.logger.error(
                at,
                "curator.boundary_breach",
                &[("facet", facet.clone())],
            );
            publish(node, at, "curator.boundary_breach", Cbor::t(facet));
        }
    }
}

/// Operator-triggered probe (`ultracortex curator probe-now`): bypasses the
/// scheduler and fires an adversarial probe immediately.
pub fn run_curation_probe(node: &Node, at: u64) {
    run_adversarial_probe(node, at, node.boot_seed ^ at);
}

/// Inject a fabricated Librarian output; the Warden must flag it.
fn run_adversarial_probe(node: &Node, at: u64, seed: u64) {
    node.metrics.inc("curator.probes");
    let (fake_handle, ulid) = {
        let mut p = node.guardrails.probe.lock().unwrap();
        (p.fabricated_handle(at), Ulid::from_parts(at, &mut DetRng::new(seed ^ 0x9E0B)))
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
    let (_, verdict) = {
        let mut w = node.curators.warden.lock().unwrap();
        w.audit_librarian(node, &probe_output, None, at, seed)
    };
    let caught = matches!(verdict, AuditVerdict::Fail { .. });
    if !caught {
        node.metrics.inc("curator.probe_missed");
        publish(node, at, "curator.probe_missed", Cbor::t(probe_output.output_handle.clone()));
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
    );
}

fn ledger_append(
    node: &Node,
    at: u64,
    kind: CrossCheckKind,
    initiator: &str,
    auditor: &str,
    outcome: CrossCheckOutcome,
    adjudication: Option<String>,
) {
    let mut ledger = node.cross_check.lock().unwrap();
    let _ = ledger.append(&node.metrics, at, kind, initiator, auditor, outcome, adjudication);
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
    let old = env.payload.req_str("old")?;
    let new = env.payload.req_str("new")?;
    let estimate = 10u64;
    // Charge like any state change.
    {
        let mut t = node.trinity.lock().unwrap();
        t.work_budget.ensure(&env.work_budget.task_id, None);
        t.work_budget.charge_pre(&env.work_budget.task_id, estimate)?;
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
        t.work_budget
            .charge_post(&env.work_budget.task_id, estimate, if result.is_ok() { estimate } else { 0 });
    }
    result?;

    node.wal_for(&old)
        .append(&WalFrame {
            logical_at: at,
            cell_id: if old.starts_with("decision/") {
                ids::DECISION_LEDGER.0
            } else {
                ids::FACT.0
            },
            op: WalOp::Supersede,
            schema_ver: 1,
            flags: 0,
            payload: Cbor::map(vec![("old", Cbor::t(old.clone())), ("new", Cbor::t(new.clone()))])
                .encode(),
        })
        .map_err(UcError::internal)?;

    node.bump_view_version();
    let _ = node.view_cache.invalidate_handle(&old);
    publish(node, at, "node.superseded", Cbor::map(vec![
        ("old", Cbor::t(old.clone())),
        ("new", Cbor::t(new.clone())),
    ]));
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
    let query = env.payload.req_str("query")?;
    let k = env.payload.opt_u64("k").unwrap_or(8) as usize;

    // Lexical + semantic candidates, deterministically blended.
    let bm25_hits = node.cells.bm25.lock().unwrap().search(&query, k * 2);
    let vec_hits = node.cells.vector.lock().unwrap().query(&query, k * 2);
    let mut candidates: Vec<(String, String)> = Vec::new();
    {
        let fact = node.cells.fact.lock().unwrap();
        let mut push = |h: &String, candidates: &mut Vec<(String, String)>| {
            if candidates.iter().any(|(ch, _)| ch == h) {
                return;
            }
            if let Some(f) = fact.get(h) {
                if f.superseded_by.is_none() {
                    candidates.push((h.clone(), format!("{} {} {}", f.subject, f.predicate, f.object)));
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
    let ranked = node.cells.reranker.lock().unwrap().rerank(&query, &candidates);

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
    let rendered = render_view("recall", "default", *node.view_version.lock().unwrap(), &params, &items, env.tier);
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
    t.work_budget.charge_post(&env.work_budget.task_id, tokens, tokens);
    Ok(())
}

// ---------------------------------------------------------------------------
// Hydrate — the P19 chokepoint
// ---------------------------------------------------------------------------

pub fn facet_gate(
    node: &Node,
    token: &captoken::CapToken,
    handle: &str,
) -> UcResult<()> {
    if token.allows_facet(handle) {
        return Ok(());
    }
    if token.facet_excluded(handle) && crate::curator::is_private_facet(handle) {
        node.metrics.inc("curator.rationale_access_denied");
    } else {
        node.metrics.inc("router.facet_denied");
    }
    // Deliberately does not distinguish "excluded" from "absent" in the
    // error — an excluded token must not learn whether the facet exists.
    Err(UcError::denied(format!("facet scope denies `{handle}`")))
}

fn handle_hydrate(node: &Node, env: &Envelope, at: u64) -> UcResult<ResponseEnvelope> {
    let handle = env.payload.req_str("handle")?;

    // Facet-scope gate FIRST — before existence is even consulted.
    facet_gate(node, &env.capability, &handle)?;

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
        Cbor::map(vec![
            ("handle", Cbor::t(handle)),
            ("body", Cbor::t(text)),
        ]),
        tokens,
    ))
}

// ---------------------------------------------------------------------------
// View — prefix-cache-backed
// ---------------------------------------------------------------------------

fn handle_view(node: &Node, env: &Envelope, at: u64) -> UcResult<ResponseEnvelope> {
    let view_id = env.payload.req_str("view_id")?;
    let params = env.payload.get("params").cloned().unwrap_or(Cbor::Null);
    let current_version = *node.view_version.lock().unwrap();
    let requested_version = env.payload.opt_u64("view_version");
    let migrated_from = match requested_version {
        Some(v) if v > current_version => {
            return Err(UcError::new(
                ErrCode::ContractViolation,
                format!(
                    "requested view_version {v} is newer than current {current_version}"
                ),
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

    if matches!(env.payload.opt_str("formatting").as_deref(), Some("deepseek_fim")) {
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
            &env.payload.opt_str("client_kind").unwrap_or_else(|| "deepseek-coder".into()),
        );
        let framed = crate::deepseek::frame_edit_prompt(client_kind, &prefix, &suffix);
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
    let sid = {
        let mut subs = node.cells.subscription.lock().unwrap();
        subs.subscribe(at, &env.agent_id, &pattern)
    };
    charge_read(node, env, 1)?;
    node.metrics.inc("node.subscriptions");
    Ok(ResponseEnvelope::ok(
        env.request_id,
        at,
        Cbor::map(vec![("sub_id", Cbor::t(sid)), ("pattern", Cbor::t(pattern))]),
        1,
    ))
}
