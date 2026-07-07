//! DeepSeek client optimizations — SPEC-DERIVED-§2–§5 (DeepSeekOptimization.md).
//!
//! UltraCortex views are laid out prefix-stable specifically so DeepSeek
//! (and any prefix-caching model) keeps its KV cache warm across
//! re-renders. This module holds the three client-side adapters:
//!
//! - [`fim_wrap`]: Fill-In-the-Middle framing for coder models
//!   (`<|fim_begin|>prefix<|fim_hole|>suffix<|fim_end|>`).
//! - [`r1_strip`]: split a DeepSeek-R1 response into (answer, reasoning) by
//!   removing `<think>…</think>` blocks — reasoning is PRIVATE-facet
//!   material and must never enter PUBLIC curator output.
//! - [`tools_manifest`]: the MCP verb manifest in the shape R1 tool-calling
//!   prefers — lowercase verbs, single-level argument objects.

use crate::core::cbor::Cbor;

pub fn fim_wrap(prefix: &str, suffix: &str) -> String {
    format!("<|fim_begin|>{prefix}<|fim_hole|>{suffix}<|fim_end|>")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeepSeekClientKind {
    Coder,
    V3,
    R1,
    Other,
}

impl DeepSeekClientKind {
    pub fn parse(s: &str) -> DeepSeekClientKind {
        match s {
            "deepseek-coder" => DeepSeekClientKind::Coder,
            "deepseek-v3" => DeepSeekClientKind::V3,
            "deepseek-r1" => DeepSeekClientKind::R1,
            _ => DeepSeekClientKind::Other,
        }
    }
}

/// Variant-aware edit framing. DeepSeek-Coder gets real FIM tags; current
/// non-Coder variants downgrade to a plain prefix+suffix splice so callers
/// never accidentally ship coder-only tokens to models that do not support
/// them.
pub fn frame_edit_prompt(kind: DeepSeekClientKind, prefix: &str, suffix: &str) -> String {
    match kind {
        DeepSeekClientKind::Coder => fim_wrap(prefix, suffix),
        DeepSeekClientKind::V3 | DeepSeekClientKind::R1 | DeepSeekClientKind::Other => {
            format!("{prefix}{suffix}")
        }
    }
}

/// Remove `<think>…</think>` blocks. Returns (stripped_answer, reasoning).
/// Unclosed blocks strip to end-of-text (defensive: truncated generations).
pub fn r1_strip(text: &str) -> (String, String) {
    let mut answer = String::with_capacity(text.len());
    let mut reasoning = String::new();
    let mut rest = text;
    loop {
        match rest.find("<think>") {
            None => {
                answer.push_str(rest);
                break;
            }
            Some(start) => {
                answer.push_str(&rest[..start]);
                let after = &rest[start + "<think>".len()..];
                match after.find("</think>") {
                    Some(end) => {
                        reasoning.push_str(&after[..end]);
                        rest = &after[end + "</think>".len()..];
                    }
                    None => {
                        reasoning.push_str(after);
                        break;
                    }
                }
            }
        }
    }
    (answer.trim().to_string(), reasoning.trim().to_string())
}

/// MCP verb manifest for tool-calling models: lowercase verbs, flat args
/// (DeepSeekOptimization.md §5: nested argument objects degrade R1
/// tool-call reliability).
pub fn tools_manifest() -> Cbor {
    let verb = |name: &str, desc: &str, args: Vec<(&str, &str)>| {
        Cbor::map(vec![
            ("name", Cbor::t(name)),
            ("description", Cbor::t(desc)),
            (
                "args",
                Cbor::map(args.into_iter().map(|(k, v)| (k, Cbor::t(v))).collect()),
            ),
        ])
    };
    Cbor::arr(vec![
        verb(
            "recall",
            "retrieve skeletons for a query at a token tier",
            vec![("query", "string"), ("tier", "L0|L1|L2|L3"), ("k", "int")],
        ),
        verb(
            "hydrate",
            "fetch the full body behind a handle",
            vec![("handle", "string")],
        ),
        verb(
            "write",
            "append a fact or blob (requires spec_anchor and work_budget)",
            vec![
                ("subject", "string"),
                ("predicate", "string"),
                ("object", "string"),
                ("spec_anchor", "string"),
            ],
        ),
        verb(
            "subscribe",
            "register for events matching a glob pattern",
            vec![("pattern", "string")],
        ),
        verb(
            "view",
            "render a prefix-stable view",
            vec![("view_id", "string"), ("params", "string")],
        ),
        verb(
            "supersede",
            "replace an active fact or decision",
            vec![("old", "string"), ("new", "string")],
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fim_frame_shape() {
        assert_eq!(
            fim_wrap("fn a() {", "}"),
            "<|fim_begin|>fn a() {<|fim_hole|>}<|fim_end|>"
        );
    }

    #[test]
    fn variant_fim_framing() {
        assert_eq!(
            frame_edit_prompt(DeepSeekClientKind::Coder, "fn a() {", "}"),
            "<|fim_begin|>fn a() {<|fim_hole|>}<|fim_end|>"
        );
        assert_eq!(
            frame_edit_prompt(DeepSeekClientKind::V3, "fn a() {", "}"),
            "fn a() {}"
        );
        assert_eq!(
            frame_edit_prompt(DeepSeekClientKind::R1, "fn a() {", "}"),
            "fn a() {}"
        );
    }

    #[test]
    fn r1_strip_variants() {
        let (a, r) = r1_strip("<think>chain of thought</think>The answer is 4.");
        assert_eq!(a, "The answer is 4.");
        assert_eq!(r, "chain of thought");
        // Multiple blocks.
        let (a, r) = r1_strip("x<think>one</think>y<think>two</think>z");
        assert_eq!(a, "xyz");
        assert_eq!(r, "onetwo");
        // Unclosed block strips to end.
        let (a, r) = r1_strip("visible<think>truncated reasoning");
        assert_eq!(a, "visible");
        assert_eq!(r, "truncated reasoning");
        // No blocks.
        let (a, r) = r1_strip("plain");
        assert_eq!(a, "plain");
        assert!(r.is_empty());
    }

    #[test]
    fn manifest_verbs_lowercase_flat() {
        let m = tools_manifest();
        let arr = m.as_array().unwrap();
        assert_eq!(arr.len(), 6);
        for v in arr {
            let name = v.req_str("name").unwrap();
            assert_eq!(name, name.to_lowercase());
            // Args are single-level: every value is a type string.
            let args = v.get("args").unwrap().as_map().unwrap();
            for (_, val) in args {
                assert!(val.as_str().is_some());
            }
        }
    }
}
