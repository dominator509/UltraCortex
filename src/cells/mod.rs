//! Cell abstraction + CatalogCell — SPEC-DERIVED-§2 (CellTaxonomy.md).
//!
//! Every cell is a single-writer state machine keyed by [`CellId`]. The
//! Router owns dispatch; cells never call each other directly (I1 —
//! isolation invariant, Architecture.md §4). `on_update` must be
//! deterministic: logical clock in, no wall-clock, no ambient randomness.
//! State snapshots round-trip through canonical CBOR so replay and
//! snapshot-restore converge byte-identically.

pub mod coord;
pub mod index;
pub mod memory;

use crate::core::cbor::Cbor;
use crate::core::{CellId, SchemaId, UcResult};
use std::collections::BTreeMap;

/// Stable cell-type discriminants (CellTaxonomy.md §1 numbering).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CellType {
    // Memory / index / coordination (14)
    Catalog,
    Fact,
    Timeline,
    Playbook,
    Scratchpad,
    Vector,
    Graph,
    Bm25,
    Blob,
    Cache,
    AgentRegistry,
    Proposal,
    Subscription,
    Reranker,
    // Native Trinity (7)
    SpecAnchor,
    DecisionLedger,
    Congruence,
    Gap,
    Quarantine,
    WorkBudget,
    Contract,
    // Curator layer (4)
    Librarian,
    Warden,
    Adjudicator,
    CrossCheckLedger,
}

impl CellType {
    pub fn as_str(&self) -> &'static str {
        match self {
            CellType::Catalog => "Catalog",
            CellType::Fact => "Fact",
            CellType::Timeline => "Timeline",
            CellType::Playbook => "Playbook",
            CellType::Scratchpad => "Scratchpad",
            CellType::Vector => "Vector",
            CellType::Graph => "Graph",
            CellType::Bm25 => "BM25",
            CellType::Blob => "Blob",
            CellType::Cache => "Cache",
            CellType::AgentRegistry => "AgentRegistry",
            CellType::Proposal => "Proposal",
            CellType::Subscription => "Subscription",
            CellType::Reranker => "Reranker",
            CellType::SpecAnchor => "SpecAnchor",
            CellType::DecisionLedger => "DecisionLedger",
            CellType::Congruence => "Congruence",
            CellType::Gap => "Gap",
            CellType::Quarantine => "Quarantine",
            CellType::WorkBudget => "WorkBudget",
            CellType::Contract => "Contract",
            CellType::Librarian => "Librarian",
            CellType::Warden => "Warden",
            CellType::Adjudicator => "Adjudicator",
            CellType::CrossCheckLedger => "CrossCheckLedger",
        }
    }

    pub fn is_trinity(&self) -> bool {
        matches!(
            self,
            CellType::SpecAnchor
                | CellType::DecisionLedger
                | CellType::Congruence
                | CellType::Gap
                | CellType::Quarantine
                | CellType::WorkBudget
                | CellType::Contract
        )
    }

    pub fn is_curator(&self) -> bool {
        matches!(
            self,
            CellType::Librarian
                | CellType::Warden
                | CellType::Adjudicator
                | CellType::CrossCheckLedger
        )
    }

    /// Inverse of [`CellType::as_str`] for envelope target resolution.
    pub fn parse(s: &str) -> Option<CellType> {
        Some(match s {
            "Catalog" => CellType::Catalog,
            "Fact" => CellType::Fact,
            "Timeline" => CellType::Timeline,
            "Playbook" => CellType::Playbook,
            "Scratchpad" => CellType::Scratchpad,
            "Vector" => CellType::Vector,
            "Graph" => CellType::Graph,
            "Bm25" => CellType::Bm25,
            "Blob" => CellType::Blob,
            "Cache" => CellType::Cache,
            "AgentRegistry" => CellType::AgentRegistry,
            "Proposal" => CellType::Proposal,
            "Subscription" => CellType::Subscription,
            "Reranker" => CellType::Reranker,
            "SpecAnchor" => CellType::SpecAnchor,
            "DecisionLedger" => CellType::DecisionLedger,
            "Congruence" => CellType::Congruence,
            "Gap" => CellType::Gap,
            "Quarantine" => CellType::Quarantine,
            "WorkBudget" => CellType::WorkBudget,
            "Contract" => CellType::Contract,
            "Librarian" => CellType::Librarian,
            "Warden" => CellType::Warden,
            "Adjudicator" => CellType::Adjudicator,
            "CrossCheckLedger" => CellType::CrossCheckLedger,
            _ => return None,
        })
    }

    /// Cells exempt from the mandatory spec_anchor requirement on writes
    /// (NATIVE_TRINITY.md §5.2: ephemeral working memory).
    pub fn anchor_exempt(&self) -> bool {
        matches!(self, CellType::Scratchpad | CellType::Cache)
    }
}

/// Common behavior every cell exposes to the Router.
pub trait CellBehavior: Send {
    fn cell_id(&self) -> CellId;
    fn cell_type(&self) -> CellType;
    fn schema_id(&self) -> SchemaId;
    /// Read path. Must not mutate state (interior mutability for stats only).
    fn on_query(&self, logical_at: u64, query: &Cbor) -> UcResult<Cbor>;
    /// Write path. Deterministic. Returns the produced handle(s)/result.
    fn on_update(&mut self, logical_at: u64, update: &Cbor) -> UcResult<Cbor>;
    /// Full-state snapshot as canonical CBOR.
    fn snapshot_state(&self) -> Cbor;
    /// Restore from a snapshot produced by `snapshot_state`.
    fn restore_state(&mut self, state: &Cbor) -> UcResult<()>;
}

// ---------------------------------------------------------------------------
// CatalogCell — cell #1, the namespace/cell registry (CellTaxonomy.md §2.1)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct CatalogEntry {
    pub cell_id: u64,
    pub cell_type: String,
    pub namespace: String,
    pub schema_id: String,
    pub registered_at: u64,
}

#[derive(Default)]
pub struct CatalogCell {
    pub id: CellId,
    entries: BTreeMap<u64, CatalogEntry>,
    namespaces: BTreeMap<String, Vec<u64>>,
}

impl CatalogCell {
    pub fn new(id: CellId) -> Self {
        CatalogCell {
            id,
            ..Default::default()
        }
    }

    pub fn register(&mut self, entry: CatalogEntry) {
        self.namespaces
            .entry(entry.namespace.clone())
            .or_default()
            .push(entry.cell_id);
        self.entries.insert(entry.cell_id, entry);
    }

    pub fn lookup(&self, cell_id: u64) -> Option<&CatalogEntry> {
        self.entries.get(&cell_id)
    }

    pub fn cells_in_namespace(&self, ns: &str) -> Vec<u64> {
        self.namespaces.get(ns).cloned().unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl CellBehavior for CatalogCell {
    fn cell_id(&self) -> CellId {
        self.id
    }
    fn cell_type(&self) -> CellType {
        CellType::Catalog
    }
    fn schema_id(&self) -> SchemaId {
        SchemaId::new("catalog.v1")
    }

    fn on_query(&self, _at: u64, query: &Cbor) -> UcResult<Cbor> {
        match query.opt_str("op").as_deref() {
            Some("list") => {
                let items: Vec<Cbor> = self
                    .entries
                    .values()
                    .map(|e| {
                        Cbor::map(vec![
                            ("cell_id", Cbor::U64(e.cell_id)),
                            ("cell_type", Cbor::t(e.cell_type.clone())),
                            ("namespace", Cbor::t(e.namespace.clone())),
                            ("schema_id", Cbor::t(e.schema_id.clone())),
                        ])
                    })
                    .collect();
                Ok(Cbor::map(vec![("cells", Cbor::Array(items))]))
            }
            Some("lookup") => {
                let id = query.req_u64("cell_id")?;
                match self.lookup(id) {
                    Some(e) => Ok(Cbor::map(vec![
                        ("cell_id", Cbor::U64(e.cell_id)),
                        ("cell_type", Cbor::t(e.cell_type.clone())),
                        ("namespace", Cbor::t(e.namespace.clone())),
                        ("schema_id", Cbor::t(e.schema_id.clone())),
                    ])),
                    None => Err(crate::core::UcError::not_found(format!(
                        "cell {id} not in catalog"
                    ))),
                }
            }
            _ => Err(crate::core::UcError::schema("catalog: unknown op")),
        }
    }

    fn on_update(&mut self, at: u64, update: &Cbor) -> UcResult<Cbor> {
        // Registration is bootstrap-driven; runtime registration is allowed
        // for dynamically provisioned namespace cells.
        let entry = CatalogEntry {
            cell_id: update.req_u64("cell_id")?,
            cell_type: update.req_str("cell_type")?,
            namespace: update.opt_str("namespace").unwrap_or_else(|| "default".into()),
            schema_id: update.opt_str("schema_id").unwrap_or_default(),
            registered_at: at,
        };
        let id = entry.cell_id;
        self.register(entry);
        Ok(Cbor::map(vec![("registered", Cbor::U64(id))]))
    }

    fn snapshot_state(&self) -> Cbor {
        let items: Vec<Cbor> = self
            .entries
            .values()
            .map(|e| {
                Cbor::map(vec![
                    ("cell_id", Cbor::U64(e.cell_id)),
                    ("cell_type", Cbor::t(e.cell_type.clone())),
                    ("namespace", Cbor::t(e.namespace.clone())),
                    ("schema_id", Cbor::t(e.schema_id.clone())),
                    ("registered_at", Cbor::U64(e.registered_at)),
                ])
            })
            .collect();
        Cbor::map(vec![("entries", Cbor::Array(items))])
    }

    fn restore_state(&mut self, state: &Cbor) -> UcResult<()> {
        self.entries.clear();
        self.namespaces.clear();
        if let Some(arr) = state.get("entries").and_then(|v| v.as_array()) {
            for item in arr {
                self.register(CatalogEntry {
                    cell_id: item.req_u64("cell_id")?,
                    cell_type: item.req_str("cell_type")?,
                    namespace: item.opt_str("namespace").unwrap_or_default(),
                    schema_id: item.opt_str("schema_id").unwrap_or_default(),
                    registered_at: item.opt_u64("registered_at").unwrap_or(0),
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_register_lookup_snapshot() {
        let mut cat = CatalogCell::new(CellId(1));
        cat.register(CatalogEntry {
            cell_id: 2,
            cell_type: "Fact".into(),
            namespace: "default".into(),
            schema_id: "fact.v1".into(),
            registered_at: 1,
        });
        assert_eq!(cat.lookup(2).unwrap().cell_type, "Fact");
        assert_eq!(cat.cells_in_namespace("default"), vec![2]);
        let snap = cat.snapshot_state();
        let mut cat2 = CatalogCell::new(CellId(1));
        cat2.restore_state(&snap).unwrap();
        assert_eq!(cat2.lookup(2).unwrap().schema_id, "fact.v1");
        // Snapshot determinism.
        assert_eq!(snap.encode(), cat2.snapshot_state().encode());
    }
}
