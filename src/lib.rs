//! UltraCortex v1.0 — a self-policing shared-memory substrate for
//! multi-agent AI systems. Zero external dependencies (std only).
//!
//! Layer map (mirrors the 9-crate plan as modules of one package —
//! IMPLEMENTATION_STATUS.md §2):
//!
//! | module      | blueprint crate | contents                               |
//! |-------------|-----------------|----------------------------------------|
//! | [`core`]    | uc-core         | ids, errors, ULID, CBOR, glob, TOML    |
//! | [`obs`]     | uc-obs          | metrics, structured log, audit chain   |
//! | [`persist`] | uc-persist      | WAL, CAS, snapshots, KMS, view cache   |
//! | [`cells`]   | uc-cells        | the 14 memory/index/coordination cells |
//! | [`trinity`] | uc-trinity      | 7 governance cells + validation chain  |
//! | [`curator`] | uc-curator      | Librarian/Warden/Adjudicator + ledger  |
//! | [`router`]  | uc-router       | tokens, envelopes, views, events, dispatch |
//! | [`proto`]   | uc-proto        | u32-LE CBOR wire, UDS/TCP, client      |
//! | [`node`]    | uc-node         | the state container                    |
//! | [`bootstrap`]| uc-node        | B1–B6 operator + admin plane           |
//! | [`deepseek`]| uc-client-deepseek | FIM, R1 strip, tools manifest       |

pub mod bootstrap;
pub mod cells;
pub mod core;
pub mod curator;
pub mod deepseek;
pub mod node;
pub mod obs;
pub mod persist;
pub mod proto;
pub mod router;
pub mod trinity;

pub use crate::core::{ErrCode, Intent, Severity, Tier, UcError, UcResult};
pub use crate::node::Node;
pub use crate::router::envelope::{Envelope, ResponseEnvelope, PROTO_VERSION};
pub use crate::router::handle_envelope;

/// Spec-anchor inventory harvested from source comments by build.rs:
/// `&[(doc, section, artifact_path:line, line)]`. The Bootstrap Operator
/// registers these in B3a; conformance test T1 checks every entry resolves.
pub mod spec_inventory {
    include!(concat!(env!("OUT_DIR"), "/spec_anchor_inventory.rs"));
}
