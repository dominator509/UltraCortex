//! Prefix-stable views — SPEC-DERIVED-§5 (RouterScheduler.md),
//! SPEC-DERIVED-§3 (DeepSeekOptimization.md).
//!
//! A view is a deterministic render of substrate state whose byte layout is
//! **prefix-stable**: the render is a CBOR *array* (order-preserving, unlike
//! maps) of five segments — header, handles, skeletons, bodies, footer —
//! each internally lexicographically sorted. Adding content only ever
//! appends within a segment or grows later segments, so a prefix-caching
//! model (DeepSeek et al.) keeps its KV cache warm across re-renders.
//!
//! Tier policy (RouterScheduler.md §5.1):
//!
//! | tier | budget            | contents                       |
//! |------|-------------------|--------------------------------|
//! | L0   | ≤ 500 tokens      | header + handles               |
//! | L1   | ≤ 1.5 KiB         | + skeletons                    |
//! | L2   | ≤ 4 KiB           | + short bodies                 |
//! | L3   | unbounded         | + full bodies                  |
//!
//! Renders that exceed the tier budget truncate at a segment-item boundary
//! (rule R1) and set `next_tier_hint` so the caller can escalate.

use crate::core::cbor::Cbor;
use crate::core::crypto::{hex, sha256};
use crate::core::{est_tokens, Tier};

/// Cache key: (view_id, namespace, version, params_hash) — matches
/// PrefixCacheStore::ViewKey (PersistenceLayer.md §6.2).
pub fn params_hash(params: &Cbor) -> String {
    hex(&sha256(&params.encode()))[..16].to_string()
}

#[derive(Clone, Debug)]
pub struct ViewItem {
    pub handle: String,
    pub skeleton: String,
    pub body: String,
}

#[derive(Clone, Debug)]
pub struct RenderedView {
    pub bytes: Vec<u8>,
    pub tokens_emitted: u64,
    pub truncated: bool,
    pub next_tier_hint: Option<Tier>,
    pub items_included: usize,
}

/// Tier byte budgets. L0's budget is expressed in tokens (≤500) which at
/// the est_tokens ratio (§C.2: bytes/4) is 2000 bytes.
fn tier_byte_budget(tier: Tier) -> Option<usize> {
    match tier {
        Tier::L0 => Some(500 * 4),
        Tier::L1 => Some(1536),
        Tier::L2 => Some(4096),
        Tier::L3 => None,
    }
}

/// Render a view. `items` must arrive pre-sorted by handle (callers sort;
/// this function asserts the invariant in debug builds).
pub fn render_view(
    view_id: &str,
    namespace: &str,
    version: u64,
    params: &Cbor,
    items: &[ViewItem],
    tier: Tier,
) -> RenderedView {
    debug_assert!(
        items.windows(2).all(|w| w[0].handle <= w[1].handle),
        "view items must be sorted by handle"
    );

    let header = Cbor::map(vec![
        ("view_id", Cbor::t(view_id)),
        ("namespace", Cbor::t(namespace)),
        ("version", Cbor::U64(version)),
        ("params_hash", Cbor::t(params_hash(params))),
        ("tier", Cbor::t(tier.as_str())),
    ]);

    let budget = tier_byte_budget(tier);
    let mut truncated = false;
    let mut included = 0usize;

    // Build segments incrementally, checking the budget after each item.
    // The header + footer skeleton cost is charged up front.
    let mut handles: Vec<Cbor> = Vec::new();
    let mut skeletons: Vec<Cbor> = Vec::new();
    let mut bodies: Vec<Cbor> = Vec::new();

    let assemble = |handles: &[Cbor], skeletons: &[Cbor], bodies: &[Cbor], truncated: bool| {
        let footer = Cbor::map(vec![
            ("items", Cbor::U64(handles.len() as u64)),
            ("truncated", Cbor::Bool(truncated)),
        ]);
        Cbor::Array(vec![
            header.clone(),
            Cbor::Array(handles.to_vec()),
            Cbor::Array(skeletons.to_vec()),
            Cbor::Array(bodies.to_vec()),
            footer,
        ])
        .encode()
    };

    for item in items {
        // Stage the addition per tier rules.
        handles.push(Cbor::t(item.handle.clone()));
        if tier >= Tier::L1 {
            skeletons.push(Cbor::map(vec![
                ("handle", Cbor::t(item.handle.clone())),
                ("skeleton", Cbor::t(item.skeleton.clone())),
            ]));
        }
        if tier >= Tier::L2 {
            let body = if tier == Tier::L2 && item.body.len() > 512 {
                // Char-boundary-safe truncation for L2 short bodies.
                let mut cut = 512;
                while !item.body.is_char_boundary(cut) {
                    cut -= 1;
                }
                format!("{}…", &item.body[..cut])
            } else {
                item.body.clone()
            };
            bodies.push(Cbor::map(vec![
                ("handle", Cbor::t(item.handle.clone())),
                ("body", Cbor::t(body)),
            ]));
        }

        if let Some(max) = budget {
            let size = assemble(&handles, &skeletons, &bodies, false).len();
            if size > max && included > 0 {
                // R1: roll back the item that burst the budget; truncate at
                // the item boundary.
                handles.pop();
                if tier >= Tier::L1 {
                    skeletons.pop();
                }
                if tier >= Tier::L2 {
                    bodies.pop();
                }
                truncated = true;
                break;
            }
            if size > max {
                // Even a single item exceeds the tier: emit it truncated=true
                // with a hint; the caller must escalate.
                truncated = true;
                break;
            }
        }
        included += 1;
    }

    if included < items.len() {
        truncated = true;
    }

    let bytes = assemble(&handles, &skeletons, &bodies, truncated);
    RenderedView {
        tokens_emitted: est_tokens(bytes.len()) as u64,
        truncated,
        next_tier_hint: if truncated { tier.next() } else { None },
        items_included: included.min(handles.len()),
        bytes,
    }
}

/// Built-in view ids the Router serves without registration
/// (RouterScheduler.md §5.3).
pub const BUILTIN_VIEWS: [&str; 4] = [
    "fact_subject",   // all active facts for params.subject
    "timeline_tail",  // last params.n entries of params.stream
    "gap_board",      // open gaps with dispatch counters
    "quarantine_log", // pending quarantine items
];

#[cfg(test)]
mod tests {
    use super::*;

    fn items(n: usize, body_len: usize) -> Vec<ViewItem> {
        (0..n)
            .map(|i| ViewItem {
                handle: format!("fact/{i:04}"),
                skeleton: format!("skeleton of item {i}"),
                body: "x".repeat(body_len),
            })
            .collect()
    }

    #[test]
    fn prefix_stability_across_growth() {
        let params = Cbor::map(vec![("subject", Cbor::t("svc"))]);
        let five = items(5, 40);
        let seven = items(7, 40);
        let r5 = render_view("fact_subject", "default", 1, &params, &five, Tier::L3);
        let r7 = render_view("fact_subject", "default", 1, &params, &seven, Tier::L3);
        // Growth must not disturb the header segment bytes: the header is
        // the first array element and identical for both renders.
        let header_len = {
            let header = Cbor::map(vec![
                ("view_id", Cbor::t("fact_subject")),
                ("namespace", Cbor::t("default")),
                ("version", Cbor::U64(1)),
                ("params_hash", Cbor::t(params_hash(&params))),
                ("tier", Cbor::t("L3")),
            ])
            .encode();
            header.len()
        };
        // Skip the outer array tag (1 byte for a 5-element array).
        assert_eq!(&r5.bytes[1..1 + header_len], &r7.bytes[1..1 + header_len]);
        // And the shared handles prefix is byte-identical.
        let shared = 40; // comfortably within the first items
        assert_eq!(
            &r5.bytes[1 + header_len..1 + header_len + shared],
            &r7.bytes[1 + header_len..1 + header_len + shared]
        );
    }

    #[test]
    fn deterministic_render() {
        let params = Cbor::map(vec![("q", Cbor::t("x"))]);
        let it = items(3, 100);
        let a = render_view("v", "ns", 2, &params, &it, Tier::L2);
        let b = render_view("v", "ns", 2, &params, &it, Tier::L2);
        assert_eq!(a.bytes, b.bytes);
        assert_eq!(a.tokens_emitted, b.tokens_emitted);
    }

    #[test]
    fn tier_budgets_and_truncation() {
        let params = Cbor::Null;
        let many = items(200, 120);
        let l0 = render_view("v", "ns", 1, &params, &many, Tier::L0);
        assert!(l0.bytes.len() <= 2000 + 64); // small footer slack
        assert!(l0.truncated);
        assert_eq!(l0.next_tier_hint, Some(Tier::L1));
        assert!(l0.items_included > 0);

        let l1 = render_view("v", "ns", 1, &params, &many, Tier::L1);
        assert!(l1.bytes.len() <= 1536 + 64);
        assert!(l1.truncated);
        assert_eq!(l1.next_tier_hint, Some(Tier::L2));

        // L3 is unbounded: everything included, no hint.
        let l3 = render_view("v", "ns", 1, &params, &many, Tier::L3);
        assert!(!l3.truncated);
        assert_eq!(l3.next_tier_hint, None);
        assert_eq!(l3.items_included, 200);
    }

    #[test]
    fn l2_shortens_long_bodies() {
        let params = Cbor::Null;
        let it = items(1, 5000);
        let l2 = render_view("v", "ns", 1, &params, &it, Tier::L2);
        let decoded = Cbor::decode(&l2.bytes).unwrap();
        let bodies = decoded.as_array().unwrap()[3].as_array().unwrap();
        let body = bodies[0].req_str("body").unwrap();
        assert!(body.len() < 600);
        assert!(body.ends_with('…'));
        // L3 keeps it whole.
        let l3 = render_view("v", "ns", 1, &params, &it, Tier::L3);
        let decoded = Cbor::decode(&l3.bytes).unwrap();
        let bodies = decoded.as_array().unwrap()[3].as_array().unwrap();
        assert_eq!(bodies[0].req_str("body").unwrap().len(), 5000);
    }

    #[test]
    fn params_hash_stability() {
        let a = Cbor::map(vec![("b", Cbor::U64(2)), ("a", Cbor::U64(1))]);
        let b = Cbor::map(vec![("a", Cbor::U64(1)), ("b", Cbor::U64(2))]);
        // Canonical encoding makes key order irrelevant.
        assert_eq!(params_hash(&a), params_hash(&b));
        assert_eq!(params_hash(&a).len(), 16);
    }
}
