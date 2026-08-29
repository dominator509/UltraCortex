//! End-to-end view-version migration tests for the live Router.

use std::sync::Arc;

use ultracortex::bootstrap;
use ultracortex::core::cbor::Cbor;
use ultracortex::core::ulid::{DetRng, Ulid};
use ultracortex::core::{ErrCode, Intent, Severity, Tier};
use ultracortex::router::captoken::issue_agent_token;
use ultracortex::router::envelope::{Envelope, EnvelopeFlags, WorkBudget, PROTO_VERSION};
use ultracortex::router::handle_envelope;
use ultracortex::router::view::decode_view;
use ultracortex::Node;

fn booted() -> Arc<Node> {
    bootstrap::dry_run().expect("boot").node
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

fn env_for(
    node: &Node,
    token: &ultracortex::router::captoken::CapToken,
    intent: Intent,
    payload: Cbor,
    anchor: Option<&str>,
    seed: u64,
) -> Envelope {
    Envelope {
        proto_version: PROTO_VERSION,
        request_id: Ulid::from_parts(node.now(), &mut DetRng::new(seed ^ 0xDD04)),
        agent_id: token.agent_id.clone(),
        capability: token.clone(),
        work_budget: WorkBudget {
            task_id: "view.versioning".into(),
            units: 50_000,
        },
        intent,
        payload,
        spec_anchor: anchor.map(String::from),
        severity: Severity::P2,
        gap_ref: None,
        tier: Tier::L1,
        seed,
        flags: EnvelopeFlags {
            semantic_check: false,
            continuation: false,
        },
    }
}

fn write_fact(
    node: &Node,
    tok: &ultracortex::router::captoken::CapToken,
    subject: &str,
    predicate: &str,
    object: &str,
    seed: u64,
) {
    let r = handle_envelope(
        node,
        &env_for(
            node,
            tok,
            Intent::Write,
            Cbor::map(vec![
                ("subject", Cbor::t(subject)),
                ("predicate", Cbor::t(predicate)),
                ("object", Cbor::t(object)),
            ]),
            Some("Architecture.md\u{00a7}4"),
            seed,
        ),
    );
    assert!(r.ok, "write failed: {:?}", r.err_message);
}

#[test]
fn stale_versions_reject_without_migration_and_upgrade_with_opt_in() {
    let node = booted();
    let tok = agent(&node, "view-version-agent");

    write_fact(
        &node,
        &tok,
        "repo.view",
        "note",
        "Initial view state for compatibility testing.",
        11,
    );

    let current = *node.view_version.lock().unwrap();
    let initial = handle_envelope(
        &node,
        &env_for(
            &node,
            &tok,
            Intent::View,
            Cbor::map(vec![
                ("view_id", Cbor::t("fact_subject")),
                ("params", Cbor::map(vec![("subject", Cbor::t("repo.view"))])),
                ("view_version", Cbor::U64(current)),
            ]),
            None,
            12,
        ),
    );
    assert!(initial.ok);
    assert_eq!(initial.result.opt_u64("view_version"), Some(current));
    assert!(initial.result.get("view_key").is_some());
    assert!(initial.result.get("migrated_from").is_some());

    write_fact(
        &node,
        &tok,
        "repo.view",
        "note",
        "A later write bumps the global view version.",
        13,
    );
    let bumped = *node.view_version.lock().unwrap();
    assert!(bumped > current);

    let stale = handle_envelope(
        &node,
        &env_for(
            &node,
            &tok,
            Intent::View,
            Cbor::map(vec![
                ("view_id", Cbor::t("fact_subject")),
                ("params", Cbor::map(vec![("subject", Cbor::t("repo.view"))])),
                ("view_version", Cbor::U64(current)),
            ]),
            None,
            14,
        ),
    );
    assert!(!stale.ok);
    assert_eq!(stale.err_code, Some(ErrCode::ContractViolation));

    let migrated = handle_envelope(
        &node,
        &env_for(
            &node,
            &tok,
            Intent::View,
            Cbor::map(vec![
                ("view_id", Cbor::t("fact_subject")),
                ("params", Cbor::map(vec![("subject", Cbor::t("repo.view"))])),
                ("view_version", Cbor::U64(current)),
                ("allow_migrate", Cbor::Bool(true)),
            ]),
            None,
            15,
        ),
    );
    assert!(migrated.ok, "{:?}", migrated.err_message);
    assert_eq!(migrated.result.opt_u64("view_version"), Some(bumped));
    assert_eq!(migrated.result.opt_u64("migrated_from"), Some(current));
    assert_eq!(migrated.result.opt_bool("cached"), Some(false));
    let bytes = migrated
        .result
        .get("view")
        .and_then(|v| v.as_bytes())
        .unwrap();
    let decoded = decode_view(bytes).unwrap();
    assert_eq!(decoded.header.opt_u64("version"), Some(bumped));

    let migrated_cached = handle_envelope(
        &node,
        &env_for(
            &node,
            &tok,
            Intent::View,
            Cbor::map(vec![
                ("view_id", Cbor::t("fact_subject")),
                ("params", Cbor::map(vec![("subject", Cbor::t("repo.view"))])),
                ("view_version", Cbor::U64(current)),
                ("allow_migrate", Cbor::Bool(true)),
            ]),
            None,
            16,
        ),
    );
    assert!(migrated_cached.ok);
    assert_eq!(migrated_cached.result.opt_bool("cached"), Some(true));
    assert_eq!(migrated_cached.result.opt_u64("view_version"), Some(bumped));
    assert_eq!(
        migrated_cached.result.opt_u64("migrated_from"),
        Some(current)
    );
}

#[test]
fn future_view_versions_are_rejected() {
    let node = booted();
    let tok = agent(&node, "view-version-future-agent");
    let current = *node.view_version.lock().unwrap();
    let r = handle_envelope(
        &node,
        &env_for(
            &node,
            &tok,
            Intent::View,
            Cbor::map(vec![
                ("view_id", Cbor::t("gap_board")),
                ("params", Cbor::Null),
                ("view_version", Cbor::U64(current + 1)),
            ]),
            None,
            21,
        ),
    );
    assert!(!r.ok);
    assert_eq!(r.err_code, Some(ErrCode::ContractViolation));
}
