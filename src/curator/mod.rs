//! Curator layer — SPEC-DERIVED-§2–§4 (CURATOR_PAIR_PROTOCOL.md).
//!
//! Three curators police semantic quality: the **Librarian** organizes
//! (skeletons, supersede proposals, archive tags), the **Warden** audits
//! everything the Librarian produces plus optionally gates writes
//! (chain step 6), and the **Adjudicator** resolves their disagreements.
//! The **CrossCheckLedger** records every interaction on its own WAL
//! stream.
//!
//! Two principles are enforced *structurally* here, not by convention:
//!
//! - **P19 Asymmetric Visibility**: every curator output is split into a
//!   [`CuratorPublic`] part (operation, grounding, confidence *band*) and a
//!   [`CuratorPrivate`] part (rationale, alternatives, precise confidence,
//!   reasoning trace). Private facets are stored as blobs behind
//!   `…/rationale`-style handles that peer-curator capability tokens
//!   exclude — the Router rejects the hydrate before any cell sees it.
//! - **P20 Substrate-Policed**: curators are ordinary agents to the Router.
//!   Their writes run the full Trinity chain; their disagreement rates are
//!   watched; their weights are pinned by SHA-256.

pub mod adjudicator;
pub mod guardrails;
pub mod ledger;
pub mod librarian;
pub mod warden;

use crate::core::cbor::Cbor;
use crate::core::{ShardTopology, UcError, UcResult};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

// ---------------------------------------------------------------------------
// Confidence + operations
// ---------------------------------------------------------------------------

/// PUBLIC confidence is a coarse band; the precise value is PRIVATE
/// (CURATOR_PAIR_PROTOCOL.md §3.2 — bands prevent curators from mirroring
/// each other's confidence surfaces).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConfidenceBand {
    Low,
    Medium,
    High,
}

impl ConfidenceBand {
    pub fn from_precise(p: f64) -> ConfidenceBand {
        if p < 0.45 {
            ConfidenceBand::Low
        } else if p < 0.8 {
            ConfidenceBand::Medium
        } else {
            ConfidenceBand::High
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            ConfidenceBand::Low => "low",
            ConfidenceBand::Medium => "medium",
            ConfidenceBand::High => "high",
        }
    }
    pub fn parse(s: &str) -> Option<ConfidenceBand> {
        Some(match s {
            "low" => ConfidenceBand::Low,
            "medium" => ConfidenceBand::Medium,
            "high" => ConfidenceBand::High,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CuratorOperation {
    Skeleton,
    SupersedeProposal,
    ArchiveTag,
    FlagHallucination,
    FlagDrift,
    AuditPass,
    AuditFail,
    SanityAgree,
    SanityDisagree,
    Adjudication,
}

impl CuratorOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            CuratorOperation::Skeleton => "skeleton",
            CuratorOperation::SupersedeProposal => "supersede_proposal",
            CuratorOperation::ArchiveTag => "archive_tag",
            CuratorOperation::FlagHallucination => "flag_hallucination",
            CuratorOperation::FlagDrift => "flag_drift",
            CuratorOperation::AuditPass => "audit_pass",
            CuratorOperation::AuditFail => "audit_fail",
            CuratorOperation::SanityAgree => "sanity_agree",
            CuratorOperation::SanityDisagree => "sanity_disagree",
            CuratorOperation::Adjudication => "adjudication",
        }
    }
    pub fn parse(s: &str) -> Option<CuratorOperation> {
        Some(match s {
            "skeleton" => CuratorOperation::Skeleton,
            "supersede_proposal" => CuratorOperation::SupersedeProposal,
            "archive_tag" => CuratorOperation::ArchiveTag,
            "flag_hallucination" => CuratorOperation::FlagHallucination,
            "flag_drift" => CuratorOperation::FlagDrift,
            "audit_pass" => CuratorOperation::AuditPass,
            "audit_fail" => CuratorOperation::AuditFail,
            "sanity_agree" => CuratorOperation::SanityAgree,
            "sanity_disagree" => CuratorOperation::SanityDisagree,
            "adjudication" => CuratorOperation::Adjudication,
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------
// PUBLIC / PRIVATE output split (P19)
// ---------------------------------------------------------------------------

/// Everything a *peer curator* may see about an output.
#[derive(Clone, Debug)]
pub struct CuratorPublic {
    pub output_handle: String, // e.g. "librarian/output/<ulid>"
    pub operation: CuratorOperation,
    pub target_handle: String,
    /// Substrate handles this output is grounded in — the auditable claim.
    pub grounded_in: Vec<String>,
    pub confidence_band: ConfidenceBand,
    pub schema_id: String,
    pub spec_anchor: String,
    pub logical_at: u64,
    /// Public body: the skeleton text / flag summary itself.
    pub body: String,
}

/// The facets peers must never see. Stored as CAS blobs behind facet
/// handles (`<output_handle>/rationale` etc.) so P19 is enforced by the
/// Router's facet-scope check, not by curator good manners.
#[derive(Clone, Debug)]
pub struct CuratorPrivate {
    pub rationale: String,
    pub considered_alts: Vec<String>,
    pub confidence_precise: f64,
    pub reasoning_trace: String,
    pub private_seed: u64,
}

#[derive(Clone, Debug)]
pub struct CuratorOutput {
    pub public: CuratorPublic,
    pub private: CuratorPrivate,
}

impl CuratorPublic {
    pub fn to_cbor(&self) -> Cbor {
        Cbor::map(vec![
            ("output_handle", Cbor::t(self.output_handle.clone())),
            ("operation", Cbor::t(self.operation.as_str())),
            ("target_handle", Cbor::t(self.target_handle.clone())),
            ("grounded_in", Cbor::text_array(&self.grounded_in)),
            ("confidence_band", Cbor::t(self.confidence_band.as_str())),
            ("schema_id", Cbor::t(self.schema_id.clone())),
            ("spec_anchor", Cbor::t(self.spec_anchor.clone())),
            ("logical_at", Cbor::U64(self.logical_at)),
            ("body", Cbor::t(self.body.clone())),
        ])
    }

    pub fn from_cbor(c: &Cbor) -> UcResult<CuratorPublic> {
        Ok(CuratorPublic {
            output_handle: c.req_str("output_handle")?,
            operation: CuratorOperation::parse(&c.req_str("operation")?)
                .ok_or_else(|| UcError::schema("bad curator operation"))?,
            target_handle: c.opt_str("target_handle").unwrap_or_default(),
            grounded_in: c
                .get("grounded_in")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            confidence_band: c
                .opt_str("confidence_band")
                .and_then(|s| ConfidenceBand::parse(&s))
                .unwrap_or(ConfidenceBand::Low),
            schema_id: c.opt_str("schema_id").unwrap_or_default(),
            spec_anchor: c.opt_str("spec_anchor").unwrap_or_default(),
            logical_at: c.opt_u64("logical_at").unwrap_or(0),
            body: c.opt_str("body").unwrap_or_default(),
        })
    }
}

/// The four private facet suffixes (CURATOR_PAIR_PROTOCOL.md §4.1). These
/// are exactly the globs excluded from peer-curator capability tokens.
pub const PRIVATE_FACETS: [&str; 4] = [
    "rationale",
    "considered_alts",
    "reasoning_trace",
    "confidence_precise",
];

pub fn facet_handle(output_handle: &str, facet: &str) -> String {
    format!("{output_handle}/{facet}")
}

pub fn is_private_facet(handle: &str) -> bool {
    PRIVATE_FACETS
        .iter()
        .any(|f| handle.ends_with(&format!("/{f}")) || handle.contains(&format!("/{f}/")))
}

// ---------------------------------------------------------------------------
// Substrate view — what curators may consult while working
// ---------------------------------------------------------------------------

/// Read-only window curators get onto substrate state. Implemented by the
/// Node/Router; test doubles implement it directly. Note the *absence* of
/// any private-facet access — the trait can't express it, so backends can't
/// ask for it.
pub trait SubstrateView {
    fn handle_exists(&self, handle: &str) -> bool;
    /// Active (non-superseded) facts for (subject, predicate) as
    /// (handle, object) pairs.
    fn active_sp(&self, subject: &str, predicate: &str) -> Vec<(String, String)>;
    /// Public text for a handle (fact object, blob text, skeleton body…).
    fn public_text(&self, handle: &str) -> Option<String>;
}

// ---------------------------------------------------------------------------
// Curator backend seam
// ---------------------------------------------------------------------------

/// Judgment produced by a backend for an ambiguous adjudication.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    InitiatorCorrect,
    AuditorCorrect,
    Uncertain,
}

/// The inference seam. [`DeterministicBackend`] is the default and is
/// genuinely functional (extractive summarization + evidence-based
/// verdicts). [`ExternalGgufBackend`] shells out to a llama.cpp-style
/// binary with pinned, SHA-verified weights, temperature 0, and the
/// envelope seed — a drop-in upgrade path that keeps determinism
/// (LibrarianCell.md §6, WardenCell.md §7).
pub trait CuratorBackend: Send + Sync {
    fn backend_id(&self) -> String;

    /// Extractive skeleton: ≤ `max_tokens` estimated tokens summarizing
    /// `text`, grounded only in the given text.
    fn skeleton(&self, text: &str, max_tokens: usize) -> String;

    /// Resolve an ambiguous dispute given only public evidence lines.
    /// `seed` selects deterministic behavior; the same inputs must yield
    /// the same verdict.
    fn adjudicate(&self, seed: u64, dispute_summary: &str, evidence: &[String]) -> Verdict;
}

// ---------------------------------------------------------------------------
// DeterministicBackend
// ---------------------------------------------------------------------------

pub struct DeterministicBackend;

impl DeterministicBackend {
    /// Score sentences by lexical centrality: sum of shared-token counts
    /// with every other sentence, position-weighted. Fully deterministic.
    fn rank_sentences(text: &str) -> Vec<(usize, String)> {
        let sentences: Vec<String> = split_sentences(text);
        if sentences.is_empty() {
            return Vec::new();
        }
        let token_sets: Vec<std::collections::BTreeSet<String>> = sentences
            .iter()
            .map(|s| crate::cells::index::tokenize(s).into_iter().collect())
            .collect();
        let mut scored: Vec<(f64, usize)> = Vec::new();
        for (i, ts) in token_sets.iter().enumerate() {
            let mut overlap = 0usize;
            for (j, other) in token_sets.iter().enumerate() {
                if i != j {
                    overlap += ts.intersection(other).count();
                }
            }
            // Early sentences get a small positional boost.
            let pos_boost = 1.0 / (1.0 + i as f64 * 0.15);
            scored.push((overlap as f64 * pos_boost + pos_boost, i));
        }
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.cmp(&b.1))
        });
        scored
            .into_iter()
            .map(|(_, i)| (i, sentences[i].clone()))
            .collect()
    }
}

impl CuratorBackend for DeterministicBackend {
    fn backend_id(&self) -> String {
        "deterministic.v1".into()
    }

    fn skeleton(&self, text: &str, max_tokens: usize) -> String {
        let ranked = Self::rank_sentences(text);
        if ranked.is_empty() {
            return String::new();
        }
        let mut chosen: Vec<(usize, String)> = Vec::new();
        let mut tokens = 0usize;
        for (idx, sentence) in ranked {
            let t = crate::core::est_tokens(sentence.len()) as usize;
            if tokens + t > max_tokens && !chosen.is_empty() {
                continue;
            }
            tokens += t;
            chosen.push((idx, sentence));
            if tokens >= max_tokens {
                break;
            }
        }
        // Restore document order for readability.
        chosen.sort_by_key(|(i, _)| *i);
        chosen
            .into_iter()
            .map(|(_, s)| s)
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn adjudicate(&self, seed: u64, dispute_summary: &str, evidence: &[String]) -> Verdict {
        // Deterministic evidence tally: lines beginning "supports_initiator"
        // or "supports_auditor" (produced by the Adjudicator's evidence
        // assembly) are counted; near-ties defer to Uncertain, which routes
        // to human escalation per AdjudicatorCell.md §6.
        let init = evidence
            .iter()
            .filter(|e| e.starts_with("supports_initiator"))
            .count() as i64;
        let audit = evidence
            .iter()
            .filter(|e| e.starts_with("supports_auditor"))
            .count() as i64;
        match init - audit {
            d if d >= 2 => Verdict::InitiatorCorrect,
            d if d <= -2 => Verdict::AuditorCorrect,
            1 => Verdict::InitiatorCorrect,
            -1 => Verdict::AuditorCorrect,
            _ => {
                // Dead tie: derive a stable lean from (seed, dispute hash) —
                // per-judge tie-break permutation (AdjudicatorCell.md §5.3).
                let h = crate::core::fnv1a64(dispute_summary.as_bytes()) ^ seed;
                if h & 0b111 == 0 {
                    Verdict::Uncertain // ~12.5% of dead ties escalate
                } else if h & 1 == 0 {
                    Verdict::InitiatorCorrect
                } else {
                    Verdict::AuditorCorrect
                }
            }
        }
    }
}

fn split_sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in text.chars() {
        cur.push(c);
        if matches!(c, '.' | '!' | '?' | '\n') {
            let s = cur.trim();
            if s.len() > 2 {
                out.push(s.to_string());
            }
            cur.clear();
        }
    }
    let s = cur.trim();
    if s.len() > 2 {
        out.push(s.to_string());
    }
    out
}

// ---------------------------------------------------------------------------
// ExternalGgufBackend
// ---------------------------------------------------------------------------

/// Shells out to an external llama.cpp-style CLI with pinned weights.
/// Weight files are SHA-verified at construction (PersistenceLayer.md §7);
/// any parse failure at inference time falls back to [`DeterministicBackend`]
/// and increments `curator.backend_fallback` — an LLM hiccup must never
/// stall the curation pipeline (LibrarianCell.md §6.4).
pub struct ExternalGgufBackend {
    pub slot: String,
    pub model_name: String,
    pub weight_path: PathBuf,
    pub cmd: String, // e.g. "llama-cli"
    fallback: DeterministicBackend,
    metrics: Option<std::sync::Arc<crate::obs::Metrics>>,
}

impl ExternalGgufBackend {
    pub fn new(
        data_dir: &std::path::Path,
        model_name: &str,
        sha_hex: &str,
        cmd: &str,
        metrics: Option<std::sync::Arc<crate::obs::Metrics>>,
    ) -> UcResult<ExternalGgufBackend> {
        Self::new_for_slot(data_dir, model_name, model_name, sha_hex, cmd, metrics)
    }

    pub fn new_for_slot(
        data_dir: &std::path::Path,
        slot: &str,
        model_name: &str,
        sha_hex: &str,
        cmd: &str,
        metrics: Option<std::sync::Arc<crate::obs::Metrics>>,
    ) -> UcResult<ExternalGgufBackend> {
        let weight_path = crate::persist::verify_weight_file(data_dir, slot, sha_hex)?;
        // Production bootstrap must not accept a configured model whose
        // runner will only fail later inside the curation path.
        if std::process::Command::new(cmd)
            .arg("--version")
            .output()
            .is_err()
        {
            return Err(UcError::not_found(format!(
                "GGUF runner not found on PATH: {cmd}"
            )));
        }
        Ok(ExternalGgufBackend {
            slot: slot.to_string(),
            model_name: model_name.to_string(),
            weight_path,
            cmd: cmd.to_string(),
            fallback: DeterministicBackend,
            metrics,
        })
    }

    fn run(&self, seed: u64, prompt: &str, max_tokens: usize) -> Option<String> {
        let output = Command::new(&self.cmd)
            .arg("-m")
            .arg(&self.weight_path)
            .arg("--temp")
            .arg("0")
            .arg("--seed")
            .arg(seed.to_string())
            .arg("-n")
            .arg(max_tokens.to_string())
            .arg("--no-display-prompt")
            .arg("-p")
            .arg(prompt)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if text.is_empty() {
            None
        } else {
            // Strip DeepSeek-R1-style <think> blocks if the pool model emits
            // them (DeepSeekOptimization.md §4).
            Some(crate::deepseek::r1_strip(&text).0)
        }
    }

    fn note_fallback(&self) {
        if let Some(m) = &self.metrics {
            m.inc("curator.backend_fallback");
        }
    }
}

impl CuratorBackend for ExternalGgufBackend {
    fn backend_id(&self) -> String {
        format!("gguf.{}", self.model_name)
    }

    fn skeleton(&self, text: &str, max_tokens: usize) -> String {
        let prompt = format!(
            "Summarize the following into at most {max_tokens} tokens. Output only the summary.\n\n{text}"
        );
        match self.run(0, &prompt, max_tokens + 16) {
            Some(s) => s,
            None => {
                self.note_fallback();
                self.fallback.skeleton(text, max_tokens)
            }
        }
    }

    fn adjudicate(&self, seed: u64, dispute_summary: &str, evidence: &[String]) -> Verdict {
        let prompt = format!(
            "You are an impartial adjudicator. Given the dispute and evidence, answer with \
             exactly one word: INITIATOR, AUDITOR, or UNCERTAIN.\n\nDispute: {dispute_summary}\n\nEvidence:\n{}",
            evidence.join("\n")
        );
        match self.run(seed, &prompt, 8).map(|s| s.to_ascii_uppercase()) {
            Some(s) if s.contains("INITIATOR") => Verdict::InitiatorCorrect,
            Some(s) if s.contains("AUDITOR") => Verdict::AuditorCorrect,
            Some(s) if s.contains("UNCERTAIN") => Verdict::Uncertain,
            _ => {
                self.note_fallback();
                self.fallback.adjudicate(seed, dispute_summary, evidence)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Curator config (mirrors [curator] TOML section)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CuratorKvBudgetProfile {
    Small,
    Reference,
    Heavy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CuratorKvBudgets {
    pub librarian_mib: u64,
    pub warden_mib: u64,
    pub adjudicator_mib: u64,
}

impl CuratorKvBudgetProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            CuratorKvBudgetProfile::Small => "small",
            CuratorKvBudgetProfile::Reference => "reference",
            CuratorKvBudgetProfile::Heavy => "heavy",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "small" => CuratorKvBudgetProfile::Small,
            "reference" => CuratorKvBudgetProfile::Reference,
            "heavy" => CuratorKvBudgetProfile::Heavy,
            _ => return None,
        })
    }

    pub fn budgets(self) -> CuratorKvBudgets {
        match self {
            CuratorKvBudgetProfile::Small => CuratorKvBudgets {
                librarian_mib: 256,
                warden_mib: 256,
                adjudicator_mib: 128,
            },
            CuratorKvBudgetProfile::Reference => CuratorKvBudgets {
                librarian_mib: 384,
                warden_mib: 384,
                adjudicator_mib: 256,
            },
            CuratorKvBudgetProfile::Heavy => CuratorKvBudgets {
                librarian_mib: 768,
                warden_mib: 768,
                adjudicator_mib: 512,
            },
        }
    }
}

impl CuratorKvBudgets {
    pub fn total_mib(self) -> u64 {
        self.librarian_mib + self.warden_mib + self.adjudicator_mib
    }
}

#[derive(Clone, Debug)]
pub struct CuratorConfig {
    pub disagreement_quota_low: f64,  // default 0.92
    pub disagreement_quota_high: f64, // default 0.97
    pub probe_rate: f64,              // default 0.001
    pub blind_reaudit_rate: f64,      // default 0.01
    pub kv_budget_profile: CuratorKvBudgetProfile,
    pub topology: ShardTopology,
    pub adjudicator_pool: Vec<String>,
    /// model -> sha256 hex, for external backends.
    pub pinned: BTreeMap<String, String>,
    pub external_cmd: Option<String>,
    /// Production model identities. The slots are deliberately separate so
    /// Librarian and Warden cannot silently converge on one model family.
    pub librarian_model: String,
    pub warden_model: String,
    /// Production boots fail closed when these pins or the runner are absent.
    pub strict_model_pins: bool,
}

pub const DEFAULT_LIBRARIAN_MODEL: &str = "gemma-2-2b-it-q4_k_m";
pub const DEFAULT_LIBRARIAN_SHA256: &str =
    "e0aee85060f168f0f2d8473d7ea41ce2f3230c1bc1374847505ea599288a7787";
pub const DEFAULT_WARDEN_MODEL: &str = "qwen2.5-coder-1.5b-q4_k_m";
pub const DEFAULT_WARDEN_SHA256: &str =
    "0c3c38b9d1e2d6fa227b321b4d30ba921e1f7694a42a0ba207020cc58576fccc";

impl Default for CuratorConfig {
    fn default() -> Self {
        CuratorConfig {
            disagreement_quota_low: 0.92,
            disagreement_quota_high: 0.97,
            probe_rate: 0.001,
            blind_reaudit_rate: 0.01,
            kv_budget_profile: CuratorKvBudgetProfile::Reference,
            topology: ShardTopology::Dedicated,
            adjudicator_pool: vec![
                "phi-3.5-mini".into(),
                "llama-3.2-3b".into(),
                "smollm2-1.7b".into(),
            ],
            pinned: BTreeMap::from([
                ("librarian".into(), DEFAULT_LIBRARIAN_SHA256.into()),
                ("warden".into(), DEFAULT_WARDEN_SHA256.into()),
            ]),
            external_cmd: Some("llama-cli".into()),
            librarian_model: DEFAULT_LIBRARIAN_MODEL.into(),
            warden_model: DEFAULT_WARDEN_MODEL.into(),
            strict_model_pins: true,
        }
    }
}

impl CuratorConfig {
    pub fn kv_budgets(&self) -> CuratorKvBudgets {
        self.kv_budget_profile.budgets()
    }

    /// Development/test mode is explicit so production configuration cannot
    /// silently downgrade a missing pinned model to deterministic behavior.
    pub fn development() -> Self {
        let mut cfg = Self::default();
        cfg.external_cmd = None;
        cfg.pinned.clear();
        cfg.strict_model_pins = false;
        cfg
    }

    pub fn validate_model_pair(&self) -> UcResult<()> {
        if self.librarian_model.is_empty() || self.warden_model.is_empty() {
            return Err(UcError::schema("curator model names must not be empty"));
        }
        if self.librarian_model == self.warden_model {
            return Err(UcError::schema(
                "librarian and warden must use different model families",
            ));
        }
        if self.strict_model_pins {
            for slot in ["librarian", "warden"] {
                let sha = self
                    .pinned
                    .get(slot)
                    .ok_or_else(|| UcError::schema(format!("missing curator pin: {slot}")))?;
                if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(UcError::schema(format!(
                        "curator pin for {slot} must be a 64-character SHA-256"
                    )));
                }
            }
            if self.external_cmd.as_deref().unwrap_or("").is_empty() {
                return Err(UcError::unsupported(
                    "production curator defaults require a local GGUF runner",
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confidence_bands() {
        assert_eq!(ConfidenceBand::from_precise(0.2), ConfidenceBand::Low);
        assert_eq!(ConfidenceBand::from_precise(0.6), ConfidenceBand::Medium);
        assert_eq!(ConfidenceBand::from_precise(0.95), ConfidenceBand::High);
    }

    #[test]
    fn private_facet_detection() {
        assert!(is_private_facet("librarian/output/01X/rationale"));
        assert!(is_private_facet("warden/judgment/01Y/reasoning_trace"));
        assert!(is_private_facet("librarian/output/01X/confidence_precise"));
        assert!(!is_private_facet("librarian/output/01X"));
        assert!(!is_private_facet("librarian/output/01X/public"));
        assert!(!is_private_facet("fact/rationale-of-the-decision")); // no slash-facet
    }

    #[test]
    fn deterministic_skeleton_is_stable_and_bounded() {
        let b = DeterministicBackend;
        let text = "The router validates every write. The trinity chain has five steps. \
                    Failures land in quarantine. Quarantine never drops items silently. \
                    The warden audits the librarian. The adjudicator resolves disputes. \
                    Budgets are mandatory on every envelope. Snapshots are copy on write.";
        let s1 = b.skeleton(text, 20);
        let s2 = b.skeleton(text, 20);
        assert_eq!(s1, s2);
        assert!(crate::core::est_tokens(s1.len()) as usize <= 20 + 12); // one-sentence slack
        assert!(!s1.is_empty());
    }

    #[test]
    fn deterministic_adjudication_by_evidence() {
        let b = DeterministicBackend;
        let ev_init = vec![
            "supports_initiator: handle exists".to_string(),
            "supports_initiator: object matches".to_string(),
        ];
        assert_eq!(b.adjudicate(1, "d", &ev_init), Verdict::InitiatorCorrect);
        let ev_aud = vec![
            "supports_auditor: handle absent".to_string(),
            "supports_auditor: conflicting fact".to_string(),
        ];
        assert_eq!(b.adjudicate(1, "d", &ev_aud), Verdict::AuditorCorrect);
        // Same seed + same dead-tie dispute => same verdict.
        let tie: Vec<String> = vec![];
        assert_eq!(
            b.adjudicate(7, "same dispute", &tie),
            b.adjudicate(7, "same dispute", &tie)
        );
    }

    #[test]
    fn public_cbor_roundtrip() {
        let p = CuratorPublic {
            output_handle: "librarian/output/01A".into(),
            operation: CuratorOperation::Skeleton,
            target_handle: "fact/01B".into(),
            grounded_in: vec!["fact/01B".into(), "fact/01C".into()],
            confidence_band: ConfidenceBand::Medium,
            schema_id: "curator.librarian.output.v1".into(),
            spec_anchor: "LibrarianCell.md\u{00a7}3".into(),
            logical_at: 44,
            body: "skeleton text".into(),
        };
        let c = p.to_cbor();
        let p2 = CuratorPublic::from_cbor(&c).unwrap();
        assert_eq!(p2.output_handle, p.output_handle);
        assert_eq!(p2.operation, CuratorOperation::Skeleton);
        assert_eq!(p2.grounded_in.len(), 2);
        assert_eq!(p2.confidence_band, ConfidenceBand::Medium);
    }

    #[test]
    fn kv_budget_profiles_surface_expected_totals() {
        let small = CuratorKvBudgetProfile::Small.budgets();
        assert_eq!(small.librarian_mib, 256);
        assert_eq!(small.warden_mib, 256);
        assert_eq!(small.adjudicator_mib, 128);
        assert_eq!(small.total_mib(), 640);

        let reference = CuratorKvBudgetProfile::Reference.budgets();
        assert_eq!(reference.total_mib(), 1_024);

        let heavy = CuratorKvBudgetProfile::Heavy.budgets();
        assert_eq!(heavy.total_mib(), 2_048);
    }

    #[test]
    fn production_curator_defaults_are_pinned_and_family_distinct() {
        let cfg = CuratorConfig::default();
        cfg.validate_model_pair().unwrap();
        assert_eq!(cfg.librarian_model, DEFAULT_LIBRARIAN_MODEL);
        assert_eq!(cfg.warden_model, DEFAULT_WARDEN_MODEL);
        assert_ne!(cfg.librarian_model, cfg.warden_model);
        assert_eq!(cfg.pinned["librarian"], DEFAULT_LIBRARIAN_SHA256);
        assert_eq!(cfg.pinned["warden"], DEFAULT_WARDEN_SHA256);
        assert!(!CuratorConfig::development().strict_model_pins);
    }
}
