//! Envelope — SPEC-DERIVED-§4–§5 (McpProtocol.md), §C (RouterScheduler.md).
//!
//! Every request travels in an Envelope. Four invariants are checked at the
//! protocol edge, before the Trinity chain ever sees the request:
//!
//! - **E1** — `proto_version` must equal 1.
//! - **E2** — `work_budget` is MANDATORY on every envelope, including
//!   reads. An envelope without one is rejected with `BudgetExceeded`
//!   (McpProtocol.md §5.2: "there is no free work").
//! - **E3** — `intent` must parse to a known verb, and state-changing
//!   intents must carry a payload.
//! - **E4** — `request_id` must be a valid ULID (dedup + traceability).

use super::captoken::CapToken;
use crate::core::cbor::Cbor;
use crate::core::ulid::Ulid;
use crate::core::{ErrCode, Intent, Severity, Tier, UcError, UcResult};

pub const PROTO_VERSION: u64 = 1;

#[derive(Clone, Copy, Debug, Default)]
pub struct EnvelopeFlags {
    /// Request the Warden semantic gate (chain step 6). The Router forces
    /// this on for P0 severity regardless of the flag.
    pub semantic_check: bool,
    /// Continuation of a prior truncated recall/view (R1 flow).
    pub continuation: bool,
}

#[derive(Clone, Debug)]
pub struct WorkBudget {
    pub task_id: String,
    /// Work units the sender authorizes for this envelope.
    pub units: u64,
}

#[derive(Clone, Debug)]
pub struct Envelope {
    pub proto_version: u64,
    pub request_id: Ulid,
    pub agent_id: String,
    pub capability: CapToken,
    pub work_budget: WorkBudget, // E2: mandatory, not Option
    pub intent: Intent,
    pub payload: Cbor,
    pub spec_anchor: Option<String>, // "Doc.md§Section"
    pub severity: Severity,
    pub gap_ref: Option<String>,
    pub tier: Tier,
    pub seed: u64,
    pub flags: EnvelopeFlags,
}

impl Envelope {
    pub fn to_cbor(&self) -> Cbor {
        Cbor::map(vec![
            ("proto_version", Cbor::U64(self.proto_version)),
            ("request_id", Cbor::t(self.request_id.to_base32())),
            ("agent_id", Cbor::t(self.agent_id.clone())),
            ("capability", self.capability.to_cbor()),
            (
                "work_budget",
                Cbor::map(vec![
                    ("task_id", Cbor::t(self.work_budget.task_id.clone())),
                    ("units", Cbor::U64(self.work_budget.units)),
                ]),
            ),
            ("intent", Cbor::t(self.intent.as_str())),
            ("payload", self.payload.clone()),
            (
                "spec_anchor",
                self.spec_anchor
                    .as_ref()
                    .map(|a| Cbor::t(a.clone()))
                    .unwrap_or(Cbor::Null),
            ),
            ("severity", Cbor::t(self.severity.as_str())),
            (
                "gap_ref",
                self.gap_ref
                    .as_ref()
                    .map(|g| Cbor::t(g.clone()))
                    .unwrap_or(Cbor::Null),
            ),
            ("tier", Cbor::t(self.tier.as_str())),
            ("seed", Cbor::U64(self.seed)),
            (
                "flags",
                Cbor::map(vec![
                    ("semantic_check", Cbor::Bool(self.flags.semantic_check)),
                    ("continuation", Cbor::Bool(self.flags.continuation)),
                ]),
            ),
        ])
    }

    /// Parse + enforce E1–E4. Every failure names its invariant.
    pub fn from_cbor(c: &Cbor) -> UcResult<Envelope> {
        // E1 — protocol version.
        let proto_version = c.opt_u64("proto_version").unwrap_or(0);
        if proto_version != PROTO_VERSION {
            return Err(UcError::new(
                ErrCode::ContractViolation,
                format!("E1: proto_version {proto_version} unsupported (expected {PROTO_VERSION})"),
            ));
        }

        // E4 — request id.
        let request_id = c
            .opt_str("request_id")
            .and_then(|s| Ulid::from_base32(&s))
            .ok_or_else(|| {
                UcError::new(ErrCode::ContractViolation, "E4: request_id must be a valid ULID")
            })?;

        let agent_id = c.req_str("agent_id")?;
        let capability = CapToken::from_cbor(
            c.get("capability")
                .ok_or_else(|| UcError::denied("envelope missing capability token"))?,
        )?;

        // E2 — work budget is MANDATORY.
        let wb = c.get("work_budget").ok_or_else(|| {
            UcError::new(
                ErrCode::BudgetExceeded,
                "E2: work_budget is mandatory on every envelope",
            )
        })?;
        let work_budget = WorkBudget {
            task_id: wb.req_str("task_id").map_err(|_| {
                UcError::new(ErrCode::BudgetExceeded, "E2: work_budget.task_id required")
            })?,
            units: wb.opt_u64("units").unwrap_or(0),
        };

        // E3 — intent.
        let intent_str = c.req_str("intent")?;
        let intent = Intent::from_str(&intent_str).ok_or_else(|| {
            UcError::new(
                ErrCode::Unimplemented,
                format!("E3: unknown intent `{intent_str}`"),
            )
        })?;
        let payload = c.get("payload").cloned().unwrap_or(Cbor::Null);
        if intent.is_state_changing() && payload == Cbor::Null {
            return Err(UcError::new(
                ErrCode::ContractViolation,
                format!("E3: {} requires a payload", intent.as_str()),
            ));
        }

        let flags_c = c.get("flags");
        let flags = EnvelopeFlags {
            semantic_check: flags_c
                .and_then(|f| f.opt_bool("semantic_check"))
                .unwrap_or(false),
            continuation: flags_c
                .and_then(|f| f.opt_bool("continuation"))
                .unwrap_or(false),
        };

        Ok(Envelope {
            proto_version,
            request_id,
            agent_id,
            capability,
            work_budget,
            intent,
            payload,
            spec_anchor: c.opt_str("spec_anchor"),
            severity: c
                .opt_str("severity")
                .and_then(|s| Severity::from_str(&s))
                .unwrap_or(Severity::P2),
            gap_ref: c.opt_str("gap_ref"),
            tier: c
                .opt_str("tier")
                .and_then(|s| Tier::from_str(&s))
                .unwrap_or(Tier::L1),
            seed: c.opt_u64("seed").unwrap_or(0),
            flags,
        })
    }
}

// ---------------------------------------------------------------------------
// Response envelope
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct ResponseEnvelope {
    pub request_id: Ulid,
    pub ok: bool,
    pub result: Cbor,
    pub err_code: Option<ErrCode>,
    pub err_message: Option<String>,
    pub quarantine_id: Option<String>,
    /// Tokens this response consumed against the work budget.
    pub tokens_emitted: u64,
    /// Set when a recall/view was truncated at the requested tier.
    pub next_tier_hint: Option<Tier>,
    pub logical_at: u64,
}

impl ResponseEnvelope {
    pub fn ok(request_id: Ulid, logical_at: u64, result: Cbor, tokens: u64) -> ResponseEnvelope {
        ResponseEnvelope {
            request_id,
            ok: true,
            result,
            err_code: None,
            err_message: None,
            quarantine_id: None,
            tokens_emitted: tokens,
            next_tier_hint: None,
            logical_at,
        }
    }

    pub fn err(request_id: Ulid, logical_at: u64, e: &UcError) -> ResponseEnvelope {
        ResponseEnvelope {
            request_id,
            ok: false,
            result: Cbor::Null,
            err_code: Some(e.code),
            err_message: Some(e.message.clone()),
            quarantine_id: e.quarantine_id.map(|q| format!("quarantine/{q}")),
            tokens_emitted: 0,
            next_tier_hint: None,
            logical_at,
        }
    }

    pub fn to_cbor(&self) -> Cbor {
        Cbor::map(vec![
            ("request_id", Cbor::t(self.request_id.to_base32())),
            ("ok", Cbor::Bool(self.ok)),
            ("result", self.result.clone()),
            (
                "err_code",
                self.err_code.map(|c| Cbor::t(c.as_str())).unwrap_or(Cbor::Null),
            ),
            (
                "err_message",
                self.err_message
                    .as_ref()
                    .map(|m| Cbor::t(m.clone()))
                    .unwrap_or(Cbor::Null),
            ),
            (
                "quarantine_id",
                self.quarantine_id
                    .as_ref()
                    .map(|q| Cbor::t(q.clone()))
                    .unwrap_or(Cbor::Null),
            ),
            ("tokens_emitted", Cbor::U64(self.tokens_emitted)),
            (
                "next_tier_hint",
                self.next_tier_hint
                    .map(|t| Cbor::t(t.as_str()))
                    .unwrap_or(Cbor::Null),
            ),
            ("logical_at", Cbor::U64(self.logical_at)),
        ])
    }

    pub fn from_cbor(c: &Cbor) -> UcResult<ResponseEnvelope> {
        Ok(ResponseEnvelope {
            request_id: c
                .opt_str("request_id")
                .and_then(|s| Ulid::from_base32(&s))
                .unwrap_or_else(Ulid::nil),
            ok: c.opt_bool("ok").unwrap_or(false),
            result: c.get("result").cloned().unwrap_or(Cbor::Null),
            err_code: c.opt_str("err_code").and_then(|s| ErrCode::from_str(&s)),
            err_message: c.opt_str("err_message"),
            quarantine_id: c.opt_str("quarantine_id"),
            tokens_emitted: c.opt_u64("tokens_emitted").unwrap_or(0),
            next_tier_hint: c.opt_str("next_tier_hint").and_then(|s| Tier::from_str(&s)),
            logical_at: c.opt_u64("logical_at").unwrap_or(0),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::captoken::{issue_agent_token, HmacSigner};
    use crate::core::ulid::DetRng;

    fn envelope() -> Envelope {
        let signer = HmacSigner::new([1u8; 32]);
        Envelope {
            proto_version: PROTO_VERSION,
            request_id: Ulid::from_parts(9, &mut DetRng::new(3)),
            agent_id: "agent-1".into(),
            capability: issue_agent_token(&signer, "agent-1", 0),
            work_budget: WorkBudget {
                task_id: "task-A".into(),
                units: 500,
            },
            intent: Intent::Write,
            payload: Cbor::map(vec![("subject", Cbor::t("s"))]),
            spec_anchor: Some("Architecture.md\u{00a7}4".into()),
            severity: Severity::P1,
            gap_ref: None,
            tier: Tier::L1,
            seed: 7,
            flags: EnvelopeFlags::default(),
        }
    }

    #[test]
    fn roundtrip() {
        let e = envelope();
        let c = e.to_cbor();
        let e2 = Envelope::from_cbor(&c).unwrap();
        assert_eq!(e2.request_id, e.request_id);
        assert_eq!(e2.intent, Intent::Write);
        assert_eq!(e2.work_budget.task_id, "task-A");
        assert_eq!(e2.work_budget.units, 500);
        assert_eq!(e2.severity, Severity::P1);
        assert_eq!(e2.spec_anchor.as_deref(), Some("Architecture.md\u{00a7}4"));
    }

    #[test]
    fn e_invariants() {
        let e = envelope();
        // E1 — wrong proto version.
        let mut c = e.to_cbor();
        if let Cbor::Map(pairs) = &mut c {
            for (k, v) in pairs.iter_mut() {
                if k.as_str() == Some("proto_version") {
                    *v = Cbor::U64(99);
                }
            }
        }
        let err = Envelope::from_cbor(&c).unwrap_err();
        assert!(err.message.starts_with("E1"));

        // E2 — missing work_budget.
        let mut c = e.to_cbor();
        if let Cbor::Map(pairs) = &mut c {
            pairs.retain(|(k, _)| k.as_str() != Some("work_budget"));
        }
        let err = Envelope::from_cbor(&c).unwrap_err();
        assert_eq!(err.code, ErrCode::BudgetExceeded);
        assert!(err.message.starts_with("E2"));

        // E3 — unknown intent.
        let mut c = e.to_cbor();
        if let Cbor::Map(pairs) = &mut c {
            for (k, v) in pairs.iter_mut() {
                if k.as_str() == Some("intent") {
                    *v = Cbor::t("meditate");
                }
            }
        }
        assert!(Envelope::from_cbor(&c).unwrap_err().message.starts_with("E3"));

        // E3 — write without payload.
        let mut c = e.to_cbor();
        if let Cbor::Map(pairs) = &mut c {
            for (k, v) in pairs.iter_mut() {
                if k.as_str() == Some("payload") {
                    *v = Cbor::Null;
                }
            }
        }
        assert!(Envelope::from_cbor(&c).unwrap_err().message.starts_with("E3"));

        // E4 — malformed request id.
        let mut c = e.to_cbor();
        if let Cbor::Map(pairs) = &mut c {
            for (k, v) in pairs.iter_mut() {
                if k.as_str() == Some("request_id") {
                    *v = Cbor::t("not-a-ulid");
                }
            }
        }
        assert!(Envelope::from_cbor(&c).unwrap_err().message.starts_with("E4"));
    }

    #[test]
    fn response_error_carries_quarantine() {
        let e = UcError::new(ErrCode::AnchorMissing, "no anchor")
            .with_quarantine(Ulid::from_parts(4, &mut DetRng::new(1)));
        let r = ResponseEnvelope::err(Ulid::nil(), 10, &e);
        assert!(!r.ok);
        assert_eq!(r.err_code, Some(ErrCode::AnchorMissing));
        assert!(r.quarantine_id.as_deref().unwrap().starts_with("quarantine/"));
        let back = ResponseEnvelope::from_cbor(&r.to_cbor()).unwrap();
        assert_eq!(back.err_code, Some(ErrCode::AnchorMissing));
        assert_eq!(back.quarantine_id, r.quarantine_id);
    }
}
