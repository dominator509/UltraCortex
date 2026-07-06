//! Native Trinity — SPEC-DERIVED-§3 (NATIVE_TRINITY.md).
//!
//! The Trinity is provisioned before any other cell (Bootstrap B3, ordering
//! is fatal if violated) and its pre-validation chain runs on every
//! state-changing envelope in the fixed order defined in
//! RouterScheduler.md §B. See [`chain::run_pre_validation`].

pub mod cells;
pub mod chain;

pub use cells::{
    ContractCell, CongruenceCell, DecisionLedgerCell, Gap, GapCell, GapState, QuarantineCell,
    QuarantineStatus, SpecAnchorCell, WorkBudgetCell, GAP_FIXATION_N,
};
pub use chain::{run_pre_validation, PreCtx, Trinity};
