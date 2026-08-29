//! Index cells — SPEC-DERIVED-§2.8–2.12 (CellTaxonomy.md),
//! SPEC-DERIVED-§3 (EmbeddingReranker.md).
//!
//! VectorCell wraps a seeded HNSW graph (deterministic level assignment from
//! the boot seed, so replay reconstructs an identical graph). Embeddings in
//! v0 come from a deterministic feature-hashing embedder (768-dim) — the
//! [`Embedder`] trait is the seam for a real model later. BM25 uses standard
//! k1=1.2, b=0.75. GraphCell keeps adjacency in sorted vecs (CSR-flavored).
//! RerankerCell blends lexical overlap with cosine similarity.

use super::{CellBehavior, CellType};
use crate::core::cbor::Cbor;
use crate::core::ulid::DetRng;
use crate::core::{fnv1a64, CellId, SchemaId, UcError, UcResult};
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap};

// ---------------------------------------------------------------------------
// Embedder
// ---------------------------------------------------------------------------

pub trait Embedder: Send + Sync {
    fn dim(&self) -> usize;
    fn embed(&self, text: &str) -> Vec<f32>;
}

/// Deterministic feature-hashing embedder: unigrams + bigrams hashed into a
/// fixed-dim signed bucket space, L2-normalized. No model weights, fully
/// reproducible, adequate for structural tests and coarse recall. Dual-dim
/// support (768 default, 1536 optional) per CellTaxonomy.md §2.8.
pub struct HashEmbedder {
    dim: usize,
}

impl HashEmbedder {
    pub fn new(dim: usize) -> Self {
        HashEmbedder { dim }
    }
}

impl Embedder for HashEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0f32; self.dim];
        let tokens: Vec<String> = tokenize(text);
        let bump = |s: &str, weight: f32, v: &mut Vec<f32>| {
            let h = fnv1a64(s.as_bytes());
            let idx = (h % self.dim as u64) as usize;
            let sign = if (h >> 63) & 1 == 0 { 1.0 } else { -1.0 };
            v[idx] += sign * weight;
        };
        for t in &tokens {
            bump(t, 1.0, &mut v);
        }
        for w in tokens.windows(2) {
            bump(&format!("{}_{}", w[0], w[1]), 0.5, &mut v);
        }
        l2_normalize(&mut v);
        v
    }
}

pub fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    // Inputs are L2-normalized, so dot == cosine.
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

// ---------------------------------------------------------------------------
// HNSW
// ---------------------------------------------------------------------------

/// f32 wrapper with total ordering for heaps (NaN never produced by cosine
/// over normalized vectors, but panics are still unacceptable in the router).
#[derive(Clone, Copy, PartialEq)]
pub struct OrdF32(pub f32);
impl Eq for OrdF32 {}
impl PartialOrd for OrdF32 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for OrdF32 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0
            .partial_cmp(&other.0)
            .unwrap_or(std::cmp::Ordering::Equal)
    }
}

const HNSW_M: usize = 16; // max neighbors per node per layer
const HNSW_M0: usize = 32; // layer-0 max neighbors
const HNSW_EF_CONSTRUCTION: usize = 64;
const HNSW_EF_SEARCH: usize = 48;

pub struct Hnsw {
    /// node -> per-layer sorted neighbor lists (layer 0 first).
    layers: Vec<Vec<Vec<u32>>>,
    vectors: Vec<Vec<f32>>,
    node_levels: Vec<usize>,
    entry: Option<u32>,
    max_level: usize,
    rng: DetRng,
}

impl Hnsw {
    pub fn new(seed: u64) -> Self {
        Hnsw {
            layers: Vec::new(),
            vectors: Vec::new(),
            node_levels: Vec::new(),
            entry: None,
            max_level: 0,
            rng: DetRng::new(seed),
        }
    }

    pub fn len(&self) -> usize {
        self.vectors.len()
    }
    pub fn is_empty(&self) -> bool {
        self.vectors.is_empty()
    }

    fn random_level(&mut self) -> usize {
        // Geometric with p=1/e approximated via bit tricks on the
        // deterministic stream: count trailing zero *pairs*.
        let r = self.rng.next_u64();
        let mut level = 0usize;
        let mut v = r;
        while v & 0b11 == 0 && level < 16 {
            level += 1;
            v >>= 2;
        }
        level
    }

    pub fn insert(&mut self, vector: Vec<f32>) -> u32 {
        let id = self.vectors.len() as u32;
        let level = self.random_level();
        self.vectors.push(vector);
        self.node_levels.push(level);
        self.layers.push(vec![Vec::new(); level + 1]);

        let Some(mut cur) = self.entry else {
            self.entry = Some(id);
            self.max_level = level;
            return id;
        };

        // Greedy descend from top to level+1.
        let q = self.vectors[id as usize].clone();
        let mut lvl = self.max_level;
        while lvl > level {
            cur = self.greedy_closest(&q, cur, lvl);
            if lvl == 0 {
                break;
            }
            lvl -= 1;
        }

        // Insert with ef_construction from min(level, max_level) down to 0.
        let top = level.min(self.max_level);
        let mut l = top;
        loop {
            let candidates = self.search_layer(&q, cur, HNSW_EF_CONSTRUCTION, l);
            let m = if l == 0 { HNSW_M0 } else { HNSW_M };
            let selected: Vec<u32> = candidates.iter().take(m).map(|(_, n)| *n).collect();
            for &n in &selected {
                self.layers[id as usize][l].push(n);
                let nl = &mut self.layers[n as usize][l];
                nl.push(id);
                if nl.len() > m {
                    // Prune neighbor's list to its m closest.
                    let nvec = self.vectors[n as usize].clone();
                    let mut scored: Vec<(OrdF32, u32)> = self.layers[n as usize][l]
                        .iter()
                        .map(|&x| (OrdF32(cosine(&nvec, &self.vectors[x as usize])), x))
                        .collect();
                    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
                    scored.truncate(m);
                    self.layers[n as usize][l] = scored.into_iter().map(|(_, x)| x).collect();
                }
            }
            if let Some((_, best)) = candidates.first() {
                cur = *best;
            }
            if l == 0 {
                break;
            }
            l -= 1;
        }

        if level > self.max_level {
            self.max_level = level;
            self.entry = Some(id);
        }
        id
    }

    fn greedy_closest(&self, q: &[f32], start: u32, layer: usize) -> u32 {
        let mut cur = start;
        let mut cur_sim = cosine(q, &self.vectors[cur as usize]);
        loop {
            let mut improved = false;
            if layer < self.layers[cur as usize].len() {
                for &n in &self.layers[cur as usize][layer] {
                    let sim = cosine(q, &self.vectors[n as usize]);
                    if sim > cur_sim {
                        cur = n;
                        cur_sim = sim;
                        improved = true;
                    }
                }
            }
            if !improved {
                return cur;
            }
        }
    }

    /// Best-first search on one layer; returns (similarity desc, node),
    /// deterministic tie-break by node id.
    fn search_layer(&self, q: &[f32], entry: u32, ef: usize, layer: usize) -> Vec<(OrdF32, u32)> {
        let mut visited: BTreeSet<u32> = BTreeSet::new();
        visited.insert(entry);
        let entry_sim = OrdF32(cosine(q, &self.vectors[entry as usize]));
        // candidates: max-heap by similarity.
        let mut candidates: BinaryHeap<(OrdF32, std::cmp::Reverse<u32>)> = BinaryHeap::new();
        candidates.push((entry_sim, std::cmp::Reverse(entry)));
        // results: min-heap (keep best ef) via Reverse.
        let mut results: BinaryHeap<std::cmp::Reverse<(OrdF32, std::cmp::Reverse<u32>)>> =
            BinaryHeap::new();
        results.push(std::cmp::Reverse((entry_sim, std::cmp::Reverse(entry))));

        while let Some((sim, std::cmp::Reverse(node))) = candidates.pop() {
            let worst = results.peek().map(|r| r.0 .0).unwrap_or(OrdF32(f32::MIN));
            if results.len() >= ef && sim < worst {
                break;
            }
            if layer < self.layers[node as usize].len() {
                for &n in &self.layers[node as usize][layer] {
                    if visited.insert(n) {
                        let nsim = OrdF32(cosine(q, &self.vectors[n as usize]));
                        let worst = results.peek().map(|r| r.0 .0).unwrap_or(OrdF32(f32::MIN));
                        if results.len() < ef || nsim > worst {
                            candidates.push((nsim, std::cmp::Reverse(n)));
                            results.push(std::cmp::Reverse((nsim, std::cmp::Reverse(n))));
                            if results.len() > ef {
                                results.pop();
                            }
                        }
                    }
                }
            }
        }
        let mut out: Vec<(OrdF32, u32)> = results
            .into_iter()
            .map(|std::cmp::Reverse((s, std::cmp::Reverse(n)))| (s, n))
            .collect();
        out.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        out
    }

    pub fn search(&self, q: &[f32], k: usize) -> Vec<(f32, u32)> {
        let Some(mut cur) = self.entry else {
            return Vec::new();
        };
        let mut lvl = self.max_level;
        while lvl > 0 {
            cur = self.greedy_closest(q, cur, lvl);
            lvl -= 1;
        }
        self.search_layer(q, cur, HNSW_EF_SEARCH.max(k), 0)
            .into_iter()
            .take(k)
            .map(|(s, n)| (s.0, n))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// VectorCell
// ---------------------------------------------------------------------------

pub struct VectorCell {
    pub id: CellId,
    index: Hnsw,
    handles: Vec<String>, // node id -> handle
    texts: Vec<String>,   // for snapshot rebuild
    embedder: HashEmbedder,
    seed: u64,
}

impl VectorCell {
    pub fn new(id: CellId, dim: usize, seed: u64) -> Self {
        VectorCell {
            id,
            index: Hnsw::new(seed),
            handles: Vec::new(),
            texts: Vec::new(),
            embedder: HashEmbedder::new(dim),
            seed,
        }
    }

    pub fn add(&mut self, handle: String, text: &str) -> u32 {
        let vec = self.embedder.embed(text);
        let node = self.index.insert(vec);
        self.handles.push(handle);
        self.texts.push(text.to_string());
        node
    }

    pub fn query(&self, text: &str, k: usize) -> Vec<(f32, String)> {
        let q = self.embedder.embed(text);
        self.index
            .search(&q, k)
            .into_iter()
            .map(|(s, n)| (s, self.handles[n as usize].clone()))
            .collect()
    }
}

impl CellBehavior for VectorCell {
    fn cell_id(&self) -> CellId {
        self.id
    }
    fn cell_type(&self) -> CellType {
        CellType::Vector
    }
    fn schema_id(&self) -> SchemaId {
        SchemaId::new(format!("vector.hnsw.{}d.v1", self.embedder.dim()))
    }

    fn on_query(&self, _at: u64, query: &Cbor) -> UcResult<Cbor> {
        let text = query.req_str("text")?;
        let k = query.opt_u64("k").unwrap_or(8) as usize;
        let hits: Vec<Cbor> = self
            .query(&text, k)
            .into_iter()
            .map(|(score, handle)| {
                Cbor::map(vec![
                    ("handle", Cbor::t(handle)),
                    ("score", Cbor::F64(score as f64)),
                ])
            })
            .collect();
        Ok(Cbor::map(vec![("hits", Cbor::Array(hits))]))
    }

    fn on_update(&mut self, _at: u64, update: &Cbor) -> UcResult<Cbor> {
        let handle = update.req_str("handle")?;
        let text = update.req_str("text")?;
        let node = self.add(handle.clone(), &text);
        Ok(Cbor::map(vec![
            ("indexed", Cbor::t(handle)),
            ("node", Cbor::U64(node as u64)),
        ]))
    }

    /// Snapshot stores (handle, text) pairs + the construction seed; restore
    /// rebuilds the graph by replaying inserts in order — deterministic, so
    /// the rebuilt graph is identical (VectorCell §2.8 replay guarantee).
    fn snapshot_state(&self) -> Cbor {
        let items: Vec<Cbor> = self
            .handles
            .iter()
            .zip(self.texts.iter())
            .map(|(h, t)| {
                Cbor::map(vec![
                    ("handle", Cbor::t(h.clone())),
                    ("text", Cbor::t(t.clone())),
                ])
            })
            .collect();
        Cbor::map(vec![
            ("dim", Cbor::U64(self.embedder.dim() as u64)),
            ("seed", Cbor::U64(self.seed)),
            ("items", Cbor::Array(items)),
        ])
    }

    fn restore_state(&mut self, state: &Cbor) -> UcResult<()> {
        let dim = state.opt_u64("dim").unwrap_or(768) as usize;
        let seed = state.opt_u64("seed").unwrap_or(self.seed);
        self.embedder = HashEmbedder::new(dim);
        self.seed = seed;
        self.index = Hnsw::new(seed);
        self.handles.clear();
        self.texts.clear();
        if let Some(arr) = state.get("items").and_then(|v| v.as_array()) {
            for item in arr {
                let h = item.req_str("handle")?;
                let t = item.req_str("text")?;
                self.add(h, &t);
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// BM25Cell
// ---------------------------------------------------------------------------

const BM25_K1: f32 = 1.2;
const BM25_B: f32 = 0.75;

pub struct Bm25Cell {
    pub id: CellId,
    docs: Vec<(String, Vec<String>)>, // (handle, tokens)
    doc_freq: HashMap<String, u32>,
    total_len: u64,
}

impl Bm25Cell {
    pub fn new(id: CellId) -> Self {
        Bm25Cell {
            id,
            docs: Vec::new(),
            doc_freq: HashMap::new(),
            total_len: 0,
        }
    }

    pub fn add(&mut self, handle: String, text: &str) {
        let tokens = tokenize(text);
        let uniq: BTreeSet<&String> = tokens.iter().collect();
        for t in uniq {
            *self.doc_freq.entry(t.clone()).or_insert(0) += 1;
        }
        self.total_len += tokens.len() as u64;
        self.docs.push((handle, tokens));
    }

    pub fn search(&self, query: &str, k: usize) -> Vec<(f32, String)> {
        if self.docs.is_empty() {
            return Vec::new();
        }
        let q_tokens = tokenize(query);
        let n = self.docs.len() as f32;
        let avg_len = self.total_len as f32 / n;
        let mut scored: Vec<(OrdF32, usize)> = Vec::new();
        for (i, (_, tokens)) in self.docs.iter().enumerate() {
            let mut tf: HashMap<&String, u32> = HashMap::new();
            for t in tokens {
                *tf.entry(t).or_insert(0) += 1;
            }
            let dl = tokens.len() as f32;
            let mut score = 0f32;
            for q in &q_tokens {
                let Some(&f) = tf.get(q) else { continue };
                let df = self.doc_freq.get(q).copied().unwrap_or(0) as f32;
                let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
                let f = f as f32;
                score += idf * (f * (BM25_K1 + 1.0))
                    / (f + BM25_K1 * (1.0 - BM25_B + BM25_B * dl / avg_len));
            }
            if score > 0.0 {
                scored.push((OrdF32(score), i));
            }
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        scored
            .into_iter()
            .take(k)
            .map(|(s, i)| (s.0, self.docs[i].0.clone()))
            .collect()
    }
}

impl CellBehavior for Bm25Cell {
    fn cell_id(&self) -> CellId {
        self.id
    }
    fn cell_type(&self) -> CellType {
        CellType::Bm25
    }
    fn schema_id(&self) -> SchemaId {
        SchemaId::new("bm25.v1")
    }

    fn on_query(&self, _at: u64, query: &Cbor) -> UcResult<Cbor> {
        let text = query.req_str("text")?;
        let k = query.opt_u64("k").unwrap_or(8) as usize;
        let hits: Vec<Cbor> = self
            .search(&text, k)
            .into_iter()
            .map(|(score, handle)| {
                Cbor::map(vec![
                    ("handle", Cbor::t(handle)),
                    ("score", Cbor::F64(score as f64)),
                ])
            })
            .collect();
        Ok(Cbor::map(vec![("hits", Cbor::Array(hits))]))
    }

    fn on_update(&mut self, _at: u64, update: &Cbor) -> UcResult<Cbor> {
        let handle = update.req_str("handle")?;
        let text = update.req_str("text")?;
        self.add(handle.clone(), &text);
        Ok(Cbor::map(vec![("indexed", Cbor::t(handle))]))
    }

    fn snapshot_state(&self) -> Cbor {
        let items: Vec<Cbor> = self
            .docs
            .iter()
            .map(|(h, tokens)| {
                Cbor::map(vec![
                    ("handle", Cbor::t(h.clone())),
                    ("text", Cbor::t(tokens.join(" "))),
                ])
            })
            .collect();
        Cbor::map(vec![("items", Cbor::Array(items))])
    }

    fn restore_state(&mut self, state: &Cbor) -> UcResult<()> {
        self.docs.clear();
        self.doc_freq.clear();
        self.total_len = 0;
        if let Some(arr) = state.get("items").and_then(|v| v.as_array()) {
            for item in arr {
                self.add(item.req_str("handle")?, &item.req_str("text")?);
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// GraphCell — adjacency (CSR-flavored: sorted neighbor vecs) (§2.9)
// ---------------------------------------------------------------------------

pub struct GraphCell {
    pub id: CellId,
    edges: BTreeMap<String, BTreeMap<String, Vec<String>>>, // src -> label -> [dst]
}

impl GraphCell {
    pub fn new(id: CellId) -> Self {
        GraphCell {
            id,
            edges: BTreeMap::new(),
        }
    }

    pub fn add_edge(&mut self, src: &str, label: &str, dst: &str) {
        let dsts = self
            .edges
            .entry(src.to_string())
            .or_default()
            .entry(label.to_string())
            .or_default();
        if let Err(pos) = dsts.binary_search(&dst.to_string()) {
            dsts.insert(pos, dst.to_string());
        }
    }

    pub fn neighbors(&self, src: &str, label: Option<&str>) -> Vec<String> {
        let Some(by_label) = self.edges.get(src) else {
            return Vec::new();
        };
        match label {
            Some(l) => by_label.get(l).cloned().unwrap_or_default(),
            None => {
                let mut out = Vec::new();
                for dsts in by_label.values() {
                    out.extend(dsts.iter().cloned());
                }
                out.sort();
                out.dedup();
                out
            }
        }
    }
}

impl CellBehavior for GraphCell {
    fn cell_id(&self) -> CellId {
        self.id
    }
    fn cell_type(&self) -> CellType {
        CellType::Graph
    }
    fn schema_id(&self) -> SchemaId {
        SchemaId::new("graph.csr.v1")
    }

    fn on_query(&self, _at: u64, query: &Cbor) -> UcResult<Cbor> {
        let src = query.req_str("src")?;
        let label = query.opt_str("label");
        let ns: Vec<Cbor> = self
            .neighbors(&src, label.as_deref())
            .into_iter()
            .map(Cbor::t)
            .collect();
        Ok(Cbor::map(vec![("neighbors", Cbor::Array(ns))]))
    }

    fn on_update(&mut self, _at: u64, update: &Cbor) -> UcResult<Cbor> {
        let src = update.req_str("src")?;
        let label = update.req_str("label")?;
        let dst = update.req_str("dst")?;
        self.add_edge(&src, &label, &dst);
        Ok(Cbor::map(vec![("edge_added", Cbor::Bool(true))]))
    }

    fn snapshot_state(&self) -> Cbor {
        let mut items = Vec::new();
        for (src, by_label) in &self.edges {
            for (label, dsts) in by_label {
                for dst in dsts {
                    items.push(Cbor::map(vec![
                        ("src", Cbor::t(src.clone())),
                        ("label", Cbor::t(label.clone())),
                        ("dst", Cbor::t(dst.clone())),
                    ]));
                }
            }
        }
        Cbor::map(vec![("edges", Cbor::Array(items))])
    }

    fn restore_state(&mut self, state: &Cbor) -> UcResult<()> {
        self.edges.clear();
        if let Some(arr) = state.get("edges").and_then(|v| v.as_array()) {
            for item in arr {
                self.add_edge(
                    &item.req_str("src")?,
                    &item.req_str("label")?,
                    &item.req_str("dst")?,
                );
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// RerankerCell — deterministic lexical + cosine blend (EmbeddingReranker.md §3)
// ---------------------------------------------------------------------------

pub struct RerankerCell {
    pub id: CellId,
    embedder: HashEmbedder,
    /// blend weight for cosine vs lexical (spec default 0.6/0.4).
    pub alpha: f32,
}

impl RerankerCell {
    pub fn new(id: CellId, dim: usize) -> Self {
        RerankerCell {
            id,
            embedder: HashEmbedder::new(dim),
            alpha: 0.6,
        }
    }

    /// Rerank (handle, text) candidates against a query; returns sorted
    /// (score desc, handle), tie-broken by handle.
    pub fn rerank(&self, query: &str, candidates: &[(String, String)]) -> Vec<(f32, String)> {
        let q_vec = self.embedder.embed(query);
        let q_tokens: BTreeSet<String> = tokenize(query).into_iter().collect();
        let mut scored: Vec<(OrdF32, String)> = candidates
            .iter()
            .map(|(handle, text)| {
                let c = cosine(&q_vec, &self.embedder.embed(text));
                let t_tokens: BTreeSet<String> = tokenize(text).into_iter().collect();
                let inter = q_tokens.intersection(&t_tokens).count() as f32;
                let union = q_tokens.union(&t_tokens).count().max(1) as f32;
                let lex = inter / union; // Jaccard
                (
                    OrdF32(self.alpha * c + (1.0 - self.alpha) * lex),
                    handle.clone(),
                )
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        scored.into_iter().map(|(s, h)| (s.0, h)).collect()
    }
}

impl CellBehavior for RerankerCell {
    fn cell_id(&self) -> CellId {
        self.id
    }
    fn cell_type(&self) -> CellType {
        CellType::Reranker
    }
    fn schema_id(&self) -> SchemaId {
        SchemaId::new("reranker.blend.v1")
    }

    fn on_query(&self, _at: u64, query: &Cbor) -> UcResult<Cbor> {
        let q = query.req_str("query")?;
        let mut candidates = Vec::new();
        if let Some(arr) = query.get("candidates").and_then(|v| v.as_array()) {
            for item in arr {
                candidates.push((item.req_str("handle")?, item.req_str("text")?));
            }
        }
        let ranked: Vec<Cbor> = self
            .rerank(&q, &candidates)
            .into_iter()
            .map(|(score, handle)| {
                Cbor::map(vec![
                    ("handle", Cbor::t(handle)),
                    ("score", Cbor::F64(score as f64)),
                ])
            })
            .collect();
        Ok(Cbor::map(vec![("ranked", Cbor::Array(ranked))]))
    }

    fn on_update(&mut self, _at: u64, _update: &Cbor) -> UcResult<Cbor> {
        Err(UcError::schema("reranker is query-only"))
    }

    fn snapshot_state(&self) -> Cbor {
        Cbor::map(vec![("alpha", Cbor::F64(self.alpha as f64))])
    }

    fn restore_state(&mut self, state: &Cbor) -> UcResult<()> {
        if let Some(a) = state.get("alpha").and_then(|v| v.as_f64()) {
            self.alpha = a as f32;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedder_deterministic_and_normalized() {
        let e = HashEmbedder::new(768);
        let a = e.embed("the router pre-validates every write");
        let b = e.embed("the router pre-validates every write");
        assert_eq!(a, b);
        let norm: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4);
        assert!(cosine(&a, &b) > 0.999);
    }

    #[test]
    fn hnsw_finds_nearest() {
        let e = HashEmbedder::new(256);
        let mut h = Hnsw::new(42);
        let corpus = [
            "quarantine absorbs failed writes",
            "the librarian summarizes memory into skeletons",
            "the warden audits librarian output for drift",
            "wal frames carry crc32c checksums",
            "capability tokens gate facet hydration",
            "hnsw builds a navigable small world graph",
            "bm25 ranks documents by term frequency",
            "snapshots use copy on write semantics",
        ];
        for text in &corpus {
            h.insert(e.embed(text));
        }
        let hits = h.search(&e.embed("warden audits the librarian"), 3);
        assert!(!hits.is_empty());
        assert_eq!(hits[0].1, 2); // the warden sentence
                                  // Determinism: same seed + insert order => same results.
        let mut h2 = Hnsw::new(42);
        for text in &corpus {
            h2.insert(e.embed(text));
        }
        let hits2 = h2.search(&e.embed("warden audits the librarian"), 3);
        assert_eq!(
            hits.iter().map(|(_, n)| *n).collect::<Vec<_>>(),
            hits2.iter().map(|(_, n)| *n).collect::<Vec<_>>()
        );
    }

    #[test]
    fn vector_cell_snapshot_rebuild_identical() {
        let mut vc = VectorCell::new(CellId(6), 256, 7);
        vc.add("fact/A".into(), "chacha20 stream cipher");
        vc.add("fact/B".into(), "hnsw vector index layers");
        vc.add("fact/C".into(), "canonical cbor sorted keys");
        let snap = vc.snapshot_state();
        let mut vc2 = VectorCell::new(CellId(6), 256, 7);
        vc2.restore_state(&snap).unwrap();
        let q = "vector index";
        assert_eq!(vc.query(q, 2), vc2.query(q, 2));
        assert_eq!(snap.encode(), vc2.snapshot_state().encode());
    }

    #[test]
    fn bm25_ranks_relevant_first() {
        let mut b = Bm25Cell::new(CellId(7));
        b.add("d1".into(), "the quarantine cell absorbs rejected writes");
        b.add("d2".into(), "vector search over embeddings");
        b.add("d3".into(), "quarantine retention and reinjection policy");
        let hits = b.search("quarantine reinjection", 2);
        assert_eq!(hits[0].1, "d3");
        assert_eq!(hits[1].1, "d1");
    }

    #[test]
    fn graph_neighbors_sorted() {
        let mut g = GraphCell::new(CellId(8));
        g.add_edge("fact/A", "supports", "decision/2");
        g.add_edge("fact/A", "supports", "decision/1");
        g.add_edge("fact/A", "contradicts", "fact/B");
        assert_eq!(
            g.neighbors("fact/A", Some("supports")),
            vec!["decision/1".to_string(), "decision/2".to_string()]
        );
        assert_eq!(g.neighbors("fact/A", None).len(), 3);
    }

    #[test]
    fn reranker_prefers_lexical_and_semantic_match() {
        let r = RerankerCell::new(CellId(9), 256);
        let ranked = r.rerank(
            "warden drift audit",
            &[
                ("h1".into(), "grocery list for the weekend".into()),
                ("h2".into(), "the warden audit found semantic drift".into()),
                ("h3".into(), "audit chain verification".into()),
            ],
        );
        assert_eq!(ranked[0].1, "h2");
    }
}
