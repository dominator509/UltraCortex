//! End-to-end DeepSeek formatting behavior on the live `view` path.

use std::sync::Arc;

use ultracortex::bootstrap;
use ultracortex::core::cbor::Cbor;
use ultracortex::core::ulid::{DetRng, Ulid};
use ultracortex::core::{Intent, Severity, Tier};
use ultracortex::router::captoken::issue_agent_token;
use ultracortex::router::envelope::{Envelope, EnvelopeFlags, WorkBudget, PROTO_VERSION};
use ultracortex::router::handle_envelope;
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
    payload: Cbor,
    seed: u64,
) -> Envelope {
    Envelope {
        proto_version: PROTO_VERSION,
        request_id: Ulid::from_parts(node.now(), &mut DetRng::new(seed ^ 0xD502)),
        agent_id: token.agent_id.clone(),
        capability: token.clone(),
        work_budget: WorkBudget {
            task_id: "view.formatting".into(),
            units: 20_000,
        },
        intent: Intent::View,
        payload,
        spec_anchor: None,
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

#[test]
fn deepseek_coder_uses_real_fim_tags() {
    let node = booted();
    let tok = agent(&node, "deepseek-coder-agent");
    let r = handle_envelope(
        &node,
        &env_for(
            &node,
            &tok,
            Cbor::map(vec![
                ("view_id", Cbor::t("fact_subject")),
                (
                    "params",
                    Cbor::map(vec![("subject", Cbor::t("repo.router"))]),
                ),
                ("formatting", Cbor::t("deepseek_fim")),
                ("client_kind", Cbor::t("deepseek-coder")),
                ("prefix", Cbor::t("fn main() {\n")),
                ("suffix", Cbor::t("}\n")),
            ]),
            41,
        ),
    );
    assert!(r.ok, "{:?}", r.err_message);
    let bytes = r.result.get("view").and_then(|v| v.as_bytes()).unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert_eq!(text, "<|fim_begin|>fn main() {\n<|fim_hole|>}\n<|fim_end|>");
}

#[test]
fn non_coder_variants_downgrade_to_plain_splice() {
    let node = booted();
    let tok = agent(&node, "deepseek-v3-agent");
    for (seed, client_kind) in [(51_u64, "deepseek-v3"), (52_u64, "deepseek-r1")] {
        let r = handle_envelope(
            &node,
            &env_for(
                &node,
                &tok,
                Cbor::map(vec![
                    ("view_id", Cbor::t("fact_subject")),
                    (
                        "params",
                        Cbor::map(vec![("subject", Cbor::t("repo.router"))]),
                    ),
                    ("formatting", Cbor::t("deepseek_fim")),
                    ("client_kind", Cbor::t(client_kind)),
                    ("prefix", Cbor::t("fn main() {\n")),
                    ("suffix", Cbor::t("}\n")),
                ]),
                seed,
            ),
        );
        assert!(r.ok, "{:?}", r.err_message);
        let bytes = r.result.get("view").and_then(|v| v.as_bytes()).unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert_eq!(text, "fn main() {\n}\n");
    }
}
