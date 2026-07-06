//! Capability tokens — SPEC-DERIVED-§A (RouterScheduler.md).
//!
//! A token grants (agent, ops, cells, facet_scope, expiry). The body is
//! canonical CBOR; the signature is HMAC-SHA256 over those bytes with the
//! node key. In v0 the single node is both issuer and verifier, so HMAC is
//! sound; the [`Signer`] seam is where Ed25519 lands for multi-node
//! (IMPLEMENTATION_STATUS.md §4 documents the path).
//!
//! Facet scopes are the P19 mechanism: the Warden's default token includes
//! `librarian/output/**` but excludes `…/rationale**` and the other three
//! private facets, so a rationale hydrate dies at token verification —
//! before any cell code runs.

use crate::core::cbor::Cbor;
use crate::core::crypto::{ct_eq, hex, hmac_sha256};
use crate::core::glob::FacetScope;
use crate::core::{Intent, UcError, UcResult};

/// Signature seam. v0: HMAC (single-node). Multi-node: Ed25519 issuer keys.
pub trait Signer: Send + Sync {
    fn sign(&self, body: &[u8]) -> Vec<u8>;
    fn verify(&self, body: &[u8], sig: &[u8]) -> bool;
    fn signer_id(&self) -> String;
}

pub struct HmacSigner {
    key: [u8; 32],
}

impl HmacSigner {
    pub fn new(key: [u8; 32]) -> Self {
        HmacSigner { key }
    }
}

impl Signer for HmacSigner {
    fn sign(&self, body: &[u8]) -> Vec<u8> {
        hmac_sha256(&self.key, body).to_vec()
    }
    fn verify(&self, body: &[u8], sig: &[u8]) -> bool {
        ct_eq(&hmac_sha256(&self.key, body), sig)
    }
    fn signer_id(&self) -> String {
        "hmac.node.v0".into()
    }
}

#[derive(Clone, Debug)]
pub struct CapToken {
    pub token_id: String,
    pub agent_id: String,
    pub ops: Vec<String>,   // lowercase intent names; "admin" for operator plane
    pub cells: Vec<String>, // cell-type globs, e.g. "*" or "Fact"
    pub facet_scope: FacetScope,
    pub expires_at: u64, // logical clock; 0 = never
    pub signature: Vec<u8>,
}

impl CapToken {
    fn body_cbor(
        token_id: &str,
        agent_id: &str,
        ops: &[String],
        cells: &[String],
        facet_scope: &FacetScope,
        expires_at: u64,
    ) -> Cbor {
        Cbor::map(vec![
            ("token_id", Cbor::t(token_id)),
            ("agent_id", Cbor::t(agent_id)),
            ("ops", Cbor::text_array(ops)),
            ("cells", Cbor::text_array(cells)),
            ("facet_scope", Cbor::t(facet_scope.canonical())),
            ("expires_at", Cbor::U64(expires_at)),
        ])
    }

    pub fn issue(
        signer: &dyn Signer,
        token_id: &str,
        agent_id: &str,
        ops: Vec<String>,
        cells: Vec<String>,
        facet_scope: FacetScope,
        expires_at: u64,
    ) -> CapToken {
        let body = Self::body_cbor(token_id, agent_id, &ops, &cells, &facet_scope, expires_at)
            .encode();
        let signature = signer.sign(&body);
        CapToken {
            token_id: token_id.to_string(),
            agent_id: agent_id.to_string(),
            ops,
            cells,
            facet_scope,
            expires_at,
            signature,
        }
    }

    /// Verify signature + expiry. Revocation is the Router's job (it holds
    /// the AgentRegistry) — see `Router::verify_token`.
    pub fn verify(&self, signer: &dyn Signer, now: u64) -> UcResult<()> {
        let body = Self::body_cbor(
            &self.token_id,
            &self.agent_id,
            &self.ops,
            &self.cells,
            &self.facet_scope,
            self.expires_at,
        )
        .encode();
        if !signer.verify(&body, &self.signature) {
            return Err(UcError::denied("capability token signature invalid"));
        }
        if self.expires_at != 0 && now >= self.expires_at {
            return Err(UcError::denied(format!(
                "capability token {} expired at {} (now {})",
                self.token_id, self.expires_at, now
            )));
        }
        Ok(())
    }

    pub fn allows_op(&self, intent: Intent) -> bool {
        self.ops.iter().any(|o| o == intent.as_str())
    }

    pub fn allows_cell(&self, cell_type: &str) -> bool {
        self.cells
            .iter()
            .any(|c| crate::core::glob::glob_match(c, cell_type))
    }

    pub fn allows_facet(&self, facet: &str) -> bool {
        self.facet_scope.allows(facet)
    }

    /// Distinguishes "specifically excluded" from "merely not included" —
    /// only the former increments `curator.rationale_access_denied`.
    pub fn facet_excluded(&self, facet: &str) -> bool {
        self.facet_scope.excluded(facet)
    }

    pub fn to_cbor(&self) -> Cbor {
        Cbor::map(vec![
            ("token_id", Cbor::t(self.token_id.clone())),
            ("agent_id", Cbor::t(self.agent_id.clone())),
            ("ops", Cbor::text_array(&self.ops)),
            ("cells", Cbor::text_array(&self.cells)),
            ("facet_scope", Cbor::t(self.facet_scope.canonical())),
            ("expires_at", Cbor::U64(self.expires_at)),
            ("signature", Cbor::Bytes(self.signature.clone())),
        ])
    }

    pub fn from_cbor(c: &Cbor) -> UcResult<CapToken> {
        Ok(CapToken {
            token_id: c.req_str("token_id")?,
            agent_id: c.req_str("agent_id")?,
            ops: c
                .get("ops")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                .unwrap_or_default(),
            cells: c
                .get("cells")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                .unwrap_or_default(),
            facet_scope: FacetScope::parse(&c.opt_str("facet_scope").unwrap_or_default()),
            expires_at: c.opt_u64("expires_at").unwrap_or(0),
            signature: c
                .get("signature")
                .and_then(|v| v.as_bytes())
                .map(|b| b.to_vec())
                .unwrap_or_default(),
        })
    }

    pub fn signature_hex(&self) -> String {
        hex(&self.signature)
    }
}

// ---------------------------------------------------------------------------
// Standard token shapes (CURATOR_PAIR_PROTOCOL.md §4)
// ---------------------------------------------------------------------------

const ALL_AGENT_OPS: [&str; 6] = ["recall", "hydrate", "write", "subscribe", "view", "supersede"];

/// The four private-facet exclusion globs, parameterized by which curator's
/// outputs are being shielded.
fn private_exclusions(prefix: &str) -> String {
    format!(
        "!{prefix}/*/rationale** !{prefix}/*/considered_alts** \
         !{prefix}/*/reasoning_trace** !{prefix}/*/confidence_precise"
    )
}

/// Plain agent: everything public, no curator private facets, no ledger.
pub fn agent_scope() -> FacetScope {
    FacetScope::parse(&format!(
        "** {} {} !cross_check/**",
        private_exclusions("librarian/output"),
        private_exclusions("warden/judgment"),
    ))
}

/// Librarian: its own facets in full; Warden privates excluded; no ledger.
pub fn librarian_scope() -> FacetScope {
    FacetScope::parse(&format!(
        "** {} !cross_check/**",
        private_exclusions("warden/judgment"),
    ))
}

/// Warden: Librarian privates excluded; its own facets in full; no ledger.
pub fn warden_scope() -> FacetScope {
    FacetScope::parse(&format!(
        "** {} !cross_check/**",
        private_exclusions("librarian/output"),
    ))
}

/// Adjudicator: BOTH curators' privates excluded AND the CrossCheckLedger
/// excluded (prior-blindness is token-enforced, not just structural).
pub fn adjudicator_scope() -> FacetScope {
    FacetScope::parse(&format!(
        "** {} {} !cross_check/**",
        private_exclusions("librarian/output"),
        private_exclusions("warden/judgment"),
    ))
}

/// Operator: unrestricted (audit/forensics plane).
pub fn operator_scope() -> FacetScope {
    FacetScope::parse("**")
}

pub fn issue_agent_token(signer: &dyn Signer, agent_id: &str, expires_at: u64) -> CapToken {
    CapToken::issue(
        signer,
        &format!("tok-{agent_id}"),
        agent_id,
        ALL_AGENT_OPS.iter().map(|s| s.to_string()).collect(),
        vec!["*".into()],
        agent_scope(),
        expires_at,
    )
}

pub fn issue_curator_token(
    signer: &dyn Signer,
    role: &str, // "curator.librarian" | "curator.warden" | "curator.adjudicator"
    expires_at: u64,
) -> CapToken {
    let scope = match role {
        "curator.librarian" => librarian_scope(),
        "curator.warden" => warden_scope(),
        "curator.adjudicator" => adjudicator_scope(),
        _ => agent_scope(),
    };
    CapToken::issue(
        signer,
        &format!("tok-{role}"),
        role,
        ALL_AGENT_OPS.iter().map(|s| s.to_string()).collect(),
        vec!["*".into()],
        scope,
        expires_at,
    )
}

pub fn issue_operator_token(signer: &dyn Signer, agent_id: &str) -> CapToken {
    let mut ops: Vec<String> = ALL_AGENT_OPS.iter().map(|s| s.to_string()).collect();
    ops.push("admin".into());
    CapToken::issue(
        signer,
        &format!("tok-op-{agent_id}"),
        agent_id,
        ops,
        vec!["*".into()],
        operator_scope(),
        0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signer() -> HmacSigner {
        HmacSigner::new([7u8; 32])
    }

    #[test]
    fn sign_verify_expiry_tamper() {
        let s = signer();
        let tok = issue_agent_token(&s, "agent-1", 100);
        assert!(tok.verify(&s, 50).is_ok());
        assert!(tok.verify(&s, 100).is_err()); // expired at boundary
        // Tamper with a field → signature check fails.
        let mut evil = tok.clone();
        evil.agent_id = "agent-2".into();
        assert!(evil.verify(&s, 50).is_err());
        let mut widened = tok.clone();
        widened.facet_scope = operator_scope();
        assert!(widened.verify(&s, 50).is_err());
        // Wrong key.
        let other = HmacSigner::new([8u8; 32]);
        assert!(tok.verify(&other, 50).is_err());
        // Roundtrip through CBOR preserves verifiability.
        let back = CapToken::from_cbor(&tok.to_cbor()).unwrap();
        assert!(back.verify(&s, 50).is_ok());
    }

    #[test]
    fn warden_token_cannot_reach_librarian_privates() {
        let s = signer();
        let tok = issue_curator_token(&s, "curator.warden", 0);
        assert!(tok.allows_facet("librarian/output/01A"));
        assert!(tok.allows_facet("librarian/output/01A/public"));
        assert!(tok.allows_facet("fact/01B"));
        assert!(tok.allows_facet("warden/judgment/01C/rationale")); // its own
        assert!(!tok.allows_facet("librarian/output/01A/rationale"));
        assert!(tok.facet_excluded("librarian/output/01A/rationale"));
        assert!(!tok.allows_facet("librarian/output/01A/considered_alts"));
        assert!(!tok.allows_facet("librarian/output/01A/reasoning_trace"));
        assert!(!tok.allows_facet("librarian/output/01A/confidence_precise"));
        assert!(!tok.allows_facet("cross_check/0001"));
    }

    #[test]
    fn adjudicator_token_is_prior_blind() {
        let s = signer();
        let tok = issue_curator_token(&s, "curator.adjudicator", 0);
        assert!(!tok.allows_facet("cross_check/0001"));
        assert!(tok.facet_excluded("cross_check/0001"));
        assert!(!tok.allows_facet("librarian/output/01A/rationale"));
        assert!(!tok.allows_facet("warden/judgment/01B/rationale"));
        assert!(tok.allows_facet("librarian/output/01A")); // public halves ok
        assert!(tok.allows_facet("warden/judgment/01B"));
    }

    #[test]
    fn op_and_cell_gates() {
        let s = signer();
        let tok = issue_agent_token(&s, "a", 0);
        assert!(tok.allows_op(Intent::Write));
        assert!(!tok.allows_op(Intent::Admin));
        assert!(tok.allows_cell("Fact"));
        let op = issue_operator_token(&s, "root");
        assert!(op.allows_op(Intent::Admin));
        assert!(op.allows_facet("librarian/output/01A/rationale"));
        assert!(op.allows_facet("cross_check/0001"));
    }
}
