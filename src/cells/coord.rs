//! Coordination cells — SPEC-DERIVED-§2.13–2.15 (CellTaxonomy.md).
//!
//! AgentRegistryCell tracks agents, their roles, revoked capability tokens,
//! and the escalation-subscriber list (operators who always receive Trinity/
//! Curator events). ProposalCell implements quorum-gated multi-agent
//! decisions (quorum = 2, GAP-011 resolution). SubscriptionCell holds
//! pattern → subscriber registrations consumed by the EventBus.

use super::{CellBehavior, CellType};
use crate::core::cbor::Cbor;
use crate::core::{CellId, SchemaId, UcError, UcResult};
use std::collections::{BTreeMap, BTreeSet};

// ---------------------------------------------------------------------------
// AgentRegistryCell
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct AgentInfo {
    pub agent_id: String,
    pub role: String, // "agent" | "operator" | "curator.librarian" | "curator.warden" | "curator.adjudicator"
    pub registered_at: u64,
    pub active: bool,
}

pub struct AgentRegistryCell {
    pub id: CellId,
    agents: BTreeMap<String, AgentInfo>,
    /// token_id -> logical_at revoked. Router consults this on every verify.
    revoked_tokens: BTreeMap<String, u64>,
    /// agent_ids that must receive every Trinity/Curator event
    /// (RouterScheduler.md §E.3 always-deliver list).
    escalation_subscribers: BTreeSet<String>,
}

impl AgentRegistryCell {
    pub fn new(id: CellId) -> Self {
        AgentRegistryCell {
            id,
            agents: BTreeMap::new(),
            revoked_tokens: BTreeMap::new(),
            escalation_subscribers: BTreeSet::new(),
        }
    }

    pub fn register(&mut self, at: u64, agent_id: &str, role: &str) {
        self.agents.insert(
            agent_id.to_string(),
            AgentInfo {
                agent_id: agent_id.to_string(),
                role: role.to_string(),
                registered_at: at,
                active: true,
            },
        );
        if role == "operator" {
            self.escalation_subscribers.insert(agent_id.to_string());
        }
    }

    pub fn get(&self, agent_id: &str) -> Option<&AgentInfo> {
        self.agents.get(agent_id)
    }

    pub fn revoke_token(&mut self, at: u64, token_id: &str) {
        self.revoked_tokens.insert(token_id.to_string(), at);
    }

    pub fn is_revoked(&self, token_id: &str) -> bool {
        self.revoked_tokens.contains_key(token_id)
    }

    pub fn escalation_subscribers(&self) -> Vec<String> {
        self.escalation_subscribers.iter().cloned().collect()
    }

    pub fn add_escalation_subscriber(&mut self, agent_id: &str) {
        self.escalation_subscribers.insert(agent_id.to_string());
    }
}

impl CellBehavior for AgentRegistryCell {
    fn cell_id(&self) -> CellId {
        self.id
    }
    fn cell_type(&self) -> CellType {
        CellType::AgentRegistry
    }
    fn schema_id(&self) -> SchemaId {
        SchemaId::new("agent_registry.v1")
    }

    fn on_query(&self, _at: u64, query: &Cbor) -> UcResult<Cbor> {
        match query.opt_str("op").as_deref() {
            Some("get") => {
                let id = query.req_str("agent_id")?;
                self.get(&id)
                    .map(|a| {
                        Cbor::map(vec![
                            ("agent_id", Cbor::t(a.agent_id.clone())),
                            ("role", Cbor::t(a.role.clone())),
                            ("active", Cbor::Bool(a.active)),
                        ])
                    })
                    .ok_or_else(|| UcError::not_found(format!("agent {id}")))
            }
            Some("list") => {
                let items: Vec<Cbor> = self
                    .agents
                    .values()
                    .map(|a| {
                        Cbor::map(vec![
                            ("agent_id", Cbor::t(a.agent_id.clone())),
                            ("role", Cbor::t(a.role.clone())),
                            ("active", Cbor::Bool(a.active)),
                        ])
                    })
                    .collect();
                Ok(Cbor::map(vec![("agents", Cbor::Array(items))]))
            }
            Some("is_revoked") => {
                let tid = query.req_str("token_id")?;
                Ok(Cbor::map(vec![("revoked", Cbor::Bool(self.is_revoked(&tid)))]))
            }
            _ => Err(UcError::schema("agent_registry: unknown op")),
        }
    }

    fn on_update(&mut self, at: u64, update: &Cbor) -> UcResult<Cbor> {
        match update.opt_str("op").as_deref() {
            Some("register") | None => {
                let agent_id = update.req_str("agent_id")?;
                let role = update.opt_str("role").unwrap_or_else(|| "agent".into());
                self.register(at, &agent_id, &role);
                Ok(Cbor::map(vec![("registered", Cbor::t(agent_id))]))
            }
            Some("revoke_token") => {
                let tid = update.req_str("token_id")?;
                self.revoke_token(at, &tid);
                Ok(Cbor::map(vec![("revoked", Cbor::t(tid))]))
            }
            Some("deactivate") => {
                let agent_id = update.req_str("agent_id")?;
                if let Some(a) = self.agents.get_mut(&agent_id) {
                    a.active = false;
                }
                Ok(Cbor::map(vec![("deactivated", Cbor::t(agent_id))]))
            }
            _ => Err(UcError::schema("agent_registry: unknown update op")),
        }
    }

    fn snapshot_state(&self) -> Cbor {
        let agents: Vec<Cbor> = self
            .agents
            .values()
            .map(|a| {
                Cbor::map(vec![
                    ("agent_id", Cbor::t(a.agent_id.clone())),
                    ("role", Cbor::t(a.role.clone())),
                    ("registered_at", Cbor::U64(a.registered_at)),
                    ("active", Cbor::Bool(a.active)),
                ])
            })
            .collect();
        let revoked: Vec<Cbor> = self
            .revoked_tokens
            .iter()
            .map(|(t, at)| {
                Cbor::map(vec![
                    ("token_id", Cbor::t(t.clone())),
                    ("at", Cbor::U64(*at)),
                ])
            })
            .collect();
        Cbor::map(vec![
            ("agents", Cbor::Array(agents)),
            ("revoked_tokens", Cbor::Array(revoked)),
            (
                "escalation_subscribers",
                Cbor::text_array(&self.escalation_subscribers.iter().cloned().collect::<Vec<_>>()),
            ),
        ])
    }

    fn restore_state(&mut self, state: &Cbor) -> UcResult<()> {
        self.agents.clear();
        self.revoked_tokens.clear();
        self.escalation_subscribers.clear();
        if let Some(arr) = state.get("agents").and_then(|v| v.as_array()) {
            for item in arr {
                let info = AgentInfo {
                    agent_id: item.req_str("agent_id")?,
                    role: item.opt_str("role").unwrap_or_default(),
                    registered_at: item.opt_u64("registered_at").unwrap_or(0),
                    active: item.opt_bool("active").unwrap_or(true),
                };
                self.agents.insert(info.agent_id.clone(), info);
            }
        }
        if let Some(arr) = state.get("revoked_tokens").and_then(|v| v.as_array()) {
            for item in arr {
                self.revoked_tokens
                    .insert(item.req_str("token_id")?, item.opt_u64("at").unwrap_or(0));
            }
        }
        if let Some(arr) = state.get("escalation_subscribers").and_then(|v| v.as_array()) {
            for item in arr {
                if let Some(s) = item.as_str() {
                    self.escalation_subscribers.insert(s.to_string());
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// ProposalCell — quorum-gated decisions (quorum = 2, GAP-011)
// ---------------------------------------------------------------------------

pub const PROPOSAL_QUORUM: usize = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProposalStatus {
    Open,
    Accepted,
    Rejected,
}

#[derive(Clone, Debug)]
pub struct Proposal {
    pub proposal_id: String,
    pub proposer: String,
    pub body: Cbor,
    pub approvals: BTreeSet<String>,
    pub rejections: BTreeSet<String>,
    pub status: ProposalStatus,
    pub opened_at: u64,
}

pub struct ProposalCell {
    pub id: CellId,
    proposals: BTreeMap<String, Proposal>,
    seq: u64,
}

impl ProposalCell {
    pub fn new(id: CellId) -> Self {
        ProposalCell {
            id,
            proposals: BTreeMap::new(),
            seq: 0,
        }
    }

    pub fn open(&mut self, at: u64, proposer: &str, body: Cbor) -> String {
        self.seq += 1;
        let pid = format!("proposal/{:016}", self.seq);
        self.proposals.insert(
            pid.clone(),
            Proposal {
                proposal_id: pid.clone(),
                proposer: proposer.to_string(),
                body,
                approvals: BTreeSet::new(),
                rejections: BTreeSet::new(),
                status: ProposalStatus::Open,
                opened_at: at,
            },
        );
        pid
    }

    /// Vote. Proposer's own approval doesn't count toward quorum — quorum
    /// means two *other* agents concur (CellTaxonomy.md §2.14).
    pub fn vote(&mut self, pid: &str, voter: &str, approve: bool) -> UcResult<ProposalStatus> {
        let p = self
            .proposals
            .get_mut(pid)
            .ok_or_else(|| UcError::not_found(format!("proposal {pid}")))?;
        if p.status != ProposalStatus::Open {
            return Err(UcError::schema(format!("proposal {pid} already closed")));
        }
        if approve {
            if voter != p.proposer {
                p.approvals.insert(voter.to_string());
            }
        } else {
            p.rejections.insert(voter.to_string());
        }
        if p.approvals.len() >= PROPOSAL_QUORUM {
            p.status = ProposalStatus::Accepted;
        } else if p.rejections.len() >= PROPOSAL_QUORUM {
            p.status = ProposalStatus::Rejected;
        }
        Ok(p.status.clone())
    }

    pub fn get(&self, pid: &str) -> Option<&Proposal> {
        self.proposals.get(pid)
    }
}

impl CellBehavior for ProposalCell {
    fn cell_id(&self) -> CellId {
        self.id
    }
    fn cell_type(&self) -> CellType {
        CellType::Proposal
    }
    fn schema_id(&self) -> SchemaId {
        SchemaId::new("proposal.v1")
    }

    fn on_query(&self, _at: u64, query: &Cbor) -> UcResult<Cbor> {
        let pid = query.req_str("proposal_id")?;
        self.get(&pid)
            .map(|p| {
                Cbor::map(vec![
                    ("proposal_id", Cbor::t(p.proposal_id.clone())),
                    ("proposer", Cbor::t(p.proposer.clone())),
                    (
                        "status",
                        Cbor::t(match p.status {
                            ProposalStatus::Open => "open",
                            ProposalStatus::Accepted => "accepted",
                            ProposalStatus::Rejected => "rejected",
                        }),
                    ),
                    ("approvals", Cbor::U64(p.approvals.len() as u64)),
                    ("rejections", Cbor::U64(p.rejections.len() as u64)),
                    ("body", p.body.clone()),
                ])
            })
            .ok_or_else(|| UcError::not_found(format!("proposal {pid}")))
    }

    fn on_update(&mut self, at: u64, update: &Cbor) -> UcResult<Cbor> {
        match update.opt_str("op").as_deref() {
            Some("open") | None => {
                let proposer = update.req_str("proposer")?;
                let body = update.get("body").cloned().unwrap_or(Cbor::Null);
                let pid = self.open(at, &proposer, body);
                Ok(Cbor::map(vec![("proposal_id", Cbor::t(pid))]))
            }
            Some("vote") => {
                let pid = update.req_str("proposal_id")?;
                let voter = update.req_str("voter")?;
                let approve = update.opt_bool("approve").unwrap_or(true);
                let status = self.vote(&pid, &voter, approve)?;
                Ok(Cbor::map(vec![(
                    "status",
                    Cbor::t(match status {
                        ProposalStatus::Open => "open",
                        ProposalStatus::Accepted => "accepted",
                        ProposalStatus::Rejected => "rejected",
                    }),
                )]))
            }
            _ => Err(UcError::schema("proposal: unknown update op")),
        }
    }

    fn snapshot_state(&self) -> Cbor {
        let items: Vec<Cbor> = self
            .proposals
            .values()
            .map(|p| {
                Cbor::map(vec![
                    ("proposal_id", Cbor::t(p.proposal_id.clone())),
                    ("proposer", Cbor::t(p.proposer.clone())),
                    ("body", p.body.clone()),
                    (
                        "approvals",
                        Cbor::text_array(&p.approvals.iter().cloned().collect::<Vec<_>>()),
                    ),
                    (
                        "rejections",
                        Cbor::text_array(&p.rejections.iter().cloned().collect::<Vec<_>>()),
                    ),
                    (
                        "status",
                        Cbor::t(match p.status {
                            ProposalStatus::Open => "open",
                            ProposalStatus::Accepted => "accepted",
                            ProposalStatus::Rejected => "rejected",
                        }),
                    ),
                    ("opened_at", Cbor::U64(p.opened_at)),
                ])
            })
            .collect();
        Cbor::map(vec![
            ("seq", Cbor::U64(self.seq)),
            ("proposals", Cbor::Array(items)),
        ])
    }

    fn restore_state(&mut self, state: &Cbor) -> UcResult<()> {
        self.proposals.clear();
        self.seq = state.opt_u64("seq").unwrap_or(0);
        if let Some(arr) = state.get("proposals").and_then(|v| v.as_array()) {
            for item in arr {
                let status = match item.opt_str("status").as_deref() {
                    Some("accepted") => ProposalStatus::Accepted,
                    Some("rejected") => ProposalStatus::Rejected,
                    _ => ProposalStatus::Open,
                };
                let approvals = item
                    .get("approvals")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                    .unwrap_or_default();
                let rejections = item
                    .get("rejections")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                    .unwrap_or_default();
                let p = Proposal {
                    proposal_id: item.req_str("proposal_id")?,
                    proposer: item.opt_str("proposer").unwrap_or_default(),
                    body: item.get("body").cloned().unwrap_or(Cbor::Null),
                    approvals,
                    rejections,
                    status,
                    opened_at: item.opt_u64("opened_at").unwrap_or(0),
                };
                self.proposals.insert(p.proposal_id.clone(), p);
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// SubscriptionCell — pattern registry for the EventBus
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Subscription {
    pub sub_id: String,
    pub agent_id: String,
    pub pattern: String, // glob over event names, e.g. "node.*", "curator.**"
    pub since: u64,
}

pub struct SubscriptionCell {
    pub id: CellId,
    subs: BTreeMap<String, Subscription>,
    seq: u64,
}

impl SubscriptionCell {
    pub fn new(id: CellId) -> Self {
        SubscriptionCell {
            id,
            subs: BTreeMap::new(),
            seq: 0,
        }
    }

    pub fn subscribe(&mut self, at: u64, agent_id: &str, pattern: &str) -> String {
        self.seq += 1;
        let sid = format!("sub/{:016}", self.seq);
        self.subs.insert(
            sid.clone(),
            Subscription {
                sub_id: sid.clone(),
                agent_id: agent_id.to_string(),
                pattern: pattern.to_string(),
                since: at,
            },
        );
        sid
    }

    pub fn unsubscribe(&mut self, sid: &str) -> bool {
        self.subs.remove(sid).is_some()
    }

    /// Agents whose patterns match this event name, sorted + deduped.
    pub fn matching(&self, event: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .subs
            .values()
            .filter(|s| crate::core::glob::glob_match(&s.pattern, event))
            .map(|s| s.agent_id.clone())
            .collect();
        out.sort();
        out.dedup();
        out
    }
}

impl CellBehavior for SubscriptionCell {
    fn cell_id(&self) -> CellId {
        self.id
    }
    fn cell_type(&self) -> CellType {
        CellType::Subscription
    }
    fn schema_id(&self) -> SchemaId {
        SchemaId::new("subscription.v1")
    }

    fn on_query(&self, _at: u64, query: &Cbor) -> UcResult<Cbor> {
        let event = query.req_str("event")?;
        Ok(Cbor::map(vec![(
            "subscribers",
            Cbor::text_array(&self.matching(&event)),
        )]))
    }

    fn on_update(&mut self, at: u64, update: &Cbor) -> UcResult<Cbor> {
        match update.opt_str("op").as_deref() {
            Some("subscribe") | None => {
                let agent = update.req_str("agent_id")?;
                let pattern = update.req_str("pattern")?;
                let sid = self.subscribe(at, &agent, &pattern);
                Ok(Cbor::map(vec![("sub_id", Cbor::t(sid))]))
            }
            Some("unsubscribe") => {
                let sid = update.req_str("sub_id")?;
                Ok(Cbor::map(vec![(
                    "removed",
                    Cbor::Bool(self.unsubscribe(&sid)),
                )]))
            }
            _ => Err(UcError::schema("subscription: unknown update op")),
        }
    }

    fn snapshot_state(&self) -> Cbor {
        let items: Vec<Cbor> = self
            .subs
            .values()
            .map(|s| {
                Cbor::map(vec![
                    ("sub_id", Cbor::t(s.sub_id.clone())),
                    ("agent_id", Cbor::t(s.agent_id.clone())),
                    ("pattern", Cbor::t(s.pattern.clone())),
                    ("since", Cbor::U64(s.since)),
                ])
            })
            .collect();
        Cbor::map(vec![
            ("seq", Cbor::U64(self.seq)),
            ("subs", Cbor::Array(items)),
        ])
    }

    fn restore_state(&mut self, state: &Cbor) -> UcResult<()> {
        self.subs.clear();
        self.seq = state.opt_u64("seq").unwrap_or(0);
        if let Some(arr) = state.get("subs").and_then(|v| v.as_array()) {
            for item in arr {
                let s = Subscription {
                    sub_id: item.req_str("sub_id")?,
                    agent_id: item.req_str("agent_id")?,
                    pattern: item.req_str("pattern")?,
                    since: item.opt_u64("since").unwrap_or(0),
                };
                self.subs.insert(s.sub_id.clone(), s);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proposal_quorum_two_others() {
        let mut pc = ProposalCell::new(CellId(12));
        let pid = pc.open(1, "agent-a", Cbor::t("switch embedder dim"));
        // Proposer self-approval doesn't count.
        assert_eq!(pc.vote(&pid, "agent-a", true).unwrap(), ProposalStatus::Open);
        assert_eq!(pc.vote(&pid, "agent-b", true).unwrap(), ProposalStatus::Open);
        assert_eq!(
            pc.vote(&pid, "agent-c", true).unwrap(),
            ProposalStatus::Accepted
        );
        // Closed proposals reject further votes.
        assert!(pc.vote(&pid, "agent-d", false).is_err());
    }

    #[test]
    fn registry_revocation_and_escalation() {
        let mut ar = AgentRegistryCell::new(CellId(11));
        ar.register(1, "op-1", "operator");
        ar.register(1, "agent-1", "agent");
        assert_eq!(ar.escalation_subscribers(), vec!["op-1".to_string()]);
        ar.revoke_token(5, "tok-123");
        assert!(ar.is_revoked("tok-123"));
        assert!(!ar.is_revoked("tok-456"));
        let snap = ar.snapshot_state();
        let mut ar2 = AgentRegistryCell::new(CellId(11));
        ar2.restore_state(&snap).unwrap();
        assert!(ar2.is_revoked("tok-123"));
        assert_eq!(ar2.escalation_subscribers(), vec!["op-1".to_string()]);
    }

    #[test]
    fn subscription_pattern_match() {
        let mut sc = SubscriptionCell::new(CellId(13));
        sc.subscribe(1, "agent-a", "node.*");
        sc.subscribe(1, "agent-b", "curator.**");
        sc.subscribe(1, "agent-c", "node.written");
        assert_eq!(
            sc.matching("node.written"),
            vec!["agent-a".to_string(), "agent-c".to_string()]
        );
        assert_eq!(
            sc.matching("curator.warden.flag"),
            vec!["agent-b".to_string()]
        );
    }
}
