//! Core vocabulary: identifiers, enums, errors, logical clock, RNG.
//!
//! SPEC-DERIVED-§3 (Architecture.md): vocabulary. SPEC-DERIVED-§8 (McpProtocol.md): error codes.

pub mod cbor;
pub mod crypto;
pub mod glob;
pub mod minitoml;
pub mod ulid;

pub use ulid::{DetRng, Ulid};

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// Universal substrate reference. Canonical string form, e.g. `fact/<ulid>`,
/// `blob/<sha256hex>`, `librarian/output/<ulid>/rationale`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Handle(pub String);

impl Handle {
    pub fn new(s: impl Into<String>) -> Self {
        Handle(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Display for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Numeric cell identifier — matches the `cell_id u64` field in WAL frames
/// (PersistenceLayer.md §3.1) and the Catalog registry keys.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CellId(pub u64);
impl CellId {
    pub fn as_u64(&self) -> u64 {
        self.0
    }
}
impl fmt::Display for CellId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cell:{}", self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SchemaId(pub String);
impl SchemaId {
    pub fn new(s: impl Into<String>) -> Self {
        SchemaId(s.into())
    }
}
impl fmt::Display for SchemaId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NamespaceId(pub String);
impl NamespaceId {
    pub fn new(s: impl Into<String>) -> Self {
        NamespaceId(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GapId(pub String);
impl GapId {
    pub fn new(s: impl Into<String>) -> Self {
        GapId(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// `SPEC-DERIVED` anchor reference: (doc, section).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AnchorRef {
    pub doc: String,
    pub section: String,
}
impl AnchorRef {
    pub fn new(doc: impl Into<String>, section: impl Into<String>) -> Self {
        AnchorRef {
            doc: doc.into(),
            section: section.into(),
        }
    }
    pub fn key(&self) -> String {
        format!("{}\u{00a7}{}", self.doc, self.section)
    }
}

pub type AgentId = Ulid;
pub type TaskId = Ulid;
pub type RequestId = Ulid;
pub type QuarantineId = Ulid;
pub type DecisionId = Ulid;

// ---------------------------------------------------------------------------
// Severity / Intent / Tier
// ---------------------------------------------------------------------------

/// SPEC-DERIVED-§6 (RouterScheduler.md): severity-aware routing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    P0,
    P1,
    P2,
}
impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::P0 => "P0",
            Severity::P1 => "P1",
            Severity::P2 => "P2",
        }
    }
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "P0" => Some(Severity::P0),
            "P1" => Some(Severity::P1),
            "P2" => Some(Severity::P2),
            _ => None,
        }
    }
}

/// SPEC-DERIVED-§4 (RouterScheduler.md) intent set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Intent {
    Recall,
    Hydrate,
    Write,
    Subscribe,
    View,
    Supersede,
    /// Operator plane. Not part of the agent-facing verb set; gated by an
    /// `admin` op in the capability token. See bootstrap::admin.
    Admin,
}
impl Intent {
    pub fn as_str(self) -> &'static str {
        match self {
            Intent::Recall => "recall",
            Intent::Hydrate => "hydrate",
            Intent::Write => "write",
            Intent::Subscribe => "subscribe",
            Intent::View => "view",
            Intent::Supersede => "supersede",
            Intent::Admin => "admin",
        }
    }
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "recall" => Some(Intent::Recall),
            "hydrate" => Some(Intent::Hydrate),
            "write" => Some(Intent::Write),
            "subscribe" => Some(Intent::Subscribe),
            "view" => Some(Intent::View),
            "supersede" => Some(Intent::Supersede),
            "admin" => Some(Intent::Admin),
            _ => None,
        }
    }
    pub fn is_state_changing(self) -> bool {
        matches!(self, Intent::Write | Intent::Supersede)
    }
}

/// SPEC-DERIVED-§5 (RouterScheduler.md): token-budget tier policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Tier {
    L0,
    L1,
    L2,
    L3,
}
impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::L0 => "L0",
            Tier::L1 => "L1",
            Tier::L2 => "L2",
            Tier::L3 => "L3",
        }
    }
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "L0" => Some(Tier::L0),
            "L1" => Some(Tier::L1),
            "L2" => Some(Tier::L2),
            "L3" => Some(Tier::L3),
            _ => None,
        }
    }
    pub fn next(self) -> Option<Tier> {
        match self {
            Tier::L0 => Some(Tier::L1),
            Tier::L1 => Some(Tier::L2),
            Tier::L2 => Some(Tier::L3),
            Tier::L3 => None,
        }
    }
}

/// Bootstrap/runtime shard-placement policy surfaced to operators.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ShardTopology {
    Dedicated,
    CoTenantShard0,
}
impl ShardTopology {
    pub fn as_str(self) -> &'static str {
        match self {
            ShardTopology::Dedicated => "dedicated",
            ShardTopology::CoTenantShard0 => "co-tenant-shard-0",
        }
    }
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "dedicated" => Some(ShardTopology::Dedicated),
            "co-tenant-shard-0" => Some(ShardTopology::CoTenantShard0),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Error model — SPEC-DERIVED-§8 (McpProtocol.md) + UltraCortex §A.2 codes.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrCode {
    Quarantined,
    BudgetExceeded,
    Fixation,
    AnchorMissing,
    ContractViolation,
    DecisionConflict,
    CongruenceDelta,
    PermissionDenied,
    RateLimited,
    DeadlineExceeded,
    NotFound,
    Unimplemented,
    Internal,
    // UltraCortex v1.0 — Curator error codes.
    SemanticDrift,
    HallucinationDetected,
    AdjudicationPending,
}

impl ErrCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrCode::Quarantined => "Quarantined",
            ErrCode::BudgetExceeded => "BudgetExceeded",
            ErrCode::Fixation => "Fixation",
            ErrCode::AnchorMissing => "AnchorMissing",
            ErrCode::ContractViolation => "ContractViolation",
            ErrCode::DecisionConflict => "DecisionConflict",
            ErrCode::CongruenceDelta => "CongruenceDelta",
            ErrCode::PermissionDenied => "PermissionDenied",
            ErrCode::RateLimited => "RateLimited",
            ErrCode::DeadlineExceeded => "DeadlineExceeded",
            ErrCode::NotFound => "NotFound",
            ErrCode::Unimplemented => "Unimplemented",
            ErrCode::Internal => "Internal",
            ErrCode::SemanticDrift => "SemanticDrift",
            ErrCode::HallucinationDetected => "HallucinationDetected",
            ErrCode::AdjudicationPending => "AdjudicationPending",
        }
    }
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "Quarantined" => ErrCode::Quarantined,
            "BudgetExceeded" => ErrCode::BudgetExceeded,
            "Fixation" => ErrCode::Fixation,
            "AnchorMissing" => ErrCode::AnchorMissing,
            "ContractViolation" => ErrCode::ContractViolation,
            "DecisionConflict" => ErrCode::DecisionConflict,
            "CongruenceDelta" => ErrCode::CongruenceDelta,
            "PermissionDenied" => ErrCode::PermissionDenied,
            "RateLimited" => ErrCode::RateLimited,
            "DeadlineExceeded" => ErrCode::DeadlineExceeded,
            "NotFound" => ErrCode::NotFound,
            "Unimplemented" => ErrCode::Unimplemented,
            "Internal" => ErrCode::Internal,
            "SemanticDrift" => ErrCode::SemanticDrift,
            "HallucinationDetected" => ErrCode::HallucinationDetected,
            "AdjudicationPending" => ErrCode::AdjudicationPending,
            _ => return None,
        })
    }
    /// Trinity-originated per McpProtocol.md §8 table (+ Curator codes,
    /// which are Trinity-governed per P20).
    pub fn is_trinity(self) -> bool {
        matches!(
            self,
            ErrCode::Quarantined
                | ErrCode::BudgetExceeded
                | ErrCode::Fixation
                | ErrCode::AnchorMissing
                | ErrCode::ContractViolation
                | ErrCode::DecisionConflict
                | ErrCode::CongruenceDelta
                | ErrCode::SemanticDrift
                | ErrCode::HallucinationDetected
                | ErrCode::AdjudicationPending
        )
    }
}

/// Structured error per McpProtocol.md §8. Never a silent drop: Trinity
/// failures carry a `quarantine_id`.
#[derive(Clone, Debug)]
pub struct UcError {
    pub code: ErrCode,
    pub message: String,
    pub quarantine_id: Option<QuarantineId>,
    pub cause_chain: Vec<ErrCode>,
    pub retry_after_logical: Option<u64>,
    pub spec_anchor: Option<Box<AnchorRef>>,
}

impl UcError {
    pub fn new(code: ErrCode, message: impl Into<String>) -> Self {
        UcError {
            code,
            message: message.into(),
            quarantine_id: None,
            cause_chain: Vec::new(),
            retry_after_logical: None,
            spec_anchor: None,
        }
    }
    pub fn internal(message: impl Into<String>) -> Self {
        UcError::new(ErrCode::Internal, message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        UcError::new(ErrCode::NotFound, message)
    }
    pub fn denied(message: impl Into<String>) -> Self {
        UcError::new(ErrCode::PermissionDenied, message)
    }
    pub fn unsupported(message: impl Into<String>) -> Self {
        UcError::new(ErrCode::Unimplemented, message)
    }
    /// Schema / contract shape violation (payload doesn't match its
    /// registered ContractCell schema).
    pub fn schema(message: impl Into<String>) -> Self {
        UcError::new(ErrCode::ContractViolation, message)
    }
    pub fn with_quarantine(mut self, qid: QuarantineId) -> Self {
        self.quarantine_id = Some(qid);
        self
    }
    pub fn with_cause(mut self, cause: ErrCode) -> Self {
        self.cause_chain.push(cause);
        self
    }
    pub fn with_anchor(mut self, a: AnchorRef) -> Self {
        self.spec_anchor = Some(Box::new(a));
        self
    }
}

impl fmt::Display for UcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}
impl std::error::Error for UcError {}

impl From<std::io::Error> for UcError {
    fn from(e: std::io::Error) -> Self {
        UcError::internal(format!("io: {e}"))
    }
}

pub type UcResult<T> = Result<T, UcError>;

// ---------------------------------------------------------------------------
// Logical clock — SPEC-DERIVED-§4 (Architecture.md). Wall-clock reads inside
// Cell `on_update` are forbidden (Invariant I5, CellTaxonomy.md §1.4).
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct LogicalClock(AtomicU64);

impl LogicalClock {
    pub fn new(start: u64) -> Self {
        LogicalClock(AtomicU64::new(start))
    }
    /// Advance and return the new value. Monotonic.
    pub fn tick(&self) -> u64 {
        self.0.fetch_add(1, Ordering::SeqCst) + 1
    }
    pub fn now(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
    /// Advance to at least `t` (used on WAL replay).
    pub fn advance_to(&self, t: u64) {
        let mut cur = self.0.load(Ordering::SeqCst);
        while cur < t {
            match self
                .0
                .compare_exchange(cur, t, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
    }
}

/// FNV-1a 64-bit — stable, seed-free hash for frame `cell_id` fields and
/// deterministic routing (no hash randomization; RouterScheduler.md §14).
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Rough token estimator (bytes/4) used by WorkBudget charging.
/// SPEC-DERIVED-§9.1 (NATIVE_TRINITY.md): consistent under-estimation is
/// reconciled by `charge_post`.
pub fn est_tokens(bytes: usize) -> u32 {
    bytes.div_ceil(4) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_monotonic() {
        let c = LogicalClock::new(0);
        let a = c.tick();
        let b = c.tick();
        assert!(b > a);
        c.advance_to(100);
        assert!(c.now() >= 100);
        c.advance_to(5);
        assert!(c.now() >= 100);
    }

    #[test]
    fn errcode_roundtrip() {
        for c in [
            ErrCode::Quarantined,
            ErrCode::SemanticDrift,
            ErrCode::HallucinationDetected,
            ErrCode::AdjudicationPending,
            ErrCode::Internal,
        ] {
            assert_eq!(ErrCode::from_str(c.as_str()), Some(c));
        }
        assert!(ErrCode::SemanticDrift.is_trinity());
        assert!(!ErrCode::RateLimited.is_trinity());
    }

    #[test]
    fn fnv_stable() {
        assert_eq!(fnv1a64(b""), 0xcbf29ce484222325);
        assert_ne!(fnv1a64(b"fact-0"), fnv1a64(b"fact-1"));
    }
}
