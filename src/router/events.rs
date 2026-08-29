//! Event bus — SPEC-DERIVED-§E (RouterScheduler.md).
//!
//! Events are named dotted strings (`node.written`, `trinity.quarantine`,
//! `curator.warden.flag`, …) with a CBOR payload. Delivery is by glob
//! subscription (SubscriptionCell), plus one hard rule: every event under
//! the [`ALWAYS_DELIVER`] prefixes is delivered to the escalation
//! subscribers (operators) regardless of their subscriptions — governance
//! and curator incidents must never be silently missable (§E.3).
//!
//! A 4096-entry ring buffer allows `since`-replay for late subscribers.

use crate::cells::coord::{AgentRegistryCell, SubscriptionCell};
use crate::core::cbor::Cbor;
use std::collections::VecDeque;

pub const EVENT_RING: usize = 4096;

/// Event-name prefixes always delivered to escalation subscribers.
pub const ALWAYS_DELIVER: [&str; 3] = ["trinity.", "curator.", "node.fatal"];

#[derive(Clone, Debug)]
pub struct Event {
    pub seq: u64,
    pub name: String,
    pub payload: Cbor,
    pub logical_at: u64,
}

#[derive(Default)]
pub struct EventBus {
    ring: VecDeque<Event>,
    next_seq: u64,
    /// (agent_id, event_seq) pairs pending pickup — v0 delivery is
    /// pull-based over the wire; push lands with subscribe streaming.
    pending: Vec<(String, u64)>,
}

impl EventBus {
    pub fn new() -> Self {
        EventBus {
            ring: VecDeque::with_capacity(EVENT_RING),
            next_seq: 0,
            pending: Vec::new(),
        }
    }

    /// Publish an event; compute the recipient set and queue deliveries.
    /// Returns the recipients (sorted, deduped) for observability.
    pub fn publish(
        &mut self,
        subs: &SubscriptionCell,
        registry: &AgentRegistryCell,
        logical_at: u64,
        name: &str,
        payload: Cbor,
    ) -> Vec<String> {
        let seq = self.next_seq;
        self.next_seq += 1;
        if self.ring.len() == EVENT_RING {
            self.ring.pop_front();
        }
        self.ring.push_back(Event {
            seq,
            name: name.to_string(),
            payload,
            logical_at,
        });

        let mut recipients = subs.matching(name);
        if ALWAYS_DELIVER.iter().any(|p| name.starts_with(p)) {
            recipients.extend(registry.escalation_subscribers());
        }
        recipients.sort();
        recipients.dedup();
        for r in &recipients {
            self.pending.push((r.clone(), seq));
        }
        recipients
    }

    /// Drain queued events for one agent (pull-based delivery).
    pub fn drain_for(&mut self, agent_id: &str) -> Vec<Event> {
        let mut seqs: Vec<u64> = Vec::new();
        self.pending.retain(|(a, s)| {
            if a == agent_id {
                seqs.push(*s);
                false
            } else {
                true
            }
        });
        seqs.sort_unstable();
        seqs.iter()
            .filter_map(|s| self.ring.iter().find(|e| e.seq == *s).cloned())
            .collect()
    }

    /// Replay events with seq > since (late-subscriber catch-up).
    pub fn since(&self, since: u64) -> Vec<&Event> {
        self.ring.iter().filter(|e| e.seq > since).collect()
    }

    /// Replay only events the named agent is currently entitled to receive.
    /// The wire protocol uses this instead of exposing the unfiltered ring so
    /// a caller cannot turn a replay cursor into an event side channel.
    pub fn since_for(
        &self,
        agent_id: &str,
        since: u64,
        subs: &SubscriptionCell,
        registry: &AgentRegistryCell,
    ) -> Vec<Event> {
        self.since(since)
            .into_iter()
            .filter(|event| {
                subs.entitled_at(agent_id, &event.name, event.logical_at)
                    || (ALWAYS_DELIVER
                        .iter()
                        .any(|prefix| event.name.starts_with(prefix))
                        && registry
                            .get(agent_id)
                            .is_some_and(|info| info.active && info.role == "operator"))
            })
            .cloned()
            .collect()
    }

    pub fn latest_seq(&self) -> u64 {
        self.next_seq.saturating_sub(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::CellId;

    fn setup() -> (EventBus, SubscriptionCell, AgentRegistryCell) {
        let mut subs = SubscriptionCell::new(CellId(13));
        let mut reg = AgentRegistryCell::new(CellId(11));
        reg.register(0, "op-1", "operator");
        reg.register(0, "agent-a", "agent");
        subs.subscribe(0, "agent-a", "node.*");
        (EventBus::new(), subs, reg)
    }

    #[test]
    fn glob_delivery_and_always_deliver() {
        let (mut bus, subs, reg) = setup();
        // node.written matches agent-a's glob; not a governance event, so
        // op-1 (who has no subscription) is not included.
        let r = bus.publish(&subs, &reg, 1, "node.written", Cbor::t("h"));
        assert_eq!(r, vec!["agent-a".to_string()]);
        // trinity.quarantine hits ALWAYS_DELIVER → operator included even
        // without a subscription.
        let r = bus.publish(&subs, &reg, 2, "trinity.quarantine", Cbor::Null);
        assert_eq!(r, vec!["op-1".to_string()]);
        // curator.* likewise.
        let r = bus.publish(&subs, &reg, 3, "curator.warden.flag", Cbor::Null);
        assert_eq!(r, vec!["op-1".to_string()]);
    }

    #[test]
    fn drain_and_since_replay() {
        let (mut bus, subs, reg) = setup();
        bus.publish(&subs, &reg, 1, "node.written", Cbor::U64(1));
        bus.publish(&subs, &reg, 2, "node.written", Cbor::U64(2));
        bus.publish(&subs, &reg, 3, "trinity.quarantine", Cbor::U64(3));
        let drained = bus.drain_for("agent-a");
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].seq, 0);
        assert_eq!(drained[1].seq, 1);
        // Second drain is empty.
        assert!(bus.drain_for("agent-a").is_empty());
        // Operator still has the governance event queued.
        let op = bus.drain_for("op-1");
        assert_eq!(op.len(), 1);
        assert_eq!(op[0].name, "trinity.quarantine");
        // since-replay sees everything after seq 0.
        assert_eq!(bus.since(0).len(), 2);
    }

    #[test]
    fn ring_caps_at_capacity() {
        let (mut bus, subs, reg) = setup();
        for i in 0..(EVENT_RING as u64 + 100) {
            bus.publish(&subs, &reg, i, "misc.tick", Cbor::U64(i));
        }
        assert_eq!(bus.ring.len(), EVENT_RING);
        assert_eq!(bus.latest_seq(), EVENT_RING as u64 + 99);
        // Oldest entries evicted.
        assert!(bus.since(0).len() == EVENT_RING);
    }

    #[test]
    fn since_replay_honors_subscription_activation_cursor() {
        let mut bus = EventBus::new();
        let mut subs = SubscriptionCell::new(CellId(13));
        let mut reg = AgentRegistryCell::new(CellId(11));
        reg.register(0, "agent-a", "agent");
        subs.subscribe(10, "agent-a", "node.*");
        bus.publish(&subs, &reg, 9, "node.written", Cbor::U64(9));
        bus.publish(&subs, &reg, 10, "node.written", Cbor::U64(10));

        let replay = bus.since_for("agent-a", 0, &subs, &reg);
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].logical_at, 10);
    }
}
