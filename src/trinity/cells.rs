//! Native Trinity cells — SPEC-DERIVED-§4–§11 (NATIVE_TRINITY.md).
//!
//! Seven governance cells that pre-validate every state-changing envelope
//! before it reaches a target cell. The chain order is fixed
//! (RouterScheduler.md §B) and enforced in [`super::chain`]:
//!
//! 1. Contract       — schema shape + deprecation + weight pinning
//! 2. SpecAnchor     — every write must cite a resolvable doc§section
//! 3. DecisionLedger — no write may contradict an active decision in scope
//! 4. WorkBudget     — mandatory budget, pre-charge before dispatch
//! 5. Congruence     — unknown-entity deltas block until accepted
//! 6. (Warden)       — optional semantic gate, wired by the Router
//!
//! Failures never vanish: they are absorbed by the QuarantineCell with a
//! cause chain (NATIVE_TRINITY.md §7 "no silent drops"). GapCell tracks
//! knowledge gaps and enforces anti-fixation (N=8 dispatches per gap
//! without a state transition ⇒ Fixation error, NATIVE_TRINITY.md §9).

use crate::cells::{CellBehavior, CellType};
use crate::core::cbor::Cbor;
use crate::core::ulid::{DetRng, Ulid};
use crate::core::{AnchorRef, CellId, ErrCode, SchemaId, UcError, UcResult};
use std::collections::{BTreeMap, BTreeSet};

/// Anti-fixation dispatch limit (NATIVE_TRINITY.md §9.2).
pub const GAP_FIXATION_N: u64 = 8;

// ---------------------------------------------------------------------------
// ContractCell (§10)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Contract {
    pub schema_id: String,
    pub required_fields: Vec<String>,
    pub deprecated: bool,
    pub registered_at: u64,
    /// Pinned curator weights: model -> sha256 hex (PersistenceLayer.md §7).
    pub pinned_weights: BTreeMap<String, String>,
}

pub struct ContractCell {
    pub id: CellId,
    contracts: BTreeMap<String, Contract>,
}

impl ContractCell {
    pub fn new(id: CellId) -> Self {
        ContractCell {
            id,
            contracts: BTreeMap::new(),
        }
    }

    pub fn register(&mut self, at: u64, schema_id: &str, required_fields: Vec<String>) {
        self.contracts.insert(
            schema_id.to_string(),
            Contract {
                schema_id: schema_id.to_string(),
                required_fields,
                deprecated: false,
                registered_at: at,
                pinned_weights: BTreeMap::new(),
            },
        );
    }

    pub fn pin_weights(&mut self, schema_id: &str, model: &str, sha_hex: &str) -> UcResult<()> {
        let c = self
            .contracts
            .get_mut(schema_id)
            .ok_or_else(|| UcError::not_found(format!("contract {schema_id}")))?;
        c.pinned_weights
            .insert(model.to_string(), sha_hex.to_ascii_lowercase());
        Ok(())
    }

    pub fn pinned_weights(&self, schema_id: &str) -> Option<&BTreeMap<String, String>> {
        self.contracts.get(schema_id).map(|c| &c.pinned_weights)
    }

    pub fn deprecate(&mut self, schema_id: &str) -> UcResult<()> {
        let c = self
            .contracts
            .get_mut(schema_id)
            .ok_or_else(|| UcError::not_found(format!("contract {schema_id}")))?;
        c.deprecated = true;
        Ok(())
    }

    /// Chain step 1. Contracts are opt-in per schema: if none is registered
    /// for `schema_id` the payload passes (curator schemas are always
    /// registered at boot, so curator writes are always shape-checked).
    pub fn validate_schema(&self, schema_id: &str, payload: &Cbor) -> UcResult<()> {
        let Some(c) = self.contracts.get(schema_id) else {
            return Ok(());
        };
        if c.deprecated {
            return Err(UcError::new(
                ErrCode::ContractViolation,
                format!("schema {schema_id} is deprecated"),
            ));
        }
        for field in &c.required_fields {
            if payload.get(field).is_none() {
                return Err(UcError::new(
                    ErrCode::ContractViolation,
                    format!("schema {schema_id}: missing required field `{field}`"),
                ));
            }
        }
        Ok(())
    }

    pub fn list(&self) -> Vec<&Contract> {
        self.contracts.values().collect()
    }
}

impl CellBehavior for ContractCell {
    fn cell_id(&self) -> CellId {
        self.id
    }
    fn cell_type(&self) -> CellType {
        CellType::Contract
    }
    fn schema_id(&self) -> SchemaId {
        SchemaId::new("trinity.contract.v1")
    }

    fn on_query(&self, _at: u64, query: &Cbor) -> UcResult<Cbor> {
        match query.opt_str("op").as_deref() {
            Some("list") => {
                let items: Vec<Cbor> = self
                    .contracts
                    .values()
                    .map(|c| {
                        Cbor::map(vec![
                            ("schema_id", Cbor::t(c.schema_id.clone())),
                            ("required_fields", Cbor::text_array(&c.required_fields)),
                            ("deprecated", Cbor::Bool(c.deprecated)),
                            (
                                "pinned_weights",
                                Cbor::map(
                                    c.pinned_weights
                                        .iter()
                                        .map(|(m, s)| (m.as_str(), Cbor::t(s.clone())))
                                        .collect(),
                                ),
                            ),
                        ])
                    })
                    .collect();
                Ok(Cbor::map(vec![("contracts", Cbor::Array(items))]))
            }
            Some("validate") => {
                let sid = query.req_str("schema_id")?;
                let payload = query
                    .get("payload")
                    .ok_or_else(|| UcError::schema("contract validate: missing payload"))?;
                self.validate_schema(&sid, payload)?;
                Ok(Cbor::map(vec![("valid", Cbor::Bool(true))]))
            }
            _ => Err(UcError::schema("contract: unknown op")),
        }
    }

    fn on_update(&mut self, at: u64, update: &Cbor) -> UcResult<Cbor> {
        match update.opt_str("op").as_deref() {
            Some("register") | None => {
                let sid = update.req_str("schema_id")?;
                let fields = update
                    .get("required_fields")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                    .unwrap_or_default();
                self.register(at, &sid, fields);
                Ok(Cbor::map(vec![("registered", Cbor::t(sid))]))
            }
            Some("deprecate") => {
                let sid = update.req_str("schema_id")?;
                self.deprecate(&sid)?;
                Ok(Cbor::map(vec![("deprecated", Cbor::t(sid))]))
            }
            Some("pin_weights") => {
                let sid = update.req_str("schema_id")?;
                let model = update.req_str("model")?;
                let sha = update.req_str("sha256")?;
                self.pin_weights(&sid, &model, &sha)?;
                Ok(Cbor::map(vec![("pinned", Cbor::t(model))]))
            }
            _ => Err(UcError::schema("contract: unknown update op")),
        }
    }

    fn snapshot_state(&self) -> Cbor {
        let items: Vec<Cbor> = self
            .contracts
            .values()
            .map(|c| {
                Cbor::map(vec![
                    ("schema_id", Cbor::t(c.schema_id.clone())),
                    ("required_fields", Cbor::text_array(&c.required_fields)),
                    ("deprecated", Cbor::Bool(c.deprecated)),
                    ("registered_at", Cbor::U64(c.registered_at)),
                    (
                        "pinned_weights",
                        Cbor::map(
                            c.pinned_weights
                                .iter()
                                .map(|(m, s)| (m.as_str(), Cbor::t(s.clone())))
                                .collect(),
                        ),
                    ),
                ])
            })
            .collect();
        Cbor::map(vec![("contracts", Cbor::Array(items))])
    }

    fn restore_state(&mut self, state: &Cbor) -> UcResult<()> {
        self.contracts.clear();
        if let Some(arr) = state.get("contracts").and_then(|v| v.as_array()) {
            for item in arr {
                let mut pinned = BTreeMap::new();
                if let Some(pw) = item.get("pinned_weights").and_then(|v| v.as_map()) {
                    for (k, v) in pw {
                        if let (Some(m), Some(s)) = (k.as_str(), v.as_str()) {
                            pinned.insert(m.to_string(), s.to_string());
                        }
                    }
                }
                let c = Contract {
                    schema_id: item.req_str("schema_id")?,
                    required_fields: item
                        .get("required_fields")
                        .and_then(|v| v.as_array())
                        .map(|a| {
                            a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()
                        })
                        .unwrap_or_default(),
                    deprecated: item.opt_bool("deprecated").unwrap_or(false),
                    registered_at: item.opt_u64("registered_at").unwrap_or(0),
                    pinned_weights: pinned,
                };
                self.contracts.insert(c.schema_id.clone(), c);
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// SpecAnchorCell (§5)
// ---------------------------------------------------------------------------

pub struct SpecAnchorCell {
    pub id: CellId,
    /// key = "doc§section"
    anchors: BTreeSet<String>,
    /// doc -> registered section count (for `congruence audit` reporting).
    per_doc: BTreeMap<String, u64>,
}

impl SpecAnchorCell {
    pub fn new(id: CellId) -> Self {
        SpecAnchorCell {
            id,
            anchors: BTreeSet::new(),
            per_doc: BTreeMap::new(),
        }
    }

    pub fn register(&mut self, doc: &str, section: &str) {
        let a = AnchorRef::new(doc, section);
        if self.anchors.insert(a.key()) {
            *self.per_doc.entry(doc.to_string()).or_insert(0) += 1;
        }
    }

    pub fn resolves(&self, anchor: &AnchorRef) -> bool {
        self.anchors.contains(&anchor.key())
    }

    /// Chain step 2. Writes to anchor-exempt cell types (Scratchpad, Cache)
    /// pass without an anchor; everything else must cite one that resolves
    /// (NATIVE_TRINITY.md §5.2).
    pub fn validate(
        &self,
        target_type: CellType,
        anchor: Option<&AnchorRef>,
    ) -> UcResult<()> {
        if target_type.anchor_exempt() {
            return Ok(());
        }
        match anchor {
            None => Err(UcError::new(
                ErrCode::AnchorMissing,
                format!(
                    "write to {} requires a spec_anchor (only Scratchpad/Cache are exempt)",
                    target_type.as_str()
                ),
            )),
            Some(a) if !self.resolves(a) => Err(UcError::new(
                ErrCode::AnchorMissing,
                format!("spec_anchor {}§{} does not resolve", a.doc, a.section),
            )
            .with_anchor(a.clone())),
            Some(_) => Ok(()),
        }
    }

    pub fn count(&self) -> usize {
        self.anchors.len()
    }

    pub fn docs(&self) -> &BTreeMap<String, u64> {
        &self.per_doc
    }

    /// Parse an anchor from its string form `Doc.md§Section` (also accepts
    /// `Doc.md#Section` for terminals that can't type §).
    pub fn parse_anchor(s: &str) -> Option<AnchorRef> {
        let (doc, section) = s.split_once('\u{00a7}').or_else(|| s.split_once('#'))?;
        if doc.is_empty() || section.is_empty() {
            return None;
        }
        Some(AnchorRef::new(doc.trim(), section.trim()))
    }
}

impl CellBehavior for SpecAnchorCell {
    fn cell_id(&self) -> CellId {
        self.id
    }
    fn cell_type(&self) -> CellType {
        CellType::SpecAnchor
    }
    fn schema_id(&self) -> SchemaId {
        SchemaId::new("trinity.spec_anchor.v1")
    }

    fn on_query(&self, _at: u64, query: &Cbor) -> UcResult<Cbor> {
        match query.opt_str("op").as_deref() {
            Some("resolves") => {
                let s = query.req_str("anchor")?;
                let ok = Self::parse_anchor(&s)
                    .map(|a| self.resolves(&a))
                    .unwrap_or(false);
                Ok(Cbor::map(vec![("resolves", Cbor::Bool(ok))]))
            }
            Some("stats") => {
                let docs: Vec<Cbor> = self
                    .per_doc
                    .iter()
                    .map(|(d, n)| {
                        Cbor::map(vec![("doc", Cbor::t(d.clone())), ("sections", Cbor::U64(*n))])
                    })
                    .collect();
                Ok(Cbor::map(vec![
                    ("total", Cbor::U64(self.anchors.len() as u64)),
                    ("docs", Cbor::Array(docs)),
                ]))
            }
            _ => Err(UcError::schema("spec_anchor: unknown op")),
        }
    }

    fn on_update(&mut self, _at: u64, update: &Cbor) -> UcResult<Cbor> {
        let doc = update.req_str("doc")?;
        let section = update.req_str("section")?;
        self.register(&doc, &section);
        Ok(Cbor::map(vec![("registered", Cbor::t(format!("{doc}\u{00a7}{section}")))]))
    }

    fn snapshot_state(&self) -> Cbor {
        Cbor::map(vec![(
            "anchors",
            Cbor::text_array(&self.anchors.iter().cloned().collect::<Vec<_>>()),
        )])
    }

    fn restore_state(&mut self, state: &Cbor) -> UcResult<()> {
        self.anchors.clear();
        self.per_doc.clear();
        if let Some(arr) = state.get("anchors").and_then(|v| v.as_array()) {
            for item in arr {
                if let Some(s) = item.as_str() {
                    if let Some(a) = Self::parse_anchor(s) {
                        self.register(&a.doc, &a.section);
                    }
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// DecisionLedgerCell (§6)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Decision {
    pub handle: String, // "decision/<ulid>"
    pub scope: String,  // e.g. "embedder.dim"
    pub statement: String,
    pub decided_by: String,
    pub made_at: u64,
    pub superseded_by: Option<String>,
    pub anchor: String,
}

pub struct DecisionLedgerCell {
    pub id: CellId,
    decisions: BTreeMap<String, Decision>,
    by_scope: BTreeMap<String, Vec<String>>,
}

impl DecisionLedgerCell {
    pub fn new(id: CellId) -> Self {
        DecisionLedgerCell {
            id,
            decisions: BTreeMap::new(),
            by_scope: BTreeMap::new(),
        }
    }

    pub fn append(
        &mut self,
        at: u64,
        seed: u64,
        scope: &str,
        statement: &str,
        decided_by: &str,
        anchor: &str,
    ) -> String {
        let handle = format!(
            "decision/{}",
            Ulid::from_parts(at, &mut DetRng::new(seed ^ at ^ 0xD0))
        );
        let d = Decision {
            handle: handle.clone(),
            scope: scope.to_string(),
            statement: statement.to_string(),
            decided_by: decided_by.to_string(),
            made_at: at,
            superseded_by: None,
            anchor: anchor.to_string(),
        };
        self.by_scope
            .entry(scope.to_string())
            .or_default()
            .push(handle.clone());
        self.decisions.insert(handle.clone(), d);
        handle
    }

    pub fn active_in_scope(&self, scope: &str) -> Vec<&Decision> {
        self.by_scope
            .get(scope)
            .map(|hs| {
                hs.iter()
                    .filter_map(|h| self.decisions.get(h))
                    .filter(|d| d.superseded_by.is_none())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn get(&self, handle: &str) -> Option<&Decision> {
        self.decisions.get(handle)
    }

    pub fn exists(&self, handle: &str) -> bool {
        self.decisions.contains_key(handle)
    }

    pub fn supersede(&mut self, old: &str, new: &str) -> UcResult<()> {
        if !self.decisions.contains_key(new) {
            return Err(UcError::not_found(format!("decision {new}")));
        }
        let d = self
            .decisions
            .get_mut(old)
            .ok_or_else(|| UcError::not_found(format!("decision {old}")))?;
        if d.superseded_by.is_some() {
            return Err(UcError::schema(format!("{old} already superseded")));
        }
        d.superseded_by = Some(new.to_string());
        Ok(())
    }

    /// Chain step 3. Payloads that declare a `decision_scope` are checked:
    /// if an active decision exists in that scope, the payload must either
    /// reference it (`respects_decision`) or explicitly supersede it
    /// (`supersedes_decision`) — otherwise DecisionConflict
    /// (NATIVE_TRINITY.md §6.3).
    pub fn check_conflicts(&self, payload: &Cbor) -> UcResult<()> {
        let Some(scope) = payload.opt_str("decision_scope") else {
            return Ok(());
        };
        let active = self.active_in_scope(&scope);
        if active.is_empty() {
            return Ok(());
        }
        let respects = payload.opt_str("respects_decision");
        let supersedes = payload.opt_str("supersedes_decision");
        let referenced_ok = |h: &str| active.iter().any(|d| d.handle == h);
        match (respects.as_deref(), supersedes.as_deref()) {
            (Some(h), _) if referenced_ok(h) => Ok(()),
            (_, Some(h)) if referenced_ok(h) => Ok(()),
            _ => {
                let holder = &active[0];
                Err(UcError::new(
                    ErrCode::DecisionConflict,
                    format!(
                        "scope `{scope}` is governed by {} (\"{}\"); payload must set \
                         respects_decision or supersedes_decision",
                        holder.handle, holder.statement
                    ),
                ))
            }
        }
    }

    pub fn len(&self) -> usize {
        self.decisions.len()
    }
    pub fn is_empty(&self) -> bool {
        self.decisions.is_empty()
    }
}

impl CellBehavior for DecisionLedgerCell {
    fn cell_id(&self) -> CellId {
        self.id
    }
    fn cell_type(&self) -> CellType {
        CellType::DecisionLedger
    }
    fn schema_id(&self) -> SchemaId {
        SchemaId::new("trinity.decision_ledger.v1")
    }

    fn on_query(&self, _at: u64, query: &Cbor) -> UcResult<Cbor> {
        match query.opt_str("op").as_deref() {
            Some("scope") => {
                let scope = query.req_str("scope")?;
                let items: Vec<Cbor> = self
                    .active_in_scope(&scope)
                    .iter()
                    .map(|d| decision_to_cbor(d))
                    .collect();
                Ok(Cbor::map(vec![("decisions", Cbor::Array(items))]))
            }
            Some("get") => {
                let h = query.req_str("handle")?;
                self.get(&h)
                    .map(decision_to_cbor)
                    .ok_or_else(|| UcError::not_found(format!("decision {h}")))
            }
            _ => Err(UcError::schema("decision_ledger: unknown op")),
        }
    }

    fn on_update(&mut self, at: u64, update: &Cbor) -> UcResult<Cbor> {
        match update.opt_str("op").as_deref() {
            Some("append") | None => {
                let handle = self.append(
                    at,
                    update.opt_u64("seed").unwrap_or(0),
                    &update.req_str("scope")?,
                    &update.req_str("statement")?,
                    &update.opt_str("decided_by").unwrap_or_default(),
                    &update.opt_str("anchor").unwrap_or_default(),
                );
                Ok(Cbor::map(vec![("handle", Cbor::t(handle))]))
            }
            Some("supersede") => {
                let old = update.req_str("old")?;
                let new = update.req_str("new")?;
                self.supersede(&old, &new)?;
                Ok(Cbor::map(vec![("superseded", Cbor::t(old)), ("by", Cbor::t(new))]))
            }
            _ => Err(UcError::schema("decision_ledger: unknown update op")),
        }
    }

    fn snapshot_state(&self) -> Cbor {
        let items: Vec<Cbor> = self.decisions.values().map(decision_to_cbor).collect();
        Cbor::map(vec![("decisions", Cbor::Array(items))])
    }

    fn restore_state(&mut self, state: &Cbor) -> UcResult<()> {
        self.decisions.clear();
        self.by_scope.clear();
        if let Some(arr) = state.get("decisions").and_then(|v| v.as_array()) {
            for item in arr {
                let d = Decision {
                    handle: item.req_str("handle")?,
                    scope: item.req_str("scope")?,
                    statement: item.opt_str("statement").unwrap_or_default(),
                    decided_by: item.opt_str("decided_by").unwrap_or_default(),
                    made_at: item.opt_u64("made_at").unwrap_or(0),
                    superseded_by: item.opt_str("superseded_by"),
                    anchor: item.opt_str("anchor").unwrap_or_default(),
                };
                self.by_scope
                    .entry(d.scope.clone())
                    .or_default()
                    .push(d.handle.clone());
                self.decisions.insert(d.handle.clone(), d);
            }
        }
        Ok(())
    }
}

fn decision_to_cbor(d: &Decision) -> Cbor {
    Cbor::map(vec![
        ("handle", Cbor::t(d.handle.clone())),
        ("scope", Cbor::t(d.scope.clone())),
        ("statement", Cbor::t(d.statement.clone())),
        ("decided_by", Cbor::t(d.decided_by.clone())),
        ("made_at", Cbor::U64(d.made_at)),
        (
            "superseded_by",
            d.superseded_by.as_ref().map(|s| Cbor::t(s.clone())).unwrap_or(Cbor::Null),
        ),
        ("anchor", Cbor::t(d.anchor.clone())),
    ])
}

// ---------------------------------------------------------------------------
// WorkBudgetCell (§8)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Budget {
    pub task_id: String,
    pub granted: u64,
    pub reserved: u64,
    pub spent: u64,
}

impl Budget {
    pub fn available(&self) -> u64 {
        self.granted.saturating_sub(self.reserved + self.spent)
    }
}

pub struct WorkBudgetCell {
    pub id: CellId,
    budgets: BTreeMap<String, Budget>,
    pub default_grant: u64,
}

impl WorkBudgetCell {
    pub fn new(id: CellId) -> Self {
        WorkBudgetCell {
            id,
            budgets: BTreeMap::new(),
            default_grant: 100_000,
        }
    }

    pub fn ensure(&mut self, task_id: &str, grant: Option<u64>) -> &Budget {
        self.budgets
            .entry(task_id.to_string())
            .or_insert_with(|| Budget {
                task_id: task_id.to_string(),
                granted: grant.unwrap_or(self.default_grant),
                reserved: 0,
                spent: 0,
            })
    }

    /// Chain step 4: reserve estimated units before dispatch. Envelopes
    /// without a work_budget never reach here — E-invariant E2 rejects them
    /// at the protocol edge (McpProtocol.md §5: work_budget is MANDATORY).
    pub fn charge_pre(&mut self, task_id: &str, estimate: u64) -> UcResult<()> {
        let b = self
            .budgets
            .get_mut(task_id)
            .ok_or_else(|| UcError::not_found(format!("budget for task {task_id}")))?;
        if b.available() < estimate {
            return Err(UcError::new(
                ErrCode::BudgetExceeded,
                format!(
                    "task {task_id}: need {estimate}, available {} (granted {}, spent {}, reserved {})",
                    b.available(),
                    b.granted,
                    b.spent,
                    b.reserved
                ),
            ));
        }
        b.reserved += estimate;
        Ok(())
    }

    /// Post-dispatch reconciliation: convert the reservation into actual
    /// spend (actual may be below the estimate; never above it — overruns
    /// are clamped and surfaced via the `budget.overrun` metric by the
    /// router).
    pub fn charge_post(&mut self, task_id: &str, reserved: u64, actual: u64) -> u64 {
        let Some(b) = self.budgets.get_mut(task_id) else {
            return 0;
        };
        let spent = actual.min(reserved);
        b.reserved = b.reserved.saturating_sub(reserved);
        b.spent += spent;
        actual.saturating_sub(reserved) // overrun amount, 0 if none
    }

    pub fn get(&self, task_id: &str) -> Option<&Budget> {
        self.budgets.get(task_id)
    }
}

impl CellBehavior for WorkBudgetCell {
    fn cell_id(&self) -> CellId {
        self.id
    }
    fn cell_type(&self) -> CellType {
        CellType::WorkBudget
    }
    fn schema_id(&self) -> SchemaId {
        SchemaId::new("trinity.work_budget.v1")
    }

    fn on_query(&self, _at: u64, query: &Cbor) -> UcResult<Cbor> {
        let task = query.req_str("task_id")?;
        self.get(&task)
            .map(|b| {
                Cbor::map(vec![
                    ("task_id", Cbor::t(b.task_id.clone())),
                    ("granted", Cbor::U64(b.granted)),
                    ("reserved", Cbor::U64(b.reserved)),
                    ("spent", Cbor::U64(b.spent)),
                    ("available", Cbor::U64(b.available())),
                ])
            })
            .ok_or_else(|| UcError::not_found(format!("budget {task}")))
    }

    fn on_update(&mut self, _at: u64, update: &Cbor) -> UcResult<Cbor> {
        let task = update.req_str("task_id")?;
        let grant = update.opt_u64("grant");
        let b = self.ensure(&task, grant);
        Ok(Cbor::map(vec![
            ("task_id", Cbor::t(b.task_id.clone())),
            ("granted", Cbor::U64(b.granted)),
        ]))
    }

    fn snapshot_state(&self) -> Cbor {
        let items: Vec<Cbor> = self
            .budgets
            .values()
            .map(|b| {
                Cbor::map(vec![
                    ("task_id", Cbor::t(b.task_id.clone())),
                    ("granted", Cbor::U64(b.granted)),
                    ("reserved", Cbor::U64(b.reserved)),
                    ("spent", Cbor::U64(b.spent)),
                ])
            })
            .collect();
        Cbor::map(vec![
            ("budgets", Cbor::Array(items)),
            ("default_grant", Cbor::U64(self.default_grant)),
        ])
    }

    fn restore_state(&mut self, state: &Cbor) -> UcResult<()> {
        self.budgets.clear();
        self.default_grant = state.opt_u64("default_grant").unwrap_or(100_000);
        if let Some(arr) = state.get("budgets").and_then(|v| v.as_array()) {
            for item in arr {
                let b = Budget {
                    task_id: item.req_str("task_id")?,
                    granted: item.opt_u64("granted").unwrap_or(0),
                    reserved: item.opt_u64("reserved").unwrap_or(0),
                    spent: item.opt_u64("spent").unwrap_or(0),
                };
                self.budgets.insert(b.task_id.clone(), b);
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// CongruenceCell (§7)
// ---------------------------------------------------------------------------

pub struct CongruenceCell {
    pub id: CellId,
    /// Known entities extracted from spec docs at boot: cell type names,
    /// `GAP-*` identifiers, `P##` principle numbers, doc file names.
    known: BTreeSet<String>,
    /// Deltas explicitly accepted (adjudicator/operator) — new entities the
    /// corpus may now reference.
    accepted: BTreeSet<String>,
}

impl CongruenceCell {
    pub fn new(id: CellId) -> Self {
        CongruenceCell {
            id,
            known: BTreeSet::new(),
            accepted: BTreeSet::new(),
        }
    }

    pub fn register_entity(&mut self, entity: &str) {
        self.known.insert(entity.to_string());
    }

    pub fn accept_delta(&mut self, entity: &str) {
        self.accepted.insert(entity.to_string());
    }

    pub fn is_known(&self, entity: &str) -> bool {
        self.known.contains(entity) || self.accepted.contains(entity)
    }

    /// Extract congruence-relevant entity mentions from payload text:
    /// `GAP-…` identifiers, two-digit `P##` principles, and `…Cell` names.
    /// (Single-digit P0/P1/P2 are severities, deliberately excluded.)
    pub fn extract_entities(payload: &Cbor) -> BTreeSet<String> {
        let mut texts = Vec::new();
        payload.collect_texts(&mut texts);
        let mut out = BTreeSet::new();
        for text in texts {
            for raw in text.split(|c: char| c.is_whitespace() || ",;()[]{}\"'".contains(c)) {
                let tok = raw.trim_matches(|c: char| ".:!?".contains(c));
                if tok.is_empty() {
                    continue;
                }
                if let Some(rest) = tok.strip_prefix("GAP-") {
                    if !rest.is_empty()
                        && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                    {
                        out.insert(tok.to_string());
                    }
                } else if tok.len() == 3
                    && tok.starts_with('P')
                    && tok[1..].chars().all(|c| c.is_ascii_digit())
                {
                    out.insert(tok.to_string());
                } else if tok.len() > 4
                    && tok.ends_with("Cell")
                    && tok.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                {
                    out.insert(tok.to_string());
                }
            }
        }
        out
    }

    /// Chain step 5: unknown entities in a state-changing payload block the
    /// write until the delta is accepted (NATIVE_TRINITY.md §7.4 — new
    /// vocabulary must arrive through governance, not through drift).
    pub fn preview_delta(&self, payload: &Cbor) -> UcResult<()> {
        let unknown: Vec<String> = Self::extract_entities(payload)
            .into_iter()
            .filter(|e| !self.is_known(e))
            .collect();
        if unknown.is_empty() {
            Ok(())
        } else {
            Err(UcError::new(
                ErrCode::CongruenceDelta,
                format!(
                    "payload references unknown entities: {} — accept the delta via \
                     `congruence` admin or supersede through a decision",
                    unknown.join(", ")
                ),
            ))
        }
    }

    pub fn known_count(&self) -> usize {
        self.known.len()
    }
    pub fn accepted_count(&self) -> usize {
        self.accepted.len()
    }
}

impl CellBehavior for CongruenceCell {
    fn cell_id(&self) -> CellId {
        self.id
    }
    fn cell_type(&self) -> CellType {
        CellType::Congruence
    }
    fn schema_id(&self) -> SchemaId {
        SchemaId::new("trinity.congruence.v1")
    }

    fn on_query(&self, _at: u64, query: &Cbor) -> UcResult<Cbor> {
        match query.opt_str("op").as_deref() {
            Some("audit") => Ok(Cbor::map(vec![
                ("known", Cbor::U64(self.known.len() as u64)),
                ("accepted_deltas", Cbor::U64(self.accepted.len() as u64)),
                (
                    "accepted",
                    Cbor::text_array(&self.accepted.iter().cloned().collect::<Vec<_>>()),
                ),
            ])),
            Some("preview") => {
                let payload = query
                    .get("payload")
                    .ok_or_else(|| UcError::schema("congruence preview: missing payload"))?;
                match self.preview_delta(payload) {
                    Ok(()) => Ok(Cbor::map(vec![("delta", Cbor::Bool(false))])),
                    Err(e) => Ok(Cbor::map(vec![
                        ("delta", Cbor::Bool(true)),
                        ("detail", Cbor::t(e.message)),
                    ])),
                }
            }
            _ => Err(UcError::schema("congruence: unknown op")),
        }
    }

    fn on_update(&mut self, _at: u64, update: &Cbor) -> UcResult<Cbor> {
        match update.opt_str("op").as_deref() {
            Some("register") | None => {
                let e = update.req_str("entity")?;
                self.register_entity(&e);
                Ok(Cbor::map(vec![("registered", Cbor::t(e))]))
            }
            Some("accept_delta") => {
                let e = update.req_str("entity")?;
                self.accept_delta(&e);
                Ok(Cbor::map(vec![("accepted", Cbor::t(e))]))
            }
            _ => Err(UcError::schema("congruence: unknown update op")),
        }
    }

    fn snapshot_state(&self) -> Cbor {
        Cbor::map(vec![
            (
                "known",
                Cbor::text_array(&self.known.iter().cloned().collect::<Vec<_>>()),
            ),
            (
                "accepted",
                Cbor::text_array(&self.accepted.iter().cloned().collect::<Vec<_>>()),
            ),
        ])
    }

    fn restore_state(&mut self, state: &Cbor) -> UcResult<()> {
        self.known.clear();
        self.accepted.clear();
        if let Some(arr) = state.get("known").and_then(|v| v.as_array()) {
            for item in arr {
                if let Some(s) = item.as_str() {
                    self.known.insert(s.to_string());
                }
            }
        }
        if let Some(arr) = state.get("accepted").and_then(|v| v.as_array()) {
            for item in arr {
                if let Some(s) = item.as_str() {
                    self.accepted.insert(s.to_string());
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// GapCell (§9)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GapState {
    Open,
    Investigating,
    Resolved,
    Fixated, // hit the N=8 limit; needs operator/adjudicator attention
}

impl GapState {
    pub fn as_str(self) -> &'static str {
        match self {
            GapState::Open => "open",
            GapState::Investigating => "investigating",
            GapState::Resolved => "resolved",
            GapState::Fixated => "fixated",
        }
    }
    pub fn parse(s: &str) -> Option<GapState> {
        Some(match s {
            "open" => GapState::Open,
            "investigating" => GapState::Investigating,
            "resolved" => GapState::Resolved,
            "fixated" => GapState::Fixated,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug)]
pub struct Gap {
    pub gap_id: String, // "GAP-…"
    pub description: String,
    pub state: GapState,
    pub opened_at: u64,
    /// Dispatches referencing this gap since the last state transition.
    pub dispatches_since_transition: u64,
    pub total_dispatches: u64,
}

pub struct GapCell {
    pub id: CellId,
    gaps: BTreeMap<String, Gap>,
}

impl GapCell {
    pub fn new(id: CellId) -> Self {
        GapCell {
            id,
            gaps: BTreeMap::new(),
        }
    }

    pub fn register(&mut self, at: u64, gap_id: &str, description: &str) {
        self.gaps.entry(gap_id.to_string()).or_insert(Gap {
            gap_id: gap_id.to_string(),
            description: description.to_string(),
            state: GapState::Open,
            opened_at: at,
            dispatches_since_transition: 0,
            total_dispatches: 0,
        });
    }

    pub fn transition(&mut self, gap_id: &str, to: GapState) -> UcResult<()> {
        let g = self
            .gaps
            .get_mut(gap_id)
            .ok_or_else(|| UcError::not_found(format!("gap {gap_id}")))?;
        g.state = to;
        // A genuine state transition resets the fixation counter (§9.3).
        g.dispatches_since_transition = 0;
        Ok(())
    }

    /// Gap-aware dispatch accounting. Called by the router whenever an
    /// envelope carries `gap_ref`. Returns Fixation once the counter
    /// *exceeds* N=8 dispatches with no state transition (dispatch N+1 and
    /// onward are rejected; the gap flips to `Fixated`) — NATIVE_TRINITY.md
    /// §9.2 and conformance test T4.
    pub fn on_dispatch(&mut self, gap_id: &str) -> UcResult<()> {
        let g = self
            .gaps
            .get_mut(gap_id)
            .ok_or_else(|| UcError::not_found(format!("gap {gap_id} not registered")))?;
        if g.state == GapState::Resolved {
            return Err(UcError::schema(format!("gap {gap_id} already resolved")));
        }
        if g.state == GapState::Fixated {
            return Err(UcError::new(
                ErrCode::Fixation,
                format!("gap {gap_id} is fixated; requires operator/adjudicator transition"),
            ));
        }
        g.dispatches_since_transition += 1;
        g.total_dispatches += 1;
        if g.dispatches_since_transition > GAP_FIXATION_N {
            g.state = GapState::Fixated;
            return Err(UcError::new(
                ErrCode::Fixation,
                format!(
                    "gap {gap_id}: {} dispatches without a state transition (limit {})",
                    g.dispatches_since_transition, GAP_FIXATION_N
                ),
            ));
        }
        Ok(())
    }

    pub fn get(&self, gap_id: &str) -> Option<&Gap> {
        self.gaps.get(gap_id)
    }

    pub fn list(&self) -> Vec<&Gap> {
        self.gaps.values().collect()
    }
}

impl CellBehavior for GapCell {
    fn cell_id(&self) -> CellId {
        self.id
    }
    fn cell_type(&self) -> CellType {
        CellType::Gap
    }
    fn schema_id(&self) -> SchemaId {
        SchemaId::new("trinity.gap.v1")
    }

    fn on_query(&self, _at: u64, query: &Cbor) -> UcResult<Cbor> {
        match query.opt_str("op").as_deref() {
            Some("list") | None => {
                let items: Vec<Cbor> = self
                    .gaps
                    .values()
                    .map(|g| {
                        Cbor::map(vec![
                            ("gap_id", Cbor::t(g.gap_id.clone())),
                            ("description", Cbor::t(g.description.clone())),
                            ("state", Cbor::t(g.state.as_str())),
                            (
                                "dispatches_since_transition",
                                Cbor::U64(g.dispatches_since_transition),
                            ),
                            ("total_dispatches", Cbor::U64(g.total_dispatches)),
                        ])
                    })
                    .collect();
                Ok(Cbor::map(vec![("gaps", Cbor::Array(items))]))
            }
            _ => Err(UcError::schema("gap: unknown op")),
        }
    }

    fn on_update(&mut self, at: u64, update: &Cbor) -> UcResult<Cbor> {
        match update.opt_str("op").as_deref() {
            Some("register") | None => {
                let gid = update.req_str("gap_id")?;
                let desc = update.opt_str("description").unwrap_or_default();
                self.register(at, &gid, &desc);
                Ok(Cbor::map(vec![("registered", Cbor::t(gid))]))
            }
            Some("transition") => {
                let gid = update.req_str("gap_id")?;
                let to = GapState::parse(&update.req_str("to")?)
                    .ok_or_else(|| UcError::schema("gap: bad state"))?;
                self.transition(&gid, to)?;
                Ok(Cbor::map(vec![
                    ("gap_id", Cbor::t(gid)),
                    ("state", Cbor::t(to.as_str())),
                ]))
            }
            _ => Err(UcError::schema("gap: unknown update op")),
        }
    }

    fn snapshot_state(&self) -> Cbor {
        let items: Vec<Cbor> = self
            .gaps
            .values()
            .map(|g| {
                Cbor::map(vec![
                    ("gap_id", Cbor::t(g.gap_id.clone())),
                    ("description", Cbor::t(g.description.clone())),
                    ("state", Cbor::t(g.state.as_str())),
                    ("opened_at", Cbor::U64(g.opened_at)),
                    (
                        "dispatches_since_transition",
                        Cbor::U64(g.dispatches_since_transition),
                    ),
                    ("total_dispatches", Cbor::U64(g.total_dispatches)),
                ])
            })
            .collect();
        Cbor::map(vec![("gaps", Cbor::Array(items))])
    }

    fn restore_state(&mut self, state: &Cbor) -> UcResult<()> {
        self.gaps.clear();
        if let Some(arr) = state.get("gaps").and_then(|v| v.as_array()) {
            for item in arr {
                let g = Gap {
                    gap_id: item.req_str("gap_id")?,
                    description: item.opt_str("description").unwrap_or_default(),
                    state: item
                        .opt_str("state")
                        .and_then(|s| GapState::parse(&s))
                        .unwrap_or(GapState::Open),
                    opened_at: item.opt_u64("opened_at").unwrap_or(0),
                    dispatches_since_transition: item
                        .opt_u64("dispatches_since_transition")
                        .unwrap_or(0),
                    total_dispatches: item.opt_u64("total_dispatches").unwrap_or(0),
                };
                self.gaps.insert(g.gap_id.clone(), g);
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// QuarantineCell (§7 of chain semantics; NATIVE_TRINITY.md §11)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuarantineStatus {
    Pending,
    Reinjected,
    Rejected,
}

impl QuarantineStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            QuarantineStatus::Pending => "pending",
            QuarantineStatus::Reinjected => "reinjected",
            QuarantineStatus::Rejected => "rejected",
        }
    }
}

#[derive(Clone, Debug)]
pub struct QuarantineItem {
    pub qid: String, // "quarantine/<ulid>"
    pub absorbed_at: u64,
    pub cause: ErrCode,
    pub detail: String,
    /// The failed envelope (or a summary of it) — enough to reinject.
    pub payload: Cbor,
    pub status: QuarantineStatus,
    pub resolved_at: Option<u64>,
}

pub struct QuarantineCell {
    pub id: CellId,
    items: BTreeMap<String, QuarantineItem>,
    /// Resolved items older than this many logical ticks are pruned;
    /// PENDING items are NEVER dropped (no-silent-drop invariant, §11.2).
    pub resolved_retention: u64,
}

impl QuarantineCell {
    pub fn new(id: CellId) -> Self {
        QuarantineCell {
            id,
            items: BTreeMap::new(),
            resolved_retention: 1_000_000,
        }
    }

    /// Absorb a failed envelope. Never fails, never drops — that's the
    /// contract the whole Trinity relies on (conformance test T6).
    pub fn absorb(
        &mut self,
        at: u64,
        seed: u64,
        cause: ErrCode,
        detail: &str,
        payload: Cbor,
    ) -> String {
        let qid = format!(
            "quarantine/{}",
            Ulid::from_parts(at, &mut DetRng::new(seed ^ at ^ 0x0A))
        );
        self.items.insert(
            qid.clone(),
            QuarantineItem {
                qid: qid.clone(),
                absorbed_at: at,
                cause,
                detail: detail.to_string(),
                payload,
                status: QuarantineStatus::Pending,
                resolved_at: None,
            },
        );
        qid
    }

    /// Prune *resolved* items past retention. Pending items are untouchable.
    pub fn sweep(&mut self, now: u64) -> u64 {
        let retention = self.resolved_retention;
        let before = self.items.len();
        self.items.retain(|_, item| {
            !(item.status != QuarantineStatus::Pending
                && item
                    .resolved_at
                    .is_some_and(|r| now.saturating_sub(r) > retention))
        });
        (before - self.items.len()) as u64
    }

    pub fn reinject(&mut self, now: u64, qid: &str) -> UcResult<Cbor> {
        let item = self
            .items
            .get_mut(qid)
            .ok_or_else(|| UcError::not_found(format!("quarantine item {qid}")))?;
        if item.status != QuarantineStatus::Pending {
            return Err(UcError::schema(format!(
                "quarantine item {qid} already {}",
                item.status.as_str()
            )));
        }
        item.status = QuarantineStatus::Reinjected;
        item.resolved_at = Some(now);
        Ok(item.payload.clone())
    }

    pub fn reject(&mut self, now: u64, qid: &str) -> UcResult<()> {
        let item = self
            .items
            .get_mut(qid)
            .ok_or_else(|| UcError::not_found(format!("quarantine item {qid}")))?;
        if item.status != QuarantineStatus::Pending {
            return Err(UcError::schema(format!(
                "quarantine item {qid} already {}",
                item.status.as_str()
            )));
        }
        item.status = QuarantineStatus::Rejected;
        item.resolved_at = Some(now);
        Ok(())
    }

    pub fn pending(&self) -> Vec<&QuarantineItem> {
        self.items
            .values()
            .filter(|i| i.status == QuarantineStatus::Pending)
            .collect()
    }

    pub fn list(&self) -> Vec<&QuarantineItem> {
        self.items.values().collect()
    }

    pub fn get(&self, qid: &str) -> Option<&QuarantineItem> {
        self.items.get(qid)
    }

    pub fn pending_count(&self) -> usize {
        self.pending().len()
    }
}

impl CellBehavior for QuarantineCell {
    fn cell_id(&self) -> CellId {
        self.id
    }
    fn cell_type(&self) -> CellType {
        CellType::Quarantine
    }
    fn schema_id(&self) -> SchemaId {
        SchemaId::new("trinity.quarantine.v1")
    }

    fn on_query(&self, _at: u64, query: &Cbor) -> UcResult<Cbor> {
        let only_pending = query.opt_bool("pending").unwrap_or(true);
        let items: Vec<Cbor> = self
            .items
            .values()
            .filter(|i| !only_pending || i.status == QuarantineStatus::Pending)
            .map(|i| {
                Cbor::map(vec![
                    ("qid", Cbor::t(i.qid.clone())),
                    ("absorbed_at", Cbor::U64(i.absorbed_at)),
                    ("cause", Cbor::t(i.cause.as_str())),
                    ("detail", Cbor::t(i.detail.clone())),
                    ("status", Cbor::t(i.status.as_str())),
                ])
            })
            .collect();
        Ok(Cbor::map(vec![("items", Cbor::Array(items))]))
    }

    fn on_update(&mut self, at: u64, update: &Cbor) -> UcResult<Cbor> {
        match update.opt_str("op").as_deref() {
            Some("reinject") => {
                let qid = update.req_str("qid")?;
                let payload = self.reinject(at, &qid)?;
                Ok(Cbor::map(vec![
                    ("reinjected", Cbor::t(qid)),
                    ("payload", payload),
                ]))
            }
            Some("reject") => {
                let qid = update.req_str("qid")?;
                self.reject(at, &qid)?;
                Ok(Cbor::map(vec![("rejected", Cbor::t(qid))]))
            }
            _ => Err(UcError::schema("quarantine: unknown update op")),
        }
    }

    fn snapshot_state(&self) -> Cbor {
        let items: Vec<Cbor> = self
            .items
            .values()
            .map(|i| {
                Cbor::map(vec![
                    ("qid", Cbor::t(i.qid.clone())),
                    ("absorbed_at", Cbor::U64(i.absorbed_at)),
                    ("cause", Cbor::t(i.cause.as_str())),
                    ("detail", Cbor::t(i.detail.clone())),
                    ("payload", i.payload.clone()),
                    ("status", Cbor::t(i.status.as_str())),
                    (
                        "resolved_at",
                        i.resolved_at.map(Cbor::U64).unwrap_or(Cbor::Null),
                    ),
                ])
            })
            .collect();
        Cbor::map(vec![
            ("items", Cbor::Array(items)),
            ("resolved_retention", Cbor::U64(self.resolved_retention)),
        ])
    }

    fn restore_state(&mut self, state: &Cbor) -> UcResult<()> {
        self.items.clear();
        self.resolved_retention = state.opt_u64("resolved_retention").unwrap_or(1_000_000);
        if let Some(arr) = state.get("items").and_then(|v| v.as_array()) {
            for item in arr {
                let status = match item.opt_str("status").as_deref() {
                    Some("reinjected") => QuarantineStatus::Reinjected,
                    Some("rejected") => QuarantineStatus::Rejected,
                    _ => QuarantineStatus::Pending,
                };
                let cause = match item.opt_str("cause").as_deref() {
                    Some(s) => ErrCode::from_str(s).unwrap_or(ErrCode::Internal),
                    None => ErrCode::Internal,
                };
                let q = QuarantineItem {
                    qid: item.req_str("qid")?,
                    absorbed_at: item.opt_u64("absorbed_at").unwrap_or(0),
                    cause,
                    detail: item.opt_str("detail").unwrap_or_default(),
                    payload: item.get("payload").cloned().unwrap_or(Cbor::Null),
                    status,
                    resolved_at: item.opt_u64("resolved_at"),
                };
                self.items.insert(q.qid.clone(), q);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_required_fields_and_deprecation() {
        let mut cc = ContractCell::new(CellId(20));
        cc.register(1, "fact.v1", vec!["subject".into(), "predicate".into(), "object".into()]);
        let ok = Cbor::map(vec![
            ("subject", Cbor::t("a")),
            ("predicate", Cbor::t("b")),
            ("object", Cbor::t("c")),
        ]);
        assert!(cc.validate_schema("fact.v1", &ok).is_ok());
        let missing = Cbor::map(vec![("subject", Cbor::t("a"))]);
        let err = cc.validate_schema("fact.v1", &missing).unwrap_err();
        assert_eq!(err.code, ErrCode::ContractViolation);
        // Unknown schema: pass (opt-in).
        assert!(cc.validate_schema("never.registered", &missing).is_ok());
        cc.deprecate("fact.v1").unwrap();
        assert!(cc.validate_schema("fact.v1", &ok).is_err());
    }

    #[test]
    fn spec_anchor_gate() {
        let mut sa = SpecAnchorCell::new(CellId(21));
        sa.register("Architecture.md", "4");
        let good = AnchorRef::new("Architecture.md", "4");
        let bad = AnchorRef::new("Architecture.md", "999");
        assert!(sa.validate(CellType::Fact, Some(&good)).is_ok());
        assert_eq!(
            sa.validate(CellType::Fact, Some(&bad)).unwrap_err().code,
            ErrCode::AnchorMissing
        );
        assert_eq!(
            sa.validate(CellType::Fact, None).unwrap_err().code,
            ErrCode::AnchorMissing
        );
        // Exemption for working memory.
        assert!(sa.validate(CellType::Scratchpad, None).is_ok());
        assert!(sa.validate(CellType::Cache, None).is_ok());
        // Parse both § and # forms.
        assert_eq!(
            SpecAnchorCell::parse_anchor("Doc.md\u{00a7}3").unwrap(),
            AnchorRef::new("Doc.md", "3")
        );
        assert_eq!(
            SpecAnchorCell::parse_anchor("Doc.md#3").unwrap(),
            AnchorRef::new("Doc.md", "3")
        );
    }

    #[test]
    fn decision_conflict_detection() {
        let mut dl = DecisionLedgerCell::new(CellId(22));
        let d1 = dl.append(1, 9, "embedder.dim", "dim is 768", "operator", "EmbeddingReranker.md§2");
        // Payload in that scope with no reference → conflict.
        let bare = Cbor::map(vec![
            ("decision_scope", Cbor::t("embedder.dim")),
            ("proposal", Cbor::t("switch to 1536")),
        ]);
        assert_eq!(
            dl.check_conflicts(&bare).unwrap_err().code,
            ErrCode::DecisionConflict
        );
        // Respecting the decision passes.
        let respects = Cbor::map(vec![
            ("decision_scope", Cbor::t("embedder.dim")),
            ("respects_decision", Cbor::t(d1.clone())),
        ]);
        assert!(dl.check_conflicts(&respects).is_ok());
        // Explicit supersede passes and then flips governance.
        let d2 = dl.append(5, 9, "embedder.dim", "dim is 1536", "operator", "EmbeddingReranker.md§2");
        dl.supersede(&d1, &d2).unwrap();
        let active = dl.active_in_scope("embedder.dim");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].handle, d2);
        // Scope-free payloads pass.
        assert!(dl.check_conflicts(&Cbor::map(vec![("x", Cbor::U64(1))])).is_ok());
    }

    #[test]
    fn budget_charge_and_exhaustion() {
        let mut wb = WorkBudgetCell::new(CellId(23));
        wb.ensure("task-1", Some(100));
        assert!(wb.charge_pre("task-1", 60).is_ok());
        // Reserved 60 of 100 → a further 50 exceeds.
        assert_eq!(
            wb.charge_pre("task-1", 50).unwrap_err().code,
            ErrCode::BudgetExceeded
        );
        // Reconcile at actual 40: 60 unreserved, 40 spent → 60 available.
        assert_eq!(wb.charge_post("task-1", 60, 40), 0);
        assert_eq!(wb.get("task-1").unwrap().available(), 60);
        assert!(wb.charge_pre("task-1", 50).is_ok());
        // Overrun clamps and reports.
        assert_eq!(wb.charge_post("task-1", 50, 55), 5);
        // Zero-budget task: everything fails pre-charge (conformance T5).
        wb.ensure("task-zero", Some(0));
        assert_eq!(
            wb.charge_pre("task-zero", 1).unwrap_err().code,
            ErrCode::BudgetExceeded
        );
    }

    #[test]
    fn congruence_blocks_unknown_entities() {
        let mut cg = CongruenceCell::new(CellId(24));
        cg.register_entity("QuarantineCell");
        cg.register_entity("GAP-011");
        cg.register_entity("P19");
        let ok = Cbor::map(vec![(
            "note",
            Cbor::t("the QuarantineCell honors P19 per GAP-011"),
        )]);
        assert!(cg.preview_delta(&ok).is_ok());
        let bad = Cbor::map(vec![(
            "note",
            Cbor::t("introduce the TelepathyCell per GAP-999 and P42"),
        )]);
        let err = cg.preview_delta(&bad).unwrap_err();
        assert_eq!(err.code, ErrCode::CongruenceDelta);
        assert!(err.message.contains("TelepathyCell"));
        assert!(err.message.contains("GAP-999"));
        assert!(err.message.contains("P42"));
        // Severities P0..P2 are not treated as principle entities.
        let sev = Cbor::map(vec![("note", Cbor::t("escalate to P0 immediately"))]);
        assert!(cg.preview_delta(&sev).is_ok());
        // Accepting the delta unblocks.
        cg.accept_delta("TelepathyCell");
        cg.accept_delta("GAP-999");
        cg.accept_delta("P42");
        assert!(cg.preview_delta(&bad).is_ok());
    }

    #[test]
    fn gap_fixation_at_n_plus_one() {
        let mut gc = GapCell::new(CellId(25));
        gc.register(1, "GAP-777", "unknown latency source");
        // N=8 dispatches succeed.
        for _ in 0..GAP_FIXATION_N {
            assert!(gc.on_dispatch("GAP-777").is_ok());
        }
        // Dispatch N+1 trips fixation.
        let err = gc.on_dispatch("GAP-777").unwrap_err();
        assert_eq!(err.code, ErrCode::Fixation);
        assert_eq!(gc.get("GAP-777").unwrap().state, GapState::Fixated);
        // Still blocked until a transition.
        assert_eq!(gc.on_dispatch("GAP-777").unwrap_err().code, ErrCode::Fixation);
        gc.transition("GAP-777", GapState::Investigating).unwrap();
        assert!(gc.on_dispatch("GAP-777").is_ok());
    }

    #[test]
    fn quarantine_never_silently_drops() {
        let mut qc = QuarantineCell::new(CellId(26));
        qc.resolved_retention = 10;
        let q1 = qc.absorb(1, 7, ErrCode::AnchorMissing, "no anchor", Cbor::t("env-1"));
        let q2 = qc.absorb(2, 7, ErrCode::BudgetExceeded, "broke", Cbor::t("env-2"));
        assert_eq!(qc.pending_count(), 2);
        // Sweep never touches pending items, no matter how old.
        assert_eq!(qc.sweep(1_000_000), 0);
        assert_eq!(qc.pending_count(), 2);
        // Reinject returns the original payload.
        let payload = qc.reinject(5, &q1).unwrap();
        assert_eq!(payload, Cbor::t("env-1"));
        assert!(qc.reinject(6, &q1).is_err()); // no double resolution
        qc.reject(6, &q2).unwrap();
        assert_eq!(qc.pending_count(), 0);
        // Resolved items prune only after retention.
        assert_eq!(qc.sweep(10), 0);
        assert_eq!(qc.sweep(100), 2);
    }
}
