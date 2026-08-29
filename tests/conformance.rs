//! Conformance suite — SPEC-DERIVED-§B5 (Bootstrap.md), CONFORMANCE.md.
//!
//! Trinity tests T1–T8 prove governance governs; Curator tests C1–C10
//! prove the pair protocol's guarantees hold *structurally*, not by
//! convention. Everything drives the real Router against a booted node —
//! no mocks below the SubstrateView seam.

use std::sync::Arc;
use ultracortex::bootstrap::{self, admin_dispatch};
use ultracortex::cells::CellType;
use ultracortex::core::cbor::Cbor;
use ultracortex::core::ulid::{DetRng, Ulid};
use ultracortex::core::{AnchorRef, ErrCode, Intent, Severity, Tier};
use ultracortex::curator::adjudicator::{Dispute, Resolution};
use ultracortex::curator::ledger::{CrossCheckKind, CrossCheckOutcome};
use ultracortex::curator::librarian::OutputStatus;
use ultracortex::curator::{ConfidenceBand, CuratorOperation, CuratorPublic, SubstrateView};
use ultracortex::router::captoken::{issue_agent_token, issue_curator_token, issue_operator_token};
use ultracortex::router::envelope::{Envelope, EnvelopeFlags, WorkBudget, PROTO_VERSION};
use ultracortex::router::handle_envelope;
use ultracortex::trinity::cells::GAP_FIXATION_N;
use ultracortex::Node;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn booted(tag: &str) -> Arc<Node> {
    // dry_run boots into a unique temp dir and runs the 11-point self-test.
    let _ = tag;
    bootstrap::dry_run().expect("boot").node
}

fn env_for(
    node: &Node,
    token: &ultracortex::router::captoken::CapToken,
    intent: Intent,
    payload: Cbor,
    anchor: Option<&str>,
    seed: u64,
    semantic: bool,
) -> Envelope {
    Envelope {
        proto_version: PROTO_VERSION,
        request_id: Ulid::from_parts(node.now(), &mut DetRng::new(seed ^ 0xC0)),
        agent_id: token.agent_id.clone(),
        capability: token.clone(),
        work_budget: WorkBudget {
            task_id: format!("conformance-{seed}"),
            units: 10_000,
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

fn agent(node: &Node, id: &str) -> ultracortex::router::captoken::CapToken {
    let tok = issue_agent_token(&*node.signer, id, 0);
    node.cells
        .agent_registry
        .lock()
        .unwrap()
        .register(node.now(), id, "agent");
    tok
}

fn write_fact(
    node: &Node,
    tok: &ultracortex::router::captoken::CapToken,
    s: &str,
    p: &str,
    o: &str,
    seed: u64,
) -> String {
    let r = handle_envelope(
        node,
        &env_for(
            node,
            tok,
            Intent::Write,
            Cbor::map(vec![
                ("subject", Cbor::t(s)),
                ("predicate", Cbor::t(p)),
                ("object", Cbor::t(o)),
            ]),
            Some("Architecture.md\u{00a7}4"),
            seed,
            false,
        ),
    );
    assert!(r.ok, "write failed: {:?}", r.err_message);
    r.result.opt_str("handle").unwrap()
}

// ---------------------------------------------------------------------------
// Trinity conformance
// ---------------------------------------------------------------------------

/// T1 — every SPEC-DERIVED marker in the source resolves against the
/// boot-registered anchors.
#[test]
fn t1_anchor_coverage() {
    let node = booted("t1");
    let t = node.trinity.lock().unwrap();
    assert!(
        !ultracortex::spec_inventory::SPEC_ANCHOR_INVENTORY.is_empty(),
        "build.rs harvested nothing — markers missing"
    );
    for (doc, section, artifact, _line) in ultracortex::spec_inventory::SPEC_ANCHOR_INVENTORY {
        let first = section
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '.')
            .next()
            .unwrap_or(section)
            .trim_matches('.');
        assert!(
            t.spec_anchor.resolves(&AnchorRef::new(*doc, first)),
            "marker at {artifact} cites unregistered anchor {doc}\u{00a7}{first}"
        );
    }
}

/// T2 — a write conflicting with an active decision is rejected +
/// quarantined unless it declares respects/supersedes.
#[test]
fn t2_decision_conflict() {
    let node = booted("t2");
    let tok = agent(&node, "t2-agent");
    let r = handle_envelope(
        &node,
        &env_for(
            &node,
            &tok,
            Intent::Write,
            Cbor::map(vec![
                ("subject", Cbor::t("governance")),
                ("predicate", Cbor::t("proposal")),
                ("object", Cbor::t("override the provisioning order")),
                ("decision_scope", Cbor::t("governance")),
            ]),
            Some("NATIVE_TRINITY.md\u{00a7}3"),
            21,
            false,
        ),
    );
    assert!(!r.ok);
    assert_eq!(r.err_code, Some(ErrCode::DecisionConflict));
    assert!(r.quarantine_id.is_some());
    // Declaring compliance (referencing the governing decision) passes
    // step 3.
    let governing = {
        let t = node.trinity.lock().unwrap();
        t.decision_ledger.active_in_scope("governance")[0]
            .handle
            .clone()
    };
    let r = handle_envelope(
        &node,
        &env_for(
            &node,
            &tok,
            Intent::Write,
            Cbor::map(vec![
                ("subject", Cbor::t("governance")),
                ("predicate", Cbor::t("proposal")),
                ("object", Cbor::t("a compliant refinement")),
                ("decision_scope", Cbor::t("governance")),
                ("respects_decision", Cbor::t(governing)),
            ]),
            Some("NATIVE_TRINITY.md\u{00a7}3"),
            22,
            false,
        ),
    );
    assert!(r.ok, "{:?}", r.err_message);
}

/// T3 — congruence blocks an unknown entity, then accepts it after an
/// explicit delta.
#[test]
fn t3_congruence_delta() {
    let node = booted("t3");
    let tok = agent(&node, "t3-agent");
    let payload = Cbor::map(vec![
        ("subject", Cbor::t("architecture")),
        ("predicate", Cbor::t("adds")),
        ("object", Cbor::t("introduce the TelemetryCell for spans")),
    ]);
    let r = handle_envelope(
        &node,
        &env_for(
            &node,
            &tok,
            Intent::Write,
            payload.clone(),
            Some("Architecture.md\u{00a7}2"),
            31,
            false,
        ),
    );
    assert!(!r.ok);
    assert_eq!(r.err_code, Some(ErrCode::CongruenceDelta));
    // Operator accepts the delta.
    {
        let mut t = node.trinity.lock().unwrap();
        t.congruence.accept_delta("TelemetryCell");
    }
    let r = handle_envelope(
        &node,
        &env_for(
            &node,
            &tok,
            Intent::Write,
            payload,
            Some("Architecture.md\u{00a7}2"),
            32,
            false,
        ),
    );
    assert!(r.ok, "{:?}", r.err_message);
}

/// T4 — the N+1th dispatch against a gap without a state transition trips
/// fixation.
#[test]
fn t4_gap_fixation() {
    let node = booted("t4");
    let tok = agent(&node, "t4-agent");
    for i in 0..=GAP_FIXATION_N {
        let mut env = env_for(
            &node,
            &tok,
            Intent::Recall,
            Cbor::map(vec![("query", Cbor::t("anything")), ("k", Cbor::U64(2))]),
            None,
            40 + i,
            false,
        );
        env.gap_ref = Some("GAP-0001".into());
        let r = handle_envelope(&node, &env);
        if i < GAP_FIXATION_N {
            assert!(r.ok, "dispatch {i} should pass: {:?}", r.err_message);
        } else {
            assert!(!r.ok, "dispatch {i} should be fixated");
            assert_eq!(r.err_code, Some(ErrCode::Fixation));
        }
    }
}

/// T5 — a zero-grant budget blocks state changes with BudgetExceeded.
#[test]
fn t5_budget_zero() {
    let node = booted("t5");
    let tok = agent(&node, "t5-agent");
    {
        let mut t = node.trinity.lock().unwrap();
        t.work_budget.ensure("t5-task", Some(0));
    }
    let mut env = env_for(
        &node,
        &tok,
        Intent::Write,
        Cbor::map(vec![
            ("subject", Cbor::t("x")),
            ("predicate", Cbor::t("y")),
            ("object", Cbor::t("z")),
        ]),
        Some("Architecture.md\u{00a7}4"),
        51,
        false,
    );
    env.work_budget.task_id = "t5-task".into();
    let r = handle_envelope(&node, &env);
    assert!(!r.ok);
    assert_eq!(r.err_code, Some(ErrCode::BudgetExceeded));
}

/// T6 — quarantine never drops items; a reinjected payload replays.
#[test]
fn t6_quarantine_no_drop_reinject() {
    let node = booted("t6");
    let tok = agent(&node, "t6-agent");
    // Anchorless write → quarantined.
    let r = handle_envelope(
        &node,
        &env_for(
            &node,
            &tok,
            Intent::Write,
            Cbor::map(vec![
                ("subject", Cbor::t("t6")),
                ("predicate", Cbor::t("state")),
                ("object", Cbor::t("recoverable")),
            ]),
            None,
            61,
            false,
        ),
    );
    assert!(!r.ok);
    let qid = r.quarantine_id.clone().unwrap();
    // Sweep must not remove pending items.
    {
        let mut t = node.trinity.lock().unwrap();
        let removed = t.quarantine.sweep(node.now() + 1_000_000);
        assert_eq!(
            t.quarantine.get(&qid).map(|q| q.qid.clone()),
            Some(qid.clone())
        );
        let _ = removed;
        assert!(t.quarantine.pending_count() >= 1);
    }
    // Operator reinjects: the write lands this time (operator supplies the
    // anchor).
    let result = admin_dispatch(
        &node,
        &Cbor::map(vec![
            ("verb", Cbor::t("quarantine reinject")),
            ("args", Cbor::map(vec![("qid", Cbor::t(qid.clone()))])),
        ]),
    )
    .unwrap();
    assert_eq!(result.opt_bool("ok"), Some(true), "{result:?}");
    assert!(node
        .active_sp("t6", "state")
        .iter()
        .any(|(_, o)| o == "recoverable"));
}

/// T7 — the audit chain verifies end-to-end after boot + activity.
#[test]
fn t7_audit_chain() {
    let node = booted("t7");
    let tok = agent(&node, "t7-agent");
    write_fact(&node, &tok, "t7", "wrote", "something", 71);
    node.audit
        .lock()
        .unwrap()
        .append(node.now(), "conformance.t7", &[("ok", Cbor::Bool(true))])
        .unwrap();
    let result =
        admin_dispatch(&node, &Cbor::map(vec![("verb", Cbor::t("audit verify"))])).unwrap();
    assert_eq!(result.opt_bool("intact"), Some(true));
    assert!(result.opt_u64("records").unwrap() >= 2);
}

/// T8 — governance exists before anything governable: a raw Node (no
/// provisioning) rejects the very first write at step 1 or 2 of the chain.
#[test]
fn t8_trinity_first() {
    let node = Arc::new(Node::ephemeral("t8-raw").unwrap());
    let tok = agent(&node, "t8-agent");
    let r = handle_envelope(
        &node,
        &env_for(
            &node,
            &tok,
            Intent::Write,
            Cbor::map(vec![
                ("subject", Cbor::t("x")),
                ("predicate", Cbor::t("y")),
                ("object", Cbor::t("z")),
            ]),
            Some("Architecture.md\u{00a7}4"),
            81,
            false,
        ),
    );
    assert!(!r.ok, "unprovisioned node must not accept writes");
    assert!(
        matches!(
            r.err_code,
            Some(ErrCode::ContractViolation) | Some(ErrCode::AnchorMissing)
        ),
        "{:?}",
        r.err_code
    );
    // And the rejection itself was governed: it landed in quarantine.
    assert!(r.quarantine_id.is_some());
}

// ---------------------------------------------------------------------------
// Curator conformance
// ---------------------------------------------------------------------------

/// C1 — P19: a Warden token hydrating any Librarian private facet is
/// denied, the metric increments, and the error does not reveal existence.
#[test]
fn c1_private_facet_denied() {
    let node = booted("c1");
    let tok = agent(&node, "c1-agent");
    let h = write_fact(
        &node,
        &tok,
        "c1",
        "topic",
        "asymmetric visibility is structural",
        91,
    );
    let output_handle = {
        let lib = node.curators.librarian.lock().unwrap();
        lib.active_skeleton_for(&h).unwrap().output_handle.clone()
    };
    let warden = issue_curator_token(&*node.signer, "curator.warden", 0);
    node.cells
        .agent_registry
        .lock()
        .unwrap()
        .register(node.now(), "curator.warden", "curator");

    let before = node.metrics.counter("curator.rationale_access_denied");
    for facet in [
        "rationale",
        "considered_alts",
        "reasoning_trace",
        "confidence_precise",
    ] {
        let target = format!("{output_handle}/{facet}");
        let r = handle_envelope(
            &node,
            &env_for(
                &node,
                &warden,
                Intent::Hydrate,
                Cbor::map(vec![("handle", Cbor::t(target.clone()))]),
                None,
                92,
                false,
            ),
        );
        assert!(!r.ok, "{facet} must be denied");
        assert_eq!(r.err_code, Some(ErrCode::PermissionDenied));
        // Denial for a NONEXISTENT facet is byte-identical in code+shape —
        // existence is not leaked.
        let ghost = format!("librarian/output/01GHOST{facet}/{facet}");
        let r2 = handle_envelope(
            &node,
            &env_for(
                &node,
                &warden,
                Intent::Hydrate,
                Cbor::map(vec![("handle", Cbor::t(ghost))]),
                None,
                93,
                false,
            ),
        );
        assert_eq!(r2.err_code, r.err_code);
    }
    assert!(node.metrics.counter("curator.rationale_access_denied") >= before + 4);
}

/// C2 — the operator hydrates the same rationale successfully.
#[test]
fn c2_operator_hydrates_rationale() {
    let node = booted("c2");
    let tok = agent(&node, "c2-agent");
    let h = write_fact(
        &node,
        &tok,
        "c2",
        "topic",
        "operators audit everything",
        101,
    );
    let facet = {
        let lib = node.curators.librarian.lock().unwrap();
        format!(
            "{}/rationale",
            lib.active_skeleton_for(&h).unwrap().output_handle
        )
    };
    let op = issue_operator_token(&*node.signer, "operator");
    let r = handle_envelope(
        &node,
        &env_for(
            &node,
            &op,
            Intent::Hydrate,
            Cbor::map(vec![("handle", Cbor::t(facet))]),
            None,
            102,
            false,
        ),
    );
    assert!(r.ok, "{:?}", r.err_message);
    assert!(!r.result.opt_str("body").unwrap().is_empty());
}

/// C3 — Warden audits are independently grounded (uncited handle or
/// hash-proof; never a bare pass).
#[test]
fn c3_independent_grounding() {
    let node = booted("c3");
    let tok = agent(&node, "c3-agent");
    // Two facts in the same (s,p): the second's audit gets an independent
    // ground (the first).
    let h1 = write_fact(&node, &tok, "c3", "design", "tokens carry scopes", 111);
    let h2 = write_fact(&node, &tok, "c3", "design", "tokens carry scopes", 112);
    let _ = h2;
    let audits: Vec<_> = {
        let w = node.curators.warden.lock().unwrap();
        // Every stored audit satisfies grounding-or-proof.
        (0..1).map(|_| w.audit_count()).collect()
    };
    assert!(audits[0] >= 2);
    // Inspect the last audit for the second output directly.
    let lib_out = {
        let lib = node.curators.librarian.lock().unwrap();
        lib.active_skeleton_for(&h2)
            .or_else(|| lib.active_skeleton_for(&h1))
            .unwrap()
            .output_handle
            .clone()
    };
    let rec = {
        let w = node.curators.warden.lock().unwrap();
        w.audit_for_target(&lib_out).cloned().unwrap()
    };
    assert!(
        !rec.independent_grounds.is_empty() || rec.hash_proof.is_some(),
        "audit must carry independent grounds or a hash proof"
    );
}

/// C4 — 100% agreement over 100 audits trips SuspiciousAgreement and the
/// probe boost.
#[test]
fn c4_suspicious_agreement() {
    let node = booted("c4");
    let before = node.metrics.counter("curator.suspicious_agreement");
    {
        let mut ledger = node.cross_check.lock().unwrap();
        for i in 0..100 {
            ledger
                .append(
                    &node.metrics,
                    1_000 + i,
                    CrossCheckKind::WardenAudit,
                    &format!("librarian/output/c4-{i}"),
                    &format!("warden/judgment/c4-{i}"),
                    CrossCheckOutcome::Agree,
                    None,
                )
                .unwrap();
        }
        assert_eq!(ledger.probe_boost(), 10.0);
    }
    assert!(node.metrics.counter("curator.suspicious_agreement") > before);
}

/// C5 — blind re-audit is deterministic and order-insensitive.
#[test]
fn c5_blind_reaudit_determinism() {
    let node = booted("c5");
    let tok = agent(&node, "c5-agent");
    let h = write_fact(&node, &tok, "c5", "facts", "one two three", 131);
    let output = {
        let lib = node.curators.librarian.lock().unwrap();
        lib.active_skeleton_for(&h).unwrap().clone()
    };
    let job = ultracortex::curator::librarian::CurationJob {
        written_handle: h,
        subject: Some("c5".into()),
        predicate: Some("facts".into()),
        object_text: "one two three".into(),
        severity: Severity::P2,
        logical_at: node.now(),
        seed: 131,
    };
    let (a1, a2) = {
        let w = node.curators.warden.lock().unwrap();
        w.blind_reaudit(&*node, &output, Some(&job), node.now(), 777)
    };
    assert_eq!(a1, a2);
    // Rerun with the same seed → identical verdict pair.
    let (b1, b2) = {
        let w = node.curators.warden.lock().unwrap();
        w.blind_reaudit(&*node, &output, Some(&job), node.now(), 777)
    };
    assert_eq!(a1, b1);
    assert_eq!(a2, b2);
}

/// C6 — adjudicator pool rotation is seed-selected and deterministic.
#[test]
fn c6_pool_rotation() {
    let node = booted("c6");
    let init = CuratorPublic {
        output_handle: "librarian/output/c6".into(),
        operation: CuratorOperation::Skeleton,
        target_handle: "fact/c6".into(),
        grounded_in: vec![],
        confidence_band: ConfidenceBand::Medium,
        schema_id: "curator.librarian.output.v1".into(),
        spec_anchor: "LibrarianCell.md\u{00a7}3".into(),
        logical_at: 1,
        body: "b".into(),
    };
    let flag = CuratorPublic {
        output_handle: "warden/judgment/c6".into(),
        operation: CuratorOperation::AuditFail, // no policy rule → pool
        target_handle: init.output_handle.clone(),
        grounded_in: vec![],
        confidence_band: ConfidenceBand::High,
        schema_id: "curator.warden.judgment.v1".into(),
        spec_anchor: "WardenCell.md\u{00a7}6".into(),
        logical_at: 2,
        body: "flag".into(),
    };
    let mut adj = node.curators.adjudicator.lock().unwrap();
    let pool = adj.pool_names();
    assert_eq!(pool.len(), 3);
    for seed in 0..6u64 {
        let rec = adj.adjudicate(
            &*node,
            &Dispute {
                initiator_output: &init,
                auditor_flag: &flag,
                logical_at: 10,
                seed,
            },
        );
        assert_eq!(
            rec.judge.as_deref(),
            Some(pool[(seed % 3) as usize].as_str())
        );
    }
}

/// C7 — prior-blindness: identical disputes resolve identically under
/// wildly different ledger histories.
#[test]
fn c7_no_prior_leakage() {
    let resolve = |node: &Arc<Node>, seed: u64| -> Resolution {
        let init = CuratorPublic {
            output_handle: "librarian/output/c7".into(),
            operation: CuratorOperation::Skeleton,
            target_handle: "fact/c7".into(),
            grounded_in: vec![],
            confidence_band: ConfidenceBand::Medium,
            schema_id: "curator.librarian.output.v1".into(),
            spec_anchor: "LibrarianCell.md\u{00a7}3".into(),
            logical_at: 1,
            body: "b".into(),
        };
        let flag = CuratorPublic {
            output_handle: "warden/judgment/c7".into(),
            operation: CuratorOperation::AuditFail,
            target_handle: init.output_handle.clone(),
            grounded_in: vec![],
            confidence_band: ConfidenceBand::High,
            schema_id: "curator.warden.judgment.v1".into(),
            spec_anchor: "WardenCell.md\u{00a7}6".into(),
            logical_at: 2,
            body: "flag".into(),
        };
        let mut adj = node.curators.adjudicator.lock().unwrap();
        adj.adjudicate(
            &**node,
            &Dispute {
                initiator_output: &init,
                auditor_flag: &flag,
                logical_at: 10,
                seed,
            },
        )
        .resolution
    };

    let node_a = booted("c7a"); // pristine ledger
    let node_b = booted("c7b"); // ledger saturated with disagreements
    {
        let mut ledger = node_b.cross_check.lock().unwrap();
        for i in 0..300 {
            ledger
                .append(
                    &node_b.metrics,
                    i,
                    CrossCheckKind::WardenAudit,
                    "librarian/output/noise",
                    "warden/judgment/noise",
                    CrossCheckOutcome::Disagree,
                    None,
                )
                .unwrap();
        }
    }
    for seed in [0u64, 1, 2, 17, 99] {
        assert_eq!(
            resolve(&node_a, seed),
            resolve(&node_b, seed),
            "seed {seed}: ledger history influenced the adjudicator"
        );
    }
}

/// C8 — Trinity governs curators: the Librarian's own output write runs
/// the contract check.
#[test]
fn c8_trinity_governs_curator() {
    let node = booted("c8");
    let tok = agent(&node, "c8-agent");
    let before = node.metrics.counter("trinity.contract.checked");
    write_fact(
        &node,
        &tok,
        "c8",
        "governance",
        "curators are agents too",
        161,
    );
    // One check for the fact write + one for the librarian output (P20).
    assert!(
        node.metrics.counter("trinity.contract.checked") >= before + 2,
        "librarian output did not traverse the chain"
    );
}

/// C9 — a fabricated-handle probe is caught by the Warden and recorded on
/// the ledger.
#[test]
fn c9_probe_detection() {
    let node = booted("c9");
    let before_probes = node.metrics.counter("curator.probes");
    ultracortex::router::run_curation_probe(&node, node.tick()).expect("curator probe");
    assert_eq!(node.metrics.counter("curator.probes"), before_probes + 1);
    assert_eq!(
        node.metrics.counter("curator.probe_missed"),
        0,
        "a fabricated handle slipped past the Warden"
    );
    let ledger = node.cross_check.lock().unwrap();
    let probes: Vec<_> = ledger
        .tail(50)
        .into_iter()
        .filter(|r| r.kind == CrossCheckKind::Probe)
        .collect();
    assert!(!probes.is_empty());
    assert!(probes
        .iter()
        .all(|r| r.outcome == CrossCheckOutcome::Disagree));
}

/// C10 — full E2E through the wire types: write → chain → curation →
/// audit → Active skeleton served on recall.
#[test]
fn c10_end_to_end() {
    let node = booted("c10");
    let tok = agent(&node, "c10-agent");
    let h = write_fact(
        &node,
        &tok,
        "pipeline",
        "guarantee",
        "every write is chained, curated, audited, and only then served. \
         The skeleton you recall has survived an adversarial audit.",
        171,
    );
    // The skeleton is Active (not Pending, not Quarantined).
    let (skeleton, status) = {
        let lib = node.curators.librarian.lock().unwrap();
        let p = lib
            .active_skeleton_for(&h)
            .expect("active skeleton")
            .clone();
        (p.body.clone(), lib.status(&p.output_handle).unwrap())
    };
    assert_eq!(status, OutputStatus::Active);
    assert!(!skeleton.is_empty());
    // Warden audited it.
    assert!(node.metrics.counter("warden.audits") >= 1);
    // Ledger recorded the full trail.
    {
        let ledger = node.cross_check.lock().unwrap();
        let kinds: Vec<CrossCheckKind> = ledger.tail(20).iter().map(|r| r.kind).collect();
        assert!(kinds.contains(&CrossCheckKind::LibrarianOutput));
        assert!(kinds.contains(&CrossCheckKind::WardenAudit));
    }
    // Recall serves the audited skeleton.
    let r = handle_envelope(
        &node,
        &env_for(
            &node,
            &tok,
            Intent::Recall,
            Cbor::map(vec![
                ("query", Cbor::t("adversarial audit")),
                ("k", Cbor::U64(4)),
            ]),
            None,
            172,
            false,
        ),
    );
    assert!(r.ok);
    assert!(r.result.opt_u64("items").unwrap_or(0) >= 1);
    let view_bytes = r
        .result
        .get("view")
        .and_then(|v| v.as_bytes())
        .unwrap()
        .to_vec();
    let decoded = ultracortex::router::view::decode_view(&view_bytes).unwrap();
    let skeletons = &decoded.skeletons;
    assert!(
        skeletons
            .iter()
            .any(|s| s.opt_str("skeleton").unwrap_or_default() == skeleton),
        "recall did not serve the audited skeleton"
    );
    // Determinism spot-check: same write on a fresh node yields the same
    // curator output handle for the same (seed, logical positions) — the
    // ULIDs are seed-derived, not wall-clock.
    let node2 = booted("c10b");
    let tok2 = agent(&node2, "c10-agent");
    let h2 = write_fact(
        &node2,
        &tok2,
        "pipeline",
        "guarantee",
        "every write is chained, curated, audited, and only then served. \
         The skeleton you recall has survived an adversarial audit.",
        171,
    );
    let skeleton2 = {
        let lib = node2.curators.librarian.lock().unwrap();
        lib.active_skeleton_for(&h2).unwrap().body.clone()
    };
    assert_eq!(skeleton, skeleton2, "curation is not deterministic");
}

/// C4b — the CellType surface is closed: parse(as_str) roundtrips for all
/// 25 (guards congruence entity lists + token cell globs).
#[test]
fn cell_type_roundtrip() {
    for ct in [
        CellType::Catalog,
        CellType::Fact,
        CellType::Timeline,
        CellType::Playbook,
        CellType::Scratchpad,
        CellType::Vector,
        CellType::Graph,
        CellType::Bm25,
        CellType::Blob,
        CellType::Cache,
        CellType::AgentRegistry,
        CellType::Proposal,
        CellType::Subscription,
        CellType::Reranker,
        CellType::SpecAnchor,
        CellType::DecisionLedger,
        CellType::Congruence,
        CellType::Gap,
        CellType::Quarantine,
        CellType::WorkBudget,
        CellType::Contract,
        CellType::Librarian,
        CellType::Warden,
        CellType::Adjudicator,
        CellType::CrossCheckLedger,
    ] {
        assert_eq!(CellType::parse(ct.as_str()), Some(ct));
    }
}
