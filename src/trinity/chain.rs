//! Pre-validation chain — SPEC-DERIVED-§B (RouterScheduler.md),
//! SPEC-DERIVED-§4 (NATIVE_TRINITY.md).
//!
//! Fixed order, no reordering, no skipping (except the anchor exemption for
//! Scratchpad/Cache targets, which lives inside step 2 itself):
//!
//! ```text
//! 1. Contract.validate_schema
//! 2. SpecAnchor.validate
//! 3. DecisionLedger.check_conflicts
//! 4. WorkBudget.charge_pre
//! 5. Congruence.preview_delta
//! 6. (optional) Warden.judge — wired by the Router when
//!    flags.semantic_check is set or severity == P0
//! ```
//!
//! Any failure is absorbed into the QuarantineCell (never a silent drop),
//! and the returned [`UcError`] carries the quarantine id plus the failing
//! step in its cause chain. A budget reservation made in step 4 is released
//! if step 5 subsequently fails — reservations must not leak
//! (NATIVE_TRINITY.md §8.4).

use super::cells::{
    CongruenceCell, ContractCell, DecisionLedgerCell, GapCell, QuarantineCell, SpecAnchorCell,
    WorkBudgetCell,
};
use crate::cells::CellType;
use crate::core::cbor::Cbor;
use crate::core::{AnchorRef, Severity, UcError, UcResult};
use crate::obs::Metrics;

/// All seven Trinity cells, owned together. The Router holds this behind a
/// single lock: the chain is a serialization point by design — governance
/// checks observe a consistent snapshot of governance state.
pub struct Trinity {
    pub contract: ContractCell,
    pub spec_anchor: SpecAnchorCell,
    pub decision_ledger: DecisionLedgerCell,
    pub work_budget: WorkBudgetCell,
    pub congruence: CongruenceCell,
    pub quarantine: QuarantineCell,
    pub gap: GapCell,
}

/// What the chain needs to know about an envelope. The Router constructs
/// this from the wire [`crate::router::envelope::Envelope`]; keeping the
/// chain decoupled from the wire type lets conformance tests drive it
/// directly.
pub struct PreCtx<'a> {
    pub logical_at: u64,
    pub seed: u64,
    pub task_id: String,
    pub schema_id: &'a str,
    pub target_type: CellType,
    pub spec_anchor: Option<AnchorRef>,
    pub severity: Severity,
    pub payload: &'a Cbor,
    /// Estimated work units for this dispatch (router derives from payload
    /// size + intent; see RouterScheduler.md §C.2).
    pub estimate: u64,
}

/// Outcome of a successful chain run: the budget reservation to reconcile
/// post-dispatch via `WorkBudget::charge_post`.
#[derive(Debug)]
pub struct PreOk {
    pub reserved: u64,
}

pub fn run_pre_validation(
    trinity: &mut Trinity,
    metrics: &Metrics,
    ctx: &PreCtx<'_>,
) -> UcResult<PreOk> {
    // Step 1 — Contract.
    metrics.inc("trinity.contract.checked");
    if let Err(e) = trinity.contract.validate_schema(ctx.schema_id, ctx.payload) {
        metrics.inc("trinity.contract.rejected");
        return Err(absorb(trinity, metrics, ctx, "contract", e));
    }

    // Step 2 — SpecAnchor.
    metrics.inc("trinity.spec_anchor.checked");
    if let Err(e) = trinity
        .spec_anchor
        .validate(ctx.target_type, ctx.spec_anchor.as_ref())
    {
        metrics.inc("trinity.spec_anchor.rejected");
        return Err(absorb(trinity, metrics, ctx, "spec_anchor", e));
    }

    // Step 3 — DecisionLedger.
    metrics.inc("trinity.decision.checked");
    if let Err(e) = trinity.decision_ledger.check_conflicts(ctx.payload) {
        metrics.inc("trinity.decision.rejected");
        return Err(absorb(trinity, metrics, ctx, "decision_ledger", e));
    }

    // Step 4 — WorkBudget (reserve).
    metrics.inc("trinity.budget.checked");
    trinity.work_budget.ensure(&ctx.task_id, None);
    if let Err(e) = trinity.work_budget.charge_pre(&ctx.task_id, ctx.estimate) {
        metrics.inc("trinity.budget.rejected");
        return Err(absorb(trinity, metrics, ctx, "work_budget", e));
    }

    // Step 5 — Congruence. On failure the step-4 reservation is released
    // before quarantining, so failed writes don't leak budget.
    metrics.inc("trinity.congruence.checked");
    if let Err(e) = trinity.congruence.preview_delta(ctx.payload) {
        metrics.inc("trinity.congruence.rejected");
        trinity.work_budget.charge_post(&ctx.task_id, ctx.estimate, 0);
        return Err(absorb(trinity, metrics, ctx, "congruence", e));
    }

    // Step 6 (Warden) is dispatched by the Router — it needs curator state
    // that deliberately lives outside the Trinity lock (P20: the substrate
    // polices curators; curators do not sit inside the governance lock).
    Ok(PreOk {
        reserved: ctx.estimate,
    })
}

/// Absorb a chain failure into quarantine and decorate the error. The
/// original envelope payload is preserved verbatim so `quarantine reinject`
/// can replay it after the cause is fixed.
fn absorb(
    trinity: &mut Trinity,
    metrics: &Metrics,
    ctx: &PreCtx<'_>,
    step: &str,
    err: UcError,
) -> UcError {
    metrics.inc("quarantine.absorbed");
    metrics.inc(&format!("quarantine.absorbed.{step}"));
    let record = Cbor::map(vec![
        ("step", Cbor::t(step)),
        ("schema_id", Cbor::t(ctx.schema_id)),
        ("target_type", Cbor::t(ctx.target_type.as_str())),
        ("task_id", Cbor::t(ctx.task_id.clone())),
        ("severity", Cbor::t(ctx.severity.as_str())),
        ("payload", ctx.payload.clone()),
        (
            "spec_anchor",
            ctx.spec_anchor
                .as_ref()
                .map(|a| Cbor::t(a.key()))
                .unwrap_or(Cbor::Null),
        ),
    ]);
    let qid = trinity
        .quarantine
        .absorb(ctx.logical_at, ctx.seed, err.code, &err.message, record);
    metrics.gauge_set(
        "quarantine.pending",
        trinity.quarantine.pending_count() as i64,
    );
    let code = err.code;
    // The quarantine id is threaded as a ULID when parseable; otherwise the
    // string form travels in the message (qid is always `quarantine/<ulid>`).
    let mut out = err.with_cause(code);
    if let Some(u) = qid
        .strip_prefix("quarantine/")
        .and_then(crate::core::ulid::Ulid::from_base32)
    {
        out = out.with_quarantine(u);
    }
    out.message = format!("{} [absorbed as {}]", out.message, qid);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{CellId, ErrCode};

    fn fresh() -> Trinity {
        let mut t = Trinity {
            contract: ContractCell::new(CellId(20)),
            spec_anchor: SpecAnchorCell::new(CellId(21)),
            decision_ledger: DecisionLedgerCell::new(CellId(22)),
            work_budget: WorkBudgetCell::new(CellId(23)),
            congruence: CongruenceCell::new(CellId(24)),
            quarantine: QuarantineCell::new(CellId(26)),
            gap: GapCell::new(CellId(25)),
        };
        t.spec_anchor.register("Architecture.md", "4");
        t.contract
            .register(0, "fact.v1", vec!["subject".into(), "predicate".into(), "object".into()]);
        t
    }

    fn ctx<'a>(payload: &'a Cbor, anchor: Option<AnchorRef>) -> PreCtx<'a> {
        PreCtx {
            logical_at: 10,
            seed: 42,
            task_id: "task-A".into(),
            schema_id: "fact.v1",
            target_type: CellType::Fact,
            spec_anchor: anchor,
            severity: Severity::P1,
            payload,
            estimate: 10,
        }
    }

    #[test]
    fn happy_path_reserves_budget() {
        let mut t = fresh();
        let m = Metrics::new();
        let payload = Cbor::map(vec![
            ("subject", Cbor::t("svc")),
            ("predicate", Cbor::t("owner")),
            ("object", Cbor::t("team")),
        ]);
        let pre = run_pre_validation(
            &mut t,
            &m,
            &ctx(&payload, Some(AnchorRef::new("Architecture.md", "4"))),
        )
        .unwrap();
        assert_eq!(pre.reserved, 10);
        assert_eq!(t.work_budget.get("task-A").unwrap().reserved, 10);
        assert_eq!(t.quarantine.pending_count(), 0);
    }

    #[test]
    fn each_step_failure_lands_in_quarantine() {
        // Contract failure (missing field).
        let mut t = fresh();
        let m = Metrics::new();
        let bad_contract = Cbor::map(vec![("subject", Cbor::t("only"))]);
        let e = run_pre_validation(
            &mut t,
            &m,
            &ctx(&bad_contract, Some(AnchorRef::new("Architecture.md", "4"))),
        )
        .unwrap_err();
        assert_eq!(e.code, ErrCode::ContractViolation);
        assert!(e.quarantine_id.is_some());
        assert_eq!(t.quarantine.pending_count(), 1);
        assert_eq!(m.counter("quarantine.absorbed.contract"), 1);

        // Anchor failure.
        let good_payload = Cbor::map(vec![
            ("subject", Cbor::t("s")),
            ("predicate", Cbor::t("p")),
            ("object", Cbor::t("o")),
        ]);
        let e = run_pre_validation(&mut t, &m, &ctx(&good_payload, None)).unwrap_err();
        assert_eq!(e.code, ErrCode::AnchorMissing);
        assert_eq!(t.quarantine.pending_count(), 2);

        // Congruence failure releases the step-4 reservation.
        let drifty = Cbor::map(vec![
            ("subject", Cbor::t("s")),
            ("predicate", Cbor::t("p")),
            ("object", Cbor::t("adopt the TelepathyCell")),
        ]);
        let e = run_pre_validation(
            &mut t,
            &m,
            &ctx(&drifty, Some(AnchorRef::new("Architecture.md", "4"))),
        )
        .unwrap_err();
        assert_eq!(e.code, ErrCode::CongruenceDelta);
        // No leaked reservation.
        assert_eq!(t.work_budget.get("task-A").unwrap().reserved, 0);
        assert_eq!(t.quarantine.pending_count(), 3);
    }

    #[test]
    fn budget_exhaustion_blocks_before_congruence() {
        let mut t = fresh();
        let m = Metrics::new();
        t.work_budget.ensure("task-B", Some(5));
        let payload = Cbor::map(vec![
            ("subject", Cbor::t("s")),
            ("predicate", Cbor::t("p")),
            ("object", Cbor::t("o")),
        ]);
        let mut c = ctx(&payload, Some(AnchorRef::new("Architecture.md", "4")));
        c.task_id = "task-B".into();
        c.estimate = 10;
        let e = run_pre_validation(&mut t, &m, &c).unwrap_err();
        assert_eq!(e.code, ErrCode::BudgetExceeded);
        assert_eq!(m.counter("trinity.congruence.checked"), 0);
    }

    #[test]
    fn quarantined_payload_is_reinjectable() {
        let mut t = fresh();
        let m = Metrics::new();
        let payload = Cbor::map(vec![
            ("subject", Cbor::t("s")),
            ("predicate", Cbor::t("p")),
            ("object", Cbor::t("o")),
        ]);
        let _ = run_pre_validation(&mut t, &m, &ctx(&payload, None)).unwrap_err();
        let qid = t.quarantine.pending()[0].qid.clone();
        let stored = t.quarantine.reinject(99, &qid).unwrap();
        // The absorbed record preserves the payload verbatim.
        assert_eq!(stored.get("payload"), Some(&payload));
        assert_eq!(stored.opt_str("step").as_deref(), Some("spec_anchor"));
    }
}
