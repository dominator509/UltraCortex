//! Memory cells — SPEC-DERIVED-§2.2–2.7 (CellTaxonomy.md).
//!
//! FactCell: (subject, predicate, object) triples with supersession chains
//! and `by_subject` / `by_sp` indexes. TimelineCell: append-only ring per
//! stream. ScratchpadCell: TTL'd working memory (logical-clock expiry heap;
//! anchor-exempt). PlaybookCell: named procedure documents. BlobCell: thin
//! wrapper over the CAS. CacheCell: in-memory LRU keyed by string.

use super::{CellBehavior, CellType};
use crate::core::cbor::Cbor;
use crate::core::ulid::Ulid;
use crate::core::{CellId, SchemaId, UcError, UcResult};
use std::collections::{BTreeMap, BinaryHeap, HashMap};

// ---------------------------------------------------------------------------
// FactCell
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Fact {
    pub handle: String, // "fact/<ulid>"
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub confidence: Option<String>, // band label, if curator-graded
    pub written_at: u64,
    pub superseded_by: Option<String>,
    pub supersedes: Option<String>,
    pub anchor: String,
}

pub struct FactCell {
    pub id: CellId,
    facts: BTreeMap<String, Fact>,
    by_subject: BTreeMap<String, Vec<String>>,
    by_sp: BTreeMap<(String, String), Vec<String>>,
}

impl FactCell {
    pub fn new(id: CellId) -> Self {
        FactCell {
            id,
            facts: BTreeMap::new(),
            by_subject: BTreeMap::new(),
            by_sp: BTreeMap::new(),
        }
    }

    pub fn insert(&mut self, fact: Fact) {
        self.by_subject
            .entry(fact.subject.clone())
            .or_default()
            .push(fact.handle.clone());
        self.by_sp
            .entry((fact.subject.clone(), fact.predicate.clone()))
            .or_default()
            .push(fact.handle.clone());
        self.facts.insert(fact.handle.clone(), fact);
    }

    /// Insert a new fact and, when requested, update both sides of its
    /// supersession edge as one validated cell transition. Callers persist
    /// the transition before invoking this method.
    pub fn insert_with_supersede(&mut self, fact: Fact, old: Option<&str>) -> UcResult<()> {
        if self.facts.contains_key(&fact.handle) {
            return Err(UcError::schema(format!(
                "fact {} already exists",
                fact.handle
            )));
        }
        if let Some(old) = old {
            let old_fact = self
                .facts
                .get(old)
                .ok_or_else(|| UcError::not_found(format!("old fact {old} not found")))?;
            if old_fact.superseded_by.is_some() {
                return Err(UcError::schema(format!("{old} already superseded")));
            }
        }
        let new = fact.handle.clone();
        self.insert(fact);
        if let Some(old) = old {
            self.facts
                .get_mut(old)
                .expect("supersede target validated before insert")
                .superseded_by = Some(new.clone());
            self.facts.get_mut(&new).unwrap().supersedes = Some(old.to_string());
        }
        Ok(())
    }

    pub fn get(&self, handle: &str) -> Option<&Fact> {
        self.facts.get(handle)
    }

    pub fn exists(&self, handle: &str) -> bool {
        self.facts.contains_key(handle)
    }

    /// Active (non-superseded) facts for a (subject, predicate) pair. The
    /// Warden's drift check keys off this: a new fact for an occupied (s,p)
    /// with a different object and no `supersedes` link is semantic drift
    /// (WardenCell.md §4.2).
    pub fn active_for_sp(&self, subject: &str, predicate: &str) -> Vec<&Fact> {
        self.by_sp
            .get(&(subject.to_string(), predicate.to_string()))
            .map(|handles| {
                handles
                    .iter()
                    .filter_map(|h| self.facts.get(h))
                    .filter(|f| f.superseded_by.is_none())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn active_for_subject(&self, subject: &str) -> Vec<&Fact> {
        self.by_subject
            .get(subject)
            .map(|handles| {
                handles
                    .iter()
                    .filter_map(|h| self.facts.get(h))
                    .filter(|f| f.superseded_by.is_none())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Mark `old` superseded by `new`. Both must exist.
    pub fn supersede(&mut self, old: &str, new: &str) -> UcResult<()> {
        if !self.facts.contains_key(new) {
            return Err(UcError::not_found(format!("new fact {new} not found")));
        }
        let old_fact = self
            .facts
            .get_mut(old)
            .ok_or_else(|| UcError::not_found(format!("old fact {old} not found")))?;
        if old_fact.superseded_by.is_some() {
            return Err(UcError::schema(format!("{old} already superseded")));
        }
        old_fact.superseded_by = Some(new.to_string());
        self.facts.get_mut(new).unwrap().supersedes = Some(old.to_string());
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.facts.len()
    }
    pub fn is_empty(&self) -> bool {
        self.facts.is_empty()
    }
    pub fn iter(&self) -> impl Iterator<Item = &Fact> {
        self.facts.values()
    }

    fn fact_to_cbor(f: &Fact) -> Cbor {
        Cbor::map(vec![
            ("handle", Cbor::t(f.handle.clone())),
            ("subject", Cbor::t(f.subject.clone())),
            ("predicate", Cbor::t(f.predicate.clone())),
            ("object", Cbor::t(f.object.clone())),
            (
                "confidence",
                f.confidence
                    .as_ref()
                    .map(|c| Cbor::t(c.clone()))
                    .unwrap_or(Cbor::Null),
            ),
            ("written_at", Cbor::U64(f.written_at)),
            (
                "superseded_by",
                f.superseded_by
                    .as_ref()
                    .map(|s| Cbor::t(s.clone()))
                    .unwrap_or(Cbor::Null),
            ),
            (
                "supersedes",
                f.supersedes
                    .as_ref()
                    .map(|s| Cbor::t(s.clone()))
                    .unwrap_or(Cbor::Null),
            ),
            ("anchor", Cbor::t(f.anchor.clone())),
        ])
    }

    fn fact_from_cbor(c: &Cbor) -> UcResult<Fact> {
        Ok(Fact {
            handle: c.req_str("handle")?,
            subject: c.req_str("subject")?,
            predicate: c.req_str("predicate")?,
            object: c.req_str("object")?,
            confidence: c.opt_str("confidence"),
            written_at: c.opt_u64("written_at").unwrap_or(0),
            superseded_by: c.opt_str("superseded_by"),
            supersedes: c.opt_str("supersedes"),
            anchor: c.opt_str("anchor").unwrap_or_default(),
        })
    }
}

impl CellBehavior for FactCell {
    fn cell_id(&self) -> CellId {
        self.id
    }
    fn cell_type(&self) -> CellType {
        CellType::Fact
    }
    fn schema_id(&self) -> SchemaId {
        SchemaId::new("fact.v1")
    }

    fn on_query(&self, _at: u64, query: &Cbor) -> UcResult<Cbor> {
        match query.opt_str("op").as_deref() {
            Some("get") => {
                let h = query.req_str("handle")?;
                self.get(&h)
                    .map(Self::fact_to_cbor)
                    .ok_or_else(|| UcError::not_found(format!("fact {h}")))
            }
            Some("by_subject") => {
                let s = query.req_str("subject")?;
                let items: Vec<Cbor> = self
                    .active_for_subject(&s)
                    .iter()
                    .map(|f| Self::fact_to_cbor(f))
                    .collect();
                Ok(Cbor::map(vec![("facts", Cbor::Array(items))]))
            }
            Some("by_sp") => {
                let s = query.req_str("subject")?;
                let p = query.req_str("predicate")?;
                let items: Vec<Cbor> = self
                    .active_for_sp(&s, &p)
                    .iter()
                    .map(|f| Self::fact_to_cbor(f))
                    .collect();
                Ok(Cbor::map(vec![("facts", Cbor::Array(items))]))
            }
            _ => Err(UcError::schema("fact: unknown op")),
        }
    }

    fn on_update(&mut self, at: u64, update: &Cbor) -> UcResult<Cbor> {
        match update.opt_str("op").as_deref() {
            Some("put") | None => {
                let seed = update.opt_u64("seed").unwrap_or(0);
                let handle = update.opt_str("handle").unwrap_or_else(|| {
                    format!(
                        "fact/{}",
                        Ulid::from_parts(at, &mut crate::core::ulid::DetRng::new(seed ^ at))
                    )
                });
                let fact = Fact {
                    handle: handle.clone(),
                    subject: update.req_str("subject")?,
                    predicate: update.req_str("predicate")?,
                    object: update.req_str("object")?,
                    confidence: update.opt_str("confidence"),
                    written_at: at,
                    superseded_by: None,
                    supersedes: None,
                    anchor: update.opt_str("anchor").unwrap_or_default(),
                };
                self.insert(fact);
                Ok(Cbor::map(vec![("handle", Cbor::t(handle))]))
            }
            Some("supersede") => {
                let old = update.req_str("old")?;
                let new = update.req_str("new")?;
                self.supersede(&old, &new)?;
                Ok(Cbor::map(vec![
                    ("superseded", Cbor::t(old)),
                    ("by", Cbor::t(new)),
                ]))
            }
            _ => Err(UcError::schema("fact: unknown update op")),
        }
    }

    fn snapshot_state(&self) -> Cbor {
        let items: Vec<Cbor> = self.facts.values().map(Self::fact_to_cbor).collect();
        Cbor::map(vec![("facts", Cbor::Array(items))])
    }

    fn restore_state(&mut self, state: &Cbor) -> UcResult<()> {
        self.facts.clear();
        self.by_subject.clear();
        self.by_sp.clear();
        if let Some(arr) = state.get("facts").and_then(|v| v.as_array()) {
            for item in arr {
                self.insert(Self::fact_from_cbor(item)?);
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// TimelineCell — append-only bounded ring per stream (§2.3)
// ---------------------------------------------------------------------------

const TIMELINE_RING_CAP: usize = 4096;

pub struct TimelineCell {
    pub id: CellId,
    streams: BTreeMap<String, Vec<(u64, String, Cbor)>>, // (at, handle, event)
    seq: u64,
}

impl TimelineCell {
    pub fn new(id: CellId) -> Self {
        TimelineCell {
            id,
            streams: BTreeMap::new(),
            seq: 0,
        }
    }

    pub fn append(&mut self, at: u64, stream: &str, event: Cbor) -> String {
        self.seq += 1;
        let handle = format!("timeline/{}/{:016}", stream, self.seq);
        let entries = self.streams.entry(stream.to_string()).or_default();
        entries.push((at, handle.clone(), event));
        if entries.len() > TIMELINE_RING_CAP {
            let overflow = entries.len() - TIMELINE_RING_CAP;
            entries.drain(0..overflow);
        }
        handle
    }

    pub fn tail(&self, stream: &str, n: usize) -> Vec<&(u64, String, Cbor)> {
        self.streams
            .get(stream)
            .map(|v| v.iter().rev().take(n).collect::<Vec<_>>())
            .unwrap_or_default()
    }
}

impl CellBehavior for TimelineCell {
    fn cell_id(&self) -> CellId {
        self.id
    }
    fn cell_type(&self) -> CellType {
        CellType::Timeline
    }
    fn schema_id(&self) -> SchemaId {
        SchemaId::new("timeline.v1")
    }

    fn on_query(&self, _at: u64, query: &Cbor) -> UcResult<Cbor> {
        let stream = query.req_str("stream")?;
        let n = query.opt_u64("n").unwrap_or(32) as usize;
        let items: Vec<Cbor> = self
            .tail(&stream, n)
            .into_iter()
            .rev()
            .map(|(at, handle, ev)| {
                Cbor::map(vec![
                    ("at", Cbor::U64(*at)),
                    ("handle", Cbor::t(handle.clone())),
                    ("event", ev.clone()),
                ])
            })
            .collect();
        Ok(Cbor::map(vec![("entries", Cbor::Array(items))]))
    }

    fn on_update(&mut self, at: u64, update: &Cbor) -> UcResult<Cbor> {
        let stream = update.req_str("stream")?;
        let event = update
            .get("event")
            .cloned()
            .ok_or_else(|| UcError::schema("timeline: missing event"))?;
        let handle = self.append(at, &stream, event);
        Ok(Cbor::map(vec![("handle", Cbor::t(handle))]))
    }

    fn snapshot_state(&self) -> Cbor {
        let streams: Vec<Cbor> = self
            .streams
            .iter()
            .map(|(name, entries)| {
                let items: Vec<Cbor> = entries
                    .iter()
                    .map(|(at, h, ev)| {
                        Cbor::map(vec![
                            ("at", Cbor::U64(*at)),
                            ("handle", Cbor::t(h.clone())),
                            ("event", ev.clone()),
                        ])
                    })
                    .collect();
                Cbor::map(vec![
                    ("stream", Cbor::t(name.clone())),
                    ("entries", Cbor::Array(items)),
                ])
            })
            .collect();
        Cbor::map(vec![
            ("seq", Cbor::U64(self.seq)),
            ("streams", Cbor::Array(streams)),
        ])
    }

    fn restore_state(&mut self, state: &Cbor) -> UcResult<()> {
        self.streams.clear();
        self.seq = state.opt_u64("seq").unwrap_or(0);
        if let Some(arr) = state.get("streams").and_then(|v| v.as_array()) {
            for s in arr {
                let name = s.req_str("stream")?;
                let mut entries = Vec::new();
                if let Some(items) = s.get("entries").and_then(|v| v.as_array()) {
                    for item in items {
                        entries.push((
                            item.opt_u64("at").unwrap_or(0),
                            item.req_str("handle")?,
                            item.get("event").cloned().unwrap_or(Cbor::Null),
                        ));
                    }
                }
                self.streams.insert(name, entries);
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// ScratchpadCell — TTL working memory (§2.5); anchor-exempt
// ---------------------------------------------------------------------------

#[derive(PartialEq, Eq)]
struct Expiry(u64, String); // (expires_at, key) — min-heap via Reverse ordering

impl Ord for Expiry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // BinaryHeap is a max-heap; invert for min-heap semantics.
        other.0.cmp(&self.0).then_with(|| other.1.cmp(&self.1))
    }
}
impl PartialOrd for Expiry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

pub struct ScratchpadCell {
    pub id: CellId,
    entries: BTreeMap<String, (Cbor, u64)>, // key -> (value, expires_at)
    expiry: BinaryHeap<Expiry>,
    pub default_ttl: u64, // logical ticks
}

impl ScratchpadCell {
    pub fn new(id: CellId) -> Self {
        ScratchpadCell {
            id,
            entries: BTreeMap::new(),
            expiry: BinaryHeap::new(),
            default_ttl: 100_000,
        }
    }

    /// Evict everything whose expiry <= now. Deterministic given logical time.
    pub fn sweep(&mut self, now: u64) -> u64 {
        let mut evicted = 0;
        while let Some(top) = self.expiry.peek() {
            if top.0 > now {
                break;
            }
            let Expiry(exp, key) = self.expiry.pop().unwrap();
            // Only evict if the live entry still carries this expiry (it may
            // have been overwritten with a later TTL).
            if let Some((_, live_exp)) = self.entries.get(&key) {
                if *live_exp == exp {
                    self.entries.remove(&key);
                    evicted += 1;
                }
            }
        }
        evicted
    }

    pub fn put(&mut self, now: u64, key: String, value: Cbor, ttl: Option<u64>) {
        let expires = now + ttl.unwrap_or(self.default_ttl);
        self.entries.insert(key.clone(), (value, expires));
        self.expiry.push(Expiry(expires, key));
    }

    pub fn get(&mut self, now: u64, key: &str) -> Option<&Cbor> {
        self.sweep(now);
        self.entries.get(key).map(|(v, _)| v)
    }
}

impl CellBehavior for ScratchpadCell {
    fn cell_id(&self) -> CellId {
        self.id
    }
    fn cell_type(&self) -> CellType {
        CellType::Scratchpad
    }
    fn schema_id(&self) -> SchemaId {
        SchemaId::new("scratchpad.v1")
    }

    fn on_query(&self, now: u64, query: &Cbor) -> UcResult<Cbor> {
        // Read path can't mutate; filter by expiry instead of sweeping.
        let key = query.req_str("key")?;
        match self.entries.get(&key) {
            Some((v, exp)) if *exp > now => Ok(Cbor::map(vec![("value", v.clone())])),
            _ => Err(UcError::not_found(format!("scratchpad key {key}"))),
        }
    }

    fn on_update(&mut self, now: u64, update: &Cbor) -> UcResult<Cbor> {
        self.sweep(now);
        let key = update.req_str("key")?;
        let value = update
            .get("value")
            .cloned()
            .ok_or_else(|| UcError::schema("scratchpad: missing value"))?;
        let ttl = update.opt_u64("ttl");
        self.put(now, key.clone(), value, ttl);
        Ok(Cbor::map(vec![("stored", Cbor::t(key))]))
    }

    fn snapshot_state(&self) -> Cbor {
        let items: Vec<Cbor> = self
            .entries
            .iter()
            .map(|(k, (v, exp))| {
                Cbor::map(vec![
                    ("key", Cbor::t(k.clone())),
                    ("value", v.clone()),
                    ("expires_at", Cbor::U64(*exp)),
                ])
            })
            .collect();
        Cbor::map(vec![("entries", Cbor::Array(items))])
    }

    fn restore_state(&mut self, state: &Cbor) -> UcResult<()> {
        self.entries.clear();
        self.expiry.clear();
        if let Some(arr) = state.get("entries").and_then(|v| v.as_array()) {
            for item in arr {
                let key = item.req_str("key")?;
                let value = item.get("value").cloned().unwrap_or(Cbor::Null);
                let exp = item.opt_u64("expires_at").unwrap_or(0);
                self.entries.insert(key.clone(), (value, exp));
                self.expiry.push(Expiry(exp, key));
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// PlaybookCell — named procedures (§2.4)
// ---------------------------------------------------------------------------

pub struct PlaybookCell {
    pub id: CellId,
    playbooks: BTreeMap<String, (u64, Cbor)>, // name -> (version, body)
}

impl PlaybookCell {
    pub fn new(id: CellId) -> Self {
        PlaybookCell {
            id,
            playbooks: BTreeMap::new(),
        }
    }
}

impl CellBehavior for PlaybookCell {
    fn cell_id(&self) -> CellId {
        self.id
    }
    fn cell_type(&self) -> CellType {
        CellType::Playbook
    }
    fn schema_id(&self) -> SchemaId {
        SchemaId::new("playbook.v1")
    }

    fn on_query(&self, _at: u64, query: &Cbor) -> UcResult<Cbor> {
        let name = query.req_str("name")?;
        self.playbooks
            .get(&name)
            .map(|(v, body)| {
                Cbor::map(vec![
                    ("name", Cbor::t(name.clone())),
                    ("version", Cbor::U64(*v)),
                    ("body", body.clone()),
                ])
            })
            .ok_or_else(|| UcError::not_found(format!("playbook {name}")))
    }

    fn on_update(&mut self, _at: u64, update: &Cbor) -> UcResult<Cbor> {
        let name = update.req_str("name")?;
        let body = update
            .get("body")
            .cloned()
            .ok_or_else(|| UcError::schema("playbook: missing body"))?;
        let version = self.playbooks.get(&name).map(|(v, _)| v + 1).unwrap_or(1);
        self.playbooks.insert(name.clone(), (version, body));
        Ok(Cbor::map(vec![
            ("name", Cbor::t(name)),
            ("version", Cbor::U64(version)),
        ]))
    }

    fn snapshot_state(&self) -> Cbor {
        let items: Vec<Cbor> = self
            .playbooks
            .iter()
            .map(|(name, (v, body))| {
                Cbor::map(vec![
                    ("name", Cbor::t(name.clone())),
                    ("version", Cbor::U64(*v)),
                    ("body", body.clone()),
                ])
            })
            .collect();
        Cbor::map(vec![("playbooks", Cbor::Array(items))])
    }

    fn restore_state(&mut self, state: &Cbor) -> UcResult<()> {
        self.playbooks.clear();
        if let Some(arr) = state.get("playbooks").and_then(|v| v.as_array()) {
            for item in arr {
                self.playbooks.insert(
                    item.req_str("name")?,
                    (
                        item.opt_u64("version").unwrap_or(1),
                        item.get("body").cloned().unwrap_or(Cbor::Null),
                    ),
                );
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// BlobCell — CAS-backed metadata (§2.6). Bytes live in the CasStore; this
// cell owns handle→(sha, meta) so state snapshots stay small.
// ---------------------------------------------------------------------------

pub struct BlobCell {
    pub id: CellId,
    blobs: BTreeMap<String, ([u8; 32], u64, String)>, // handle -> (sha, size, media)
}

impl BlobCell {
    pub fn new(id: CellId) -> Self {
        BlobCell {
            id,
            blobs: BTreeMap::new(),
        }
    }

    pub fn register(&mut self, sha: [u8; 32], size: u64, media: String) -> String {
        let handle = format!("blob/{}", crate::core::crypto::hex(&sha));
        self.blobs.insert(handle.clone(), (sha, size, media));
        handle
    }

    pub fn lookup(&self, handle: &str) -> Option<&([u8; 32], u64, String)> {
        self.blobs.get(handle)
    }

    pub fn exists(&self, handle: &str) -> bool {
        self.blobs.contains_key(handle)
    }

    pub fn iter_hashes(&self) -> impl Iterator<Item = &[u8; 32]> {
        self.blobs.values().map(|(sha, _, _)| sha)
    }
}

impl CellBehavior for BlobCell {
    fn cell_id(&self) -> CellId {
        self.id
    }
    fn cell_type(&self) -> CellType {
        CellType::Blob
    }
    fn schema_id(&self) -> SchemaId {
        SchemaId::new("blob.v1")
    }

    fn on_query(&self, _at: u64, query: &Cbor) -> UcResult<Cbor> {
        let handle = query.req_str("handle")?;
        self.lookup(&handle)
            .map(|(sha, size, media)| {
                Cbor::map(vec![
                    ("handle", Cbor::t(handle.clone())),
                    ("sha256", Cbor::Bytes(sha.to_vec())),
                    ("size", Cbor::U64(*size)),
                    ("media", Cbor::t(media.clone())),
                ])
            })
            .ok_or_else(|| UcError::not_found(format!("blob {handle}")))
    }

    fn on_update(&mut self, _at: u64, update: &Cbor) -> UcResult<Cbor> {
        let sha_bytes = update
            .get("sha256")
            .and_then(|v| v.as_bytes())
            .ok_or_else(|| UcError::schema("blob: missing sha256"))?;
        if sha_bytes.len() != 32 {
            return Err(UcError::schema("blob: sha256 must be 32 bytes"));
        }
        let mut sha = [0u8; 32];
        sha.copy_from_slice(sha_bytes);
        let size = update.opt_u64("size").unwrap_or(0);
        let media = update
            .opt_str("media")
            .unwrap_or_else(|| "application/octet-stream".into());
        let handle = self.register(sha, size, media);
        Ok(Cbor::map(vec![("handle", Cbor::t(handle))]))
    }

    fn snapshot_state(&self) -> Cbor {
        let items: Vec<Cbor> = self
            .blobs
            .iter()
            .map(|(h, (sha, size, media))| {
                Cbor::map(vec![
                    ("handle", Cbor::t(h.clone())),
                    ("sha256", Cbor::Bytes(sha.to_vec())),
                    ("size", Cbor::U64(*size)),
                    ("media", Cbor::t(media.clone())),
                ])
            })
            .collect();
        Cbor::map(vec![("blobs", Cbor::Array(items))])
    }

    fn restore_state(&mut self, state: &Cbor) -> UcResult<()> {
        self.blobs.clear();
        if let Some(arr) = state.get("blobs").and_then(|v| v.as_array()) {
            for item in arr {
                let handle = item.req_str("handle")?;
                let sha_bytes = item
                    .get("sha256")
                    .and_then(|v| v.as_bytes())
                    .ok_or_else(|| UcError::schema("blob snapshot: missing sha256"))?;
                let mut sha = [0u8; 32];
                sha.copy_from_slice(&sha_bytes[..32.min(sha_bytes.len())]);
                self.blobs.insert(
                    handle,
                    (
                        sha,
                        item.opt_u64("size").unwrap_or(0),
                        item.opt_str("media").unwrap_or_default(),
                    ),
                );
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// CacheCell — in-memory LRU (§2.7); anchor-exempt, never persisted verbatim
// ---------------------------------------------------------------------------

pub struct CacheCell {
    pub id: CellId,
    map: HashMap<String, (Cbor, u64)>, // key -> (value, last_used)
    pub capacity: usize,
}

impl CacheCell {
    pub fn new(id: CellId) -> Self {
        CacheCell {
            id,
            map: HashMap::new(),
            capacity: 10_000,
        }
    }
}

impl CellBehavior for CacheCell {
    fn cell_id(&self) -> CellId {
        self.id
    }
    fn cell_type(&self) -> CellType {
        CellType::Cache
    }
    fn schema_id(&self) -> SchemaId {
        SchemaId::new("cache.v1")
    }

    fn on_query(&self, _at: u64, query: &Cbor) -> UcResult<Cbor> {
        let key = query.req_str("key")?;
        self.map
            .get(&key)
            .map(|(v, _)| Cbor::map(vec![("value", v.clone())]))
            .ok_or_else(|| UcError::not_found(format!("cache key {key}")))
    }

    fn on_update(&mut self, now: u64, update: &Cbor) -> UcResult<Cbor> {
        let key = update.req_str("key")?;
        let value = update
            .get("value")
            .cloned()
            .ok_or_else(|| UcError::schema("cache: missing value"))?;
        if self.map.len() >= self.capacity && !self.map.contains_key(&key) {
            // Deterministic eviction: oldest last_used, ties by key order.
            if let Some(victim) = self
                .map
                .iter()
                .min_by(|a, b| a.1 .1.cmp(&b.1 .1).then_with(|| a.0.cmp(b.0)))
                .map(|(k, _)| k.clone())
            {
                self.map.remove(&victim);
            }
        }
        self.map.insert(key.clone(), (value, now));
        Ok(Cbor::map(vec![("stored", Cbor::t(key))]))
    }

    /// Cache contents are ephemeral by design — the snapshot is empty
    /// (CellTaxonomy.md §2.7: cache never persisted).
    fn snapshot_state(&self) -> Cbor {
        Cbor::map(vec![("ephemeral", Cbor::Bool(true))])
    }

    fn restore_state(&mut self, _state: &Cbor) -> UcResult<()> {
        self.map.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fact_supersession_and_sp_index() {
        let mut fc = FactCell::new(CellId(2));
        fc.insert(Fact {
            handle: "fact/A".into(),
            subject: "svc.auth".into(),
            predicate: "owner".into(),
            object: "team-x".into(),
            confidence: None,
            written_at: 1,
            superseded_by: None,
            supersedes: None,
            anchor: "Architecture.md§4".into(),
        });
        fc.insert(Fact {
            handle: "fact/B".into(),
            subject: "svc.auth".into(),
            predicate: "owner".into(),
            object: "team-y".into(),
            confidence: None,
            written_at: 2,
            superseded_by: None,
            supersedes: None,
            anchor: "Architecture.md§4".into(),
        });
        // Two active facts for the same (s,p) — the drift condition.
        assert_eq!(fc.active_for_sp("svc.auth", "owner").len(), 2);
        fc.supersede("fact/A", "fact/B").unwrap();
        let active = fc.active_for_sp("svc.auth", "owner");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].handle, "fact/B");
        assert_eq!(active[0].supersedes.as_deref(), Some("fact/A"));
        // Double supersede rejected.
        assert!(fc.supersede("fact/A", "fact/B").is_err());
        // Snapshot round-trip preserves chains.
        let snap = fc.snapshot_state();
        let mut fc2 = FactCell::new(CellId(2));
        fc2.restore_state(&snap).unwrap();
        assert_eq!(
            fc2.get("fact/A").unwrap().superseded_by.as_deref(),
            Some("fact/B")
        );
        assert_eq!(snap.encode(), fc2.snapshot_state().encode());
    }

    #[test]
    fn scratchpad_ttl_expiry() {
        let mut sp = ScratchpadCell::new(CellId(5));
        sp.put(10, "k1".into(), Cbor::t("v1"), Some(5)); // expires at 15
        sp.put(10, "k2".into(), Cbor::t("v2"), Some(100)); // expires at 110
        assert!(sp.get(14, "k1").is_some());
        assert!(sp.get(15, "k1").is_none()); // expired exactly at boundary
        assert!(sp.get(15, "k2").is_some());
        // Overwrite extends life.
        sp.put(16, "k2".into(), Cbor::t("v2b"), Some(5)); // expires 21
        assert!(sp.get(20, "k2").is_some());
        assert!(sp.get(21, "k2").is_none());
    }

    #[test]
    fn timeline_ring_and_order() {
        let mut tl = TimelineCell::new(CellId(3));
        for i in 0..5 {
            tl.append(i, "node.written", Cbor::U64(i));
        }
        let tail = tl.tail("node.written", 3);
        assert_eq!(tail.len(), 3);
        assert_eq!(tail[0].0, 4); // newest first
    }

    #[test]
    fn cache_lru_eviction_deterministic() {
        let mut cc = CacheCell::new(CellId(10));
        cc.capacity = 2;
        cc.on_update(
            1,
            &Cbor::map(vec![("key", Cbor::t("a")), ("value", Cbor::U64(1))]),
        )
        .unwrap();
        cc.on_update(
            2,
            &Cbor::map(vec![("key", Cbor::t("b")), ("value", Cbor::U64(2))]),
        )
        .unwrap();
        cc.on_update(
            3,
            &Cbor::map(vec![("key", Cbor::t("c")), ("value", Cbor::U64(3))]),
        )
        .unwrap();
        // "a" (oldest) evicted.
        assert!(cc
            .on_query(4, &Cbor::map(vec![("key", Cbor::t("a"))]))
            .is_err());
        assert!(cc
            .on_query(4, &Cbor::map(vec![("key", Cbor::t("b"))]))
            .is_ok());
    }
}
