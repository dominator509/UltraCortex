//! CrossCheckLedgerCell — SPEC-DERIVED-§2–§7 (CrossCheckLedgerCell.md).
//!
//! Append-only record of every curator↔curator interaction, persisted on
//! its **own WAL stream** (`wal/cross_check/`, frames marked
//! [`crate::persist::wal::FLAG_CROSS_CHECK`]) so curator forensics survive
//! independently of shard WALs.
//!
//! The ledger is the collusion tripwire: over a rolling window of W=200
//! audits, the Librarian↔Warden agreement rate must sit inside the
//! disagreement quota band (agreement 92–97%, i.e. the Warden *should* be
//! finding real problems 3–8% of the time):
//! - agreement > 99% → `curator.suspicious_agreement` + probe-rate boost
//!   (rubber-stamping / collusion signal);
//! - agreement < 92% → `curator.miscalibration` (the pair is thrashing).
//!
//! Every 256 records at encryption tier T2+, a batch HMAC signature over
//! the record hashes is appended and persisted beside the dedicated WAL
//! stream (tamper-evidence beyond the WAL CRCs).
//!
//! **The Adjudicator never reads this cell** — enforced structurally
//! (adjudicator.rs takes no ledger reference) and by capability token
//! (`!cross_check/**` in the adjudicator's facet scope).

use crate::cells::{CellBehavior, CellType};
use crate::core::cbor::Cbor;
use crate::core::crypto::{hex, sha256};
use crate::core::{CellId, SchemaId, UcError, UcResult};
use crate::obs::Metrics;
use crate::persist::wal::{
    payload_nonce, payload_purpose, WalFrame, WalOp, WalWriter, FLAG_CROSS_CHECK,
};
use crate::persist::{BatchSignature, Kms};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

pub const AGREEMENT_WINDOW: usize = 200;
pub const BATCH_SIGN_EVERY: u64 = 256;
pub const PROBE_BOOST_ON_SUSPICIOUS: f64 = 10.0;
pub const RETENTION_POLICY: &str = "indefinite";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrossCheckKind {
    LibrarianOutput,
    WardenAudit,
    WardenFlag,
    LibrarianSanity,
    Adjudication,
    Probe,
    BlindReaudit,
}

impl CrossCheckKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CrossCheckKind::LibrarianOutput => "librarian_output",
            CrossCheckKind::WardenAudit => "warden_audit",
            CrossCheckKind::WardenFlag => "warden_flag",
            CrossCheckKind::LibrarianSanity => "librarian_sanity",
            CrossCheckKind::Adjudication => "adjudication",
            CrossCheckKind::Probe => "probe",
            CrossCheckKind::BlindReaudit => "blind_reaudit",
        }
    }
    pub fn parse(s: &str) -> Option<CrossCheckKind> {
        Some(match s {
            "librarian_output" => CrossCheckKind::LibrarianOutput,
            "warden_audit" => CrossCheckKind::WardenAudit,
            "warden_flag" => CrossCheckKind::WardenFlag,
            "librarian_sanity" => CrossCheckKind::LibrarianSanity,
            "adjudication" => CrossCheckKind::Adjudication,
            "probe" => CrossCheckKind::Probe,
            "blind_reaudit" => CrossCheckKind::BlindReaudit,
            _ => return None,
        })
    }
}

/// Outcome from the *initiator's perspective* — for a WardenAudit record,
/// `Agree` means the audit passed (Warden agreed the output is sound).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrossCheckOutcome {
    Agree,
    Disagree,
    Escalated,
}

impl CrossCheckOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            CrossCheckOutcome::Agree => "agree",
            CrossCheckOutcome::Disagree => "disagree",
            CrossCheckOutcome::Escalated => "escalated",
        }
    }
    pub fn parse(s: &str) -> Option<CrossCheckOutcome> {
        Some(match s {
            "agree" => CrossCheckOutcome::Agree,
            "disagree" => CrossCheckOutcome::Disagree,
            "escalated" => CrossCheckOutcome::Escalated,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug)]
pub struct CrossCheckRecord {
    pub seq: u64,
    pub kind: CrossCheckKind,
    pub initiator: String, // handle of the initiating artifact
    pub auditor: String,   // handle of the auditing artifact ("" if n/a)
    pub outcome: CrossCheckOutcome,
    pub adjudication: Option<String>,
    pub logical_at: u64,
}

impl CrossCheckRecord {
    pub fn to_cbor(&self) -> Cbor {
        Cbor::map(vec![
            ("seq", Cbor::U64(self.seq)),
            ("kind", Cbor::t(self.kind.as_str())),
            ("initiator", Cbor::t(self.initiator.clone())),
            ("auditor", Cbor::t(self.auditor.clone())),
            ("outcome", Cbor::t(self.outcome.as_str())),
            (
                "adjudication",
                self.adjudication
                    .as_ref()
                    .map(|a| Cbor::t(a.clone()))
                    .unwrap_or(Cbor::Null),
            ),
            ("logical_at", Cbor::U64(self.logical_at)),
        ])
    }

    pub fn from_cbor(c: &Cbor) -> UcResult<CrossCheckRecord> {
        Ok(CrossCheckRecord {
            seq: c.req_u64("seq")?,
            kind: CrossCheckKind::parse(&c.req_str("kind")?)
                .ok_or_else(|| UcError::schema("bad cross-check kind"))?,
            initiator: c.opt_str("initiator").unwrap_or_default(),
            auditor: c.opt_str("auditor").unwrap_or_default(),
            outcome: CrossCheckOutcome::parse(&c.opt_str("outcome").unwrap_or_default())
                .unwrap_or(CrossCheckOutcome::Escalated),
            adjudication: c.opt_str("adjudication"),
            logical_at: c.opt_u64("logical_at").unwrap_or(0),
        })
    }
}

/// Agreement-band health over the rolling window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgreementHealth {
    Healthy,
    SuspiciousAgreement, // > 99% — collusion signal
    Miscalibration,      // < 92%
    InsufficientData,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchSignatureRecord {
    pub last_seq: u64,
    pub key_id: u64,
    pub hmac_hex: String,
}

impl BatchSignatureRecord {
    fn to_cbor(&self) -> Cbor {
        Cbor::map(vec![
            ("last_seq", Cbor::U64(self.last_seq)),
            ("key_id", Cbor::U64(self.key_id)),
            ("hmac", Cbor::t(self.hmac_hex.clone())),
        ])
    }

    fn from_cbor(c: &Cbor) -> UcResult<BatchSignatureRecord> {
        Ok(BatchSignatureRecord {
            last_seq: c.opt_u64("last_seq").unwrap_or(0),
            key_id: c.opt_u64("key_id").unwrap_or(0),
            hmac_hex: c.opt_str("hmac").unwrap_or_default(),
        })
    }
}

pub struct CrossCheckLedgerCell {
    pub id: CellId,
    records: Vec<CrossCheckRecord>,
    by_initiator: BTreeMap<String, Vec<u64>>,
    by_outcome: BTreeMap<&'static str, u64>,
    next_seq: u64,
    /// Rolling window of WardenAudit outcomes (true = Agree).
    audit_window: std::collections::VecDeque<bool>,
    pub quota_low: f64,
    pub quota_high: f64,
    /// Optional own WAL stream + KMS for batch signing (absent in tests).
    wal: Option<Arc<WalWriter>>,
    kms: Option<Arc<Kms>>,
    batch_hashes: Vec<[u8; 32]>,
    pub batch_signatures: Vec<BatchSignatureRecord>,
}

impl CrossCheckLedgerCell {
    pub fn new(id: CellId) -> Self {
        CrossCheckLedgerCell {
            id,
            records: Vec::new(),
            by_initiator: BTreeMap::new(),
            by_outcome: BTreeMap::new(),
            next_seq: 0,
            audit_window: std::collections::VecDeque::with_capacity(AGREEMENT_WINDOW),
            quota_low: 0.92,
            quota_high: 0.97,
            wal: None,
            kms: None,
            batch_hashes: Vec::new(),
            batch_signatures: Vec::new(),
        }
    }

    pub fn attach_persistence(&mut self, wal: Arc<WalWriter>, kms: Arc<Kms>) {
        self.wal = Some(wal);
        self.kms = Some(kms);
    }

    /// Build the next ledger record without making it visible. The Router
    /// durably commits the record through Node before calling apply_record.
    #[allow(clippy::too_many_arguments)]
    pub fn next_record(
        &self,
        logical_at: u64,
        kind: CrossCheckKind,
        initiator: &str,
        auditor: &str,
        outcome: CrossCheckOutcome,
        adjudication: Option<String>,
    ) -> CrossCheckRecord {
        CrossCheckRecord {
            seq: self.next_seq,
            kind,
            initiator: initiator.to_string(),
            auditor: auditor.to_string(),
            outcome,
            adjudication,
            logical_at,
        }
    }

    /// Apply a record whose WAL transaction has already committed. This is
    /// deliberately separate from append so an audit/WAL failure cannot leave
    /// a visible ledger record that recovery would later replay differently.
    pub fn apply_record(&mut self, metrics: &Metrics, rec: CrossCheckRecord) -> UcResult<u64> {
        if rec.seq < self.next_seq {
            return Ok(rec.seq);
        }
        if rec.seq != self.next_seq {
            return Err(UcError::internal(format!(
                "cross-check record sequence gap: expected {}, got {}",
                self.next_seq, rec.seq
            )));
        }
        let bytes = rec.to_cbor().encode();
        self.batch_hashes.push(sha256(&bytes));

        self.by_initiator
            .entry(rec.initiator.clone())
            .or_default()
            .push(rec.seq);
        *self.by_outcome.entry(rec.outcome.as_str()).or_insert(0) += 1;
        if rec.kind == CrossCheckKind::WardenAudit {
            if self.audit_window.len() == AGREEMENT_WINDOW {
                self.audit_window.pop_front();
            }
            self.audit_window
                .push_back(rec.outcome == CrossCheckOutcome::Agree);
            match self.health() {
                AgreementHealth::SuspiciousAgreement => metrics.inc("curator.suspicious_agreement"),
                AgreementHealth::Miscalibration => metrics.inc("curator.miscalibration"),
                _ => {}
            }
        }
        metrics.inc("cross_check.records");

        if self.next_seq % BATCH_SIGN_EVERY == BATCH_SIGN_EVERY - 1 {
            self.sign_batch()?;
        }

        self.records.push(rec);
        self.next_seq += 1;
        Ok(self.next_seq - 1)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn append(
        &mut self,
        metrics: &Metrics,
        logical_at: u64,
        kind: CrossCheckKind,
        initiator: &str,
        auditor: &str,
        outcome: CrossCheckOutcome,
        adjudication: Option<String>,
    ) -> UcResult<u64> {
        let rec = CrossCheckRecord {
            seq: self.next_seq,
            kind,
            initiator: initiator.to_string(),
            auditor: auditor.to_string(),
            outcome,
            adjudication,
            logical_at,
        };
        // Own WAL stream first — durability before visibility (§5).
        let bytes = rec.to_cbor().encode();
        if let Some(wal) = &self.wal {
            let payload = if let Some(kms) = &self.kms {
                kms.seal(
                    &payload_purpose(self.id.0, FLAG_CROSS_CHECK),
                    payload_nonce(logical_at, self.id.0, WalOp::CrossCheck, &bytes),
                    &bytes,
                )
            } else {
                bytes.clone()
            };
            wal.append(&WalFrame {
                logical_at,
                cell_id: self.id.0,
                op: WalOp::CrossCheck,
                schema_ver: 1,
                flags: FLAG_CROSS_CHECK,
                payload,
            })
            .map_err(UcError::internal)?;
        }
        self.batch_hashes.push(sha256(&bytes));

        // Indices + window.
        self.by_initiator
            .entry(rec.initiator.clone())
            .or_default()
            .push(rec.seq);
        *self.by_outcome.entry(outcome.as_str()).or_insert(0) += 1;
        if kind == CrossCheckKind::WardenAudit {
            if self.audit_window.len() == AGREEMENT_WINDOW {
                self.audit_window.pop_front();
            }
            self.audit_window
                .push_back(outcome == CrossCheckOutcome::Agree);
            // Health metrics on every audit append.
            match self.health() {
                AgreementHealth::SuspiciousAgreement => metrics.inc("curator.suspicious_agreement"),
                AgreementHealth::Miscalibration => metrics.inc("curator.miscalibration"),
                _ => {}
            }
        }
        metrics.inc("cross_check.records");

        // Batch signature every 256 records at T2+ (§7).
        if self.next_seq % BATCH_SIGN_EVERY == BATCH_SIGN_EVERY - 1 {
            self.sign_batch()?;
        }

        self.records.push(rec);
        self.next_seq += 1;
        Ok(self.next_seq - 1)
    }

    /// Re-apply a record that was already durably written to the dedicated
    /// WAL. Recovery must never append it again, but it must rebuild the same
    /// indices, rolling window, batch hashes, and sequence number.
    pub fn replay_record(&mut self, metrics: &Metrics, rec: CrossCheckRecord) -> UcResult<()> {
        if rec.seq < self.next_seq {
            return Ok(());
        }
        if rec.seq != self.next_seq {
            return Err(UcError::internal(format!(
                "cross-check WAL sequence gap: expected {}, got {}",
                self.next_seq, rec.seq
            )));
        }
        let bytes = rec.to_cbor().encode();
        self.batch_hashes.push(sha256(&bytes));
        self.by_initiator
            .entry(rec.initiator.clone())
            .or_default()
            .push(rec.seq);
        *self.by_outcome.entry(rec.outcome.as_str()).or_insert(0) += 1;
        if rec.kind == CrossCheckKind::WardenAudit {
            if self.audit_window.len() == AGREEMENT_WINDOW {
                self.audit_window.pop_front();
            }
            self.audit_window
                .push_back(rec.outcome == CrossCheckOutcome::Agree);
        }
        metrics.inc("cross_check.records_replayed");
        if self.next_seq % BATCH_SIGN_EVERY == BATCH_SIGN_EVERY - 1
            && !self
                .batch_signatures
                .iter()
                .any(|sig| sig.last_seq == self.next_seq)
        {
            // A replay never writes a new WAL frame, but a completed batch
            // still needs its signature state reconstructed if it was absent.
            self.sign_batch()?;
        } else if self.next_seq % BATCH_SIGN_EVERY == BATCH_SIGN_EVERY - 1 {
            self.batch_hashes.clear();
        }
        self.records.push(rec);
        self.next_seq += 1;
        Ok(())
    }

    fn signatures_path(&self) -> Option<std::path::PathBuf> {
        self.wal
            .as_ref()
            .map(|wal| wal.dir().join("batch-signatures.cbor"))
    }

    fn persist_signature_state(&self) -> UcResult<()> {
        let Some(path) = self.signatures_path() else {
            return Ok(());
        };
        let bytes = Cbor::Array(
            self.batch_signatures
                .iter()
                .map(BatchSignatureRecord::to_cbor)
                .collect(),
        )
        .encode();
        atomic_write_bytes(&path, &bytes)?;
        Ok(())
    }

    pub fn reload_signature_state_from_sidecar(&mut self) -> UcResult<()> {
        let Some(path) = self.signatures_path() else {
            return Ok(());
        };
        if !path.exists() {
            return Ok(());
        }
        let raw = std::fs::read(&path)?;
        let parsed = Cbor::decode(&raw)?;
        let mut sigs = Vec::new();
        if let Some(items) = parsed.as_array() {
            for item in items {
                sigs.push(BatchSignatureRecord::from_cbor(item)?);
            }
        }
        self.batch_signatures = sigs;
        Ok(())
    }

    pub fn verify_batch_signatures(&self) -> UcResult<(u64, bool)> {
        let expected_batches = (self.records.len() / BATCH_SIGN_EVERY as usize) as u64;
        if expected_batches == 0 {
            return Ok((0, true));
        }
        let Some(kms) = &self.kms else {
            return Ok((0, false));
        };
        if self.batch_signatures.len() != expected_batches as usize {
            return Ok((0, false));
        }
        let mut verified = 0u64;
        for (batch_idx, sig) in self.batch_signatures.iter().enumerate() {
            let start = batch_idx * BATCH_SIGN_EVERY as usize;
            let end = start + BATCH_SIGN_EVERY as usize;
            let expected_last_seq = end as u64 - 1;
            if sig.last_seq != expected_last_seq {
                return Ok((verified, false));
            }
            let mut concat = Vec::with_capacity(BATCH_SIGN_EVERY as usize * 32);
            for rec in &self.records[start..end] {
                concat.extend_from_slice(&sha256(&rec.to_cbor().encode()));
            }
            let digest = sha256(&concat);
            let mac = parse_hex_32(&sig.hmac_hex)?;
            if !kms.verify_batch_signature(
                &BatchSignature {
                    key_id: sig.key_id,
                    mac,
                },
                &digest,
            ) {
                return Ok((verified, false));
            }
            verified += 1;
        }
        Ok((verified, true))
    }

    fn sign_batch(&mut self) -> UcResult<()> {
        let Some(kms) = &self.kms else {
            self.batch_hashes.clear();
            return Ok(());
        };
        let mut concat = Vec::with_capacity(self.batch_hashes.len() * 32);
        for h in &self.batch_hashes {
            concat.extend_from_slice(h);
        }
        let digest = sha256(&concat);
        if let Some(sig) = kms.batch_sign(&digest) {
            self.batch_signatures.push(BatchSignatureRecord {
                last_seq: self.next_seq,
                key_id: sig.key_id,
                hmac_hex: hex(&sig.mac),
            });
            self.persist_signature_state()?;
        }
        self.batch_hashes.clear();
        Ok(())
    }

    /// Rolling-window Warden-audit agreement rate; `None` until 20 samples.
    pub fn agreement_rate(&self) -> Option<f64> {
        if self.audit_window.len() < 20 {
            return None;
        }
        let agree = self.audit_window.iter().filter(|b| **b).count();
        Some(agree as f64 / self.audit_window.len() as f64)
    }

    pub fn health(&self) -> AgreementHealth {
        match self.agreement_rate() {
            None => AgreementHealth::InsufficientData,
            Some(r) if r > 0.99 => AgreementHealth::SuspiciousAgreement,
            Some(r) if r < self.quota_low => AgreementHealth::Miscalibration,
            Some(_) => AgreementHealth::Healthy,
        }
    }

    /// Probe-rate boost multiplier when collusion is suspected (§6.3).
    pub fn probe_boost(&self) -> f64 {
        match self.health() {
            AgreementHealth::SuspiciousAgreement => PROBE_BOOST_ON_SUSPICIOUS,
            _ => 1.0,
        }
    }

    pub fn tail(&self, n: usize) -> Vec<&CrossCheckRecord> {
        let start = self.records.len().saturating_sub(n);
        self.records[start..].iter().collect()
    }

    pub fn retention_policy(&self) -> &'static str {
        RETENTION_POLICY
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn outcome_counts(&self) -> &BTreeMap<&'static str, u64> {
        &self.by_outcome
    }
}

impl CellBehavior for CrossCheckLedgerCell {
    fn cell_id(&self) -> CellId {
        self.id
    }
    fn cell_type(&self) -> CellType {
        CellType::CrossCheckLedger
    }
    fn schema_id(&self) -> SchemaId {
        SchemaId::new("curator.cross_check_ledger.v1")
    }

    fn on_query(&self, _at: u64, query: &Cbor) -> UcResult<Cbor> {
        match query.opt_str("op").as_deref() {
            Some("tail") | None => {
                let n = query.opt_u64("n").unwrap_or(20) as usize;
                let items: Vec<Cbor> = self.tail(n).iter().map(|r| r.to_cbor()).collect();
                Ok(Cbor::map(vec![
                    ("records", Cbor::Array(items)),
                    (
                        "agreement_rate",
                        self.agreement_rate().map(Cbor::F64).unwrap_or(Cbor::Null),
                    ),
                    (
                        "health",
                        Cbor::t(match self.health() {
                            AgreementHealth::Healthy => "healthy",
                            AgreementHealth::SuspiciousAgreement => "suspicious_agreement",
                            AgreementHealth::Miscalibration => "miscalibration",
                            AgreementHealth::InsufficientData => "insufficient_data",
                        }),
                    ),
                ]))
            }
            _ => Err(UcError::schema("cross_check: unknown op")),
        }
    }

    fn on_update(&mut self, _at: u64, _update: &Cbor) -> UcResult<Cbor> {
        Err(UcError::schema(
            "cross_check ledger is appended by the curator flow, not direct writes",
        ))
    }

    fn snapshot_state(&self) -> Cbor {
        let items: Vec<Cbor> = self.records.iter().map(|r| r.to_cbor()).collect();
        let sigs: Vec<Cbor> = self
            .batch_signatures
            .iter()
            .map(BatchSignatureRecord::to_cbor)
            .collect();
        Cbor::map(vec![
            ("records", Cbor::Array(items)),
            ("batch_signatures", Cbor::Array(sigs)),
        ])
    }

    fn restore_state(&mut self, state: &Cbor) -> UcResult<()> {
        self.records.clear();
        self.by_initiator.clear();
        self.by_outcome.clear();
        self.audit_window.clear();
        self.next_seq = 0;
        self.batch_signatures.clear();
        if let Some(arr) = state.get("records").and_then(|v| v.as_array()) {
            for item in arr {
                let rec = CrossCheckRecord::from_cbor(item)?;
                self.by_initiator
                    .entry(rec.initiator.clone())
                    .or_default()
                    .push(rec.seq);
                *self.by_outcome.entry(rec.outcome.as_str()).or_insert(0) += 1;
                if rec.kind == CrossCheckKind::WardenAudit {
                    if self.audit_window.len() == AGREEMENT_WINDOW {
                        self.audit_window.pop_front();
                    }
                    self.audit_window
                        .push_back(rec.outcome == CrossCheckOutcome::Agree);
                }
                self.next_seq = self.next_seq.max(rec.seq + 1);
                self.records.push(rec);
            }
        }
        if let Some(arr) = state.get("batch_signatures").and_then(|v| v.as_array()) {
            for item in arr {
                self.batch_signatures
                    .push(BatchSignatureRecord::from_cbor(item)?);
            }
        }
        // Keep the partial batch hash accumulator aligned with the restored
        // sequence. Completed batches are represented by signatures; only the
        // current incomplete batch should accept new/replayed records.
        let batch_start = (self.next_seq / BATCH_SIGN_EVERY) * BATCH_SIGN_EVERY;
        self.batch_hashes = self
            .records
            .iter()
            .filter(|rec| rec.seq >= batch_start)
            .map(|rec| sha256(&rec.to_cbor().encode()))
            .collect();
        Ok(())
    }
}

fn parse_hex_32(s: &str) -> UcResult<[u8; 32]> {
    if s.len() != 64 {
        return Err(UcError::schema(
            "cross_check signature hex must be 64 chars",
        ));
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        let hi = u8::from_str_radix(&s[i * 2..i * 2 + 1], 16)
            .map_err(|_| UcError::schema("cross_check signature hex invalid"))?;
        let lo = u8::from_str_radix(&s[i * 2 + 1..i * 2 + 2], 16)
            .map_err(|_| UcError::schema("cross_check signature hex invalid"))?;
        out[i] = (hi << 4) | lo;
    }
    Ok(out)
}

fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> UcResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| UcError::internal("cross_check signatures path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::wal::WalWriter;
    use crate::persist::EncryptionTier;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("uc-cross-check-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn append_audit(ledger: &mut CrossCheckLedgerCell, m: &Metrics, at: u64, agree: bool) {
        ledger
            .append(
                m,
                at,
                CrossCheckKind::WardenAudit,
                &format!("librarian/output/{at:04}"),
                &format!("warden/judgment/{at:04}"),
                if agree {
                    CrossCheckOutcome::Agree
                } else {
                    CrossCheckOutcome::Disagree
                },
                None,
            )
            .unwrap();
    }

    #[test]
    fn suspicious_agreement_at_100_of_100() {
        let mut ledger = CrossCheckLedgerCell::new(CellId(33));
        let m = Metrics::new();
        for i in 0..100 {
            append_audit(&mut ledger, &m, i, true);
        }
        assert_eq!(ledger.health(), AgreementHealth::SuspiciousAgreement);
        assert!(m.counter("curator.suspicious_agreement") > 0);
        assert_eq!(ledger.probe_boost(), 10.0);
    }

    #[test]
    fn healthy_band_and_miscalibration() {
        let mut ledger = CrossCheckLedgerCell::new(CellId(33));
        let m = Metrics::new();
        // 95% agreement: healthy.
        for i in 0..100 {
            append_audit(&mut ledger, &m, i, i % 20 != 0);
        }
        assert_eq!(ledger.health(), AgreementHealth::Healthy);
        // Push agreement below 92%: miscalibration.
        for i in 100..200 {
            append_audit(&mut ledger, &m, i, i % 3 != 0);
        }
        assert_eq!(ledger.health(), AgreementHealth::Miscalibration);
        assert!(m.counter("curator.miscalibration") > 0);
    }

    #[test]
    fn window_is_rolling() {
        let mut ledger = CrossCheckLedgerCell::new(CellId(33));
        let m = Metrics::new();
        // 200 disagreements, then 200 agreements: the window forgets.
        for i in 0..200 {
            append_audit(&mut ledger, &m, i, false);
        }
        assert_eq!(ledger.health(), AgreementHealth::Miscalibration);
        for i in 200..400 {
            append_audit(&mut ledger, &m, i, true);
        }
        assert_eq!(ledger.health(), AgreementHealth::SuspiciousAgreement);
        assert_eq!(ledger.agreement_rate(), Some(1.0));
    }

    #[test]
    fn snapshot_restores_window_and_indices() {
        let mut ledger = CrossCheckLedgerCell::new(CellId(33));
        let m = Metrics::new();
        for i in 0..50 {
            append_audit(&mut ledger, &m, i, i % 10 != 0);
        }
        assert_eq!(ledger.retention_policy(), RETENTION_POLICY);
        let snap = ledger.snapshot_state();
        let mut ledger2 = CrossCheckLedgerCell::new(CellId(33));
        ledger2.restore_state(&snap).unwrap();
        assert_eq!(ledger2.len(), 50);
        assert_eq!(ledger2.retention_policy(), RETENTION_POLICY);
        assert_eq!(ledger.agreement_rate(), ledger2.agreement_rate());
        assert_eq!(snap.encode(), ledger2.snapshot_state().encode());
    }

    #[test]
    fn batch_signatures_persist_and_verify_across_restore() {
        let dir = tmpdir("signatures");
        let wal = WalWriter::open(&dir.join("wal")).unwrap();
        let kms = Arc::new(Kms::open(&dir, EncryptionTier::T3).unwrap());
        let mut ledger = CrossCheckLedgerCell::new(CellId(33));
        ledger.attach_persistence(wal.clone(), kms.clone());
        let m = Metrics::new();
        for i in 0..BATCH_SIGN_EVERY {
            append_audit(&mut ledger, &m, i, true);
        }
        let (verified, ok) = ledger.verify_batch_signatures().unwrap();
        assert_eq!(verified, 1);
        assert!(ok);
        assert_eq!(ledger.batch_signatures.len(), 1);

        let snap = ledger.snapshot_state();
        let mut restored = CrossCheckLedgerCell::new(CellId(33));
        restored.attach_persistence(wal.clone(), kms);
        restored.restore_state(&snap).unwrap();
        restored.reload_signature_state_from_sidecar().unwrap();
        let (verified_restored, ok_restored) = restored.verify_batch_signatures().unwrap();
        assert_eq!(verified_restored, 1);
        assert!(ok_restored);
        assert_eq!(restored.batch_signatures[0].last_seq, BATCH_SIGN_EVERY - 1);

        wal.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
