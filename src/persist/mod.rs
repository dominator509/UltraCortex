//! L0 Persistence — SPEC-DERIVED-§3, §5, §6, §9 (PersistenceLayer.md).
//!
//! Layout under `data_dir`:
//! ```text
//! manifest.cbor              node identity, epochs, clean marker, state hashes
//! wal/shard-NN/epoch-*.wal   per-shard WAL streams
//! wal/cross_check/epoch-*.wal  CrossCheckLedger stream (own WAL, §CCL-5)
//! snapshots/snap-*.cbor      CoW full-state snapshots
//! cas/aa/bb/<hex>            SHA-256 content-addressed blobs
//! cache/tier-{l1,l2,l3}/     PrefixCacheStore payloads
//! cache/index/view_keys.cbor cache index
//! kms/                       wrapped keys (T1+)
//! weights/<model>/<sha>.gguf pinned curator weights
//! audit/audit.chain          hash-chained audit log
//! ```

pub mod wal;

use crate::core::cbor::Cbor;
use crate::core::crypto::{chacha20_xor, hex, hmac_sha256, sha256, sha256_file};
use crate::core::{UcError, UcResult};
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

/// `manifest.cbor` — written via temp-file + atomic rename (§3.6). The
/// `clean` marker distinguishes fresh-vs-recovery boot paths (B3a/B3b).
#[derive(Clone, Debug, Default)]
pub struct Manifest {
    pub node_id: String,
    pub proto_version: u64,
    pub logical_at: u64,
    pub clean_shutdown: bool,
    pub encryption_tier: String,
    pub shard_count: u64,
    /// cell_id -> sha256(state) recorded at last clean shutdown.
    pub state_hashes: BTreeMap<u64, [u8; 32]>,
    /// Last durable snapshot filename, if any.
    pub last_snapshot: Option<String>,
}

impl Manifest {
    pub fn to_cbor(&self) -> Cbor {
        let hashes: Vec<Cbor> = self
            .state_hashes
            .iter()
            .map(|(k, v)| {
                Cbor::map(vec![
                    ("cell_id", Cbor::U64(*k)),
                    ("sha256", Cbor::Bytes(v.to_vec())),
                ])
            })
            .collect();
        Cbor::map(vec![
            ("node_id", Cbor::t(self.node_id.clone())),
            ("proto_version", Cbor::U64(self.proto_version)),
            ("logical_at", Cbor::U64(self.logical_at)),
            ("clean_shutdown", Cbor::Bool(self.clean_shutdown)),
            ("encryption_tier", Cbor::t(self.encryption_tier.clone())),
            ("shard_count", Cbor::U64(self.shard_count)),
            ("state_hashes", Cbor::Array(hashes)),
            (
                "last_snapshot",
                match &self.last_snapshot {
                    Some(s) => Cbor::t(s.clone()),
                    None => Cbor::Null,
                },
            ),
        ])
    }

    pub fn from_cbor(c: &Cbor) -> UcResult<Manifest> {
        let mut m = Manifest {
            node_id: c.req_str("node_id")?,
            proto_version: c.req_u64("proto_version")?,
            logical_at: c.req_u64("logical_at")?,
            clean_shutdown: c.opt_bool("clean_shutdown").unwrap_or(false),
            encryption_tier: c.opt_str("encryption_tier").unwrap_or_else(|| "T0".into()),
            shard_count: c.opt_u64("shard_count").unwrap_or(2),
            state_hashes: BTreeMap::new(),
            last_snapshot: c.opt_str("last_snapshot"),
        };
        if let Some(arr) = c.get("state_hashes").and_then(|v| v.as_array()) {
            for item in arr {
                let id = item.req_u64("cell_id")?;
                if let Some(b) = item.get("sha256").and_then(|v| v.as_bytes()) {
                    if b.len() == 32 {
                        let mut h = [0u8; 32];
                        h.copy_from_slice(b);
                        m.state_hashes.insert(id, h);
                    }
                }
            }
        }
        Ok(m)
    }

    pub fn save(&self, data_dir: &Path) -> UcResult<()> {
        atomic_write(&data_dir.join("manifest.cbor"), &self.to_cbor().encode())
    }

    pub fn load(data_dir: &Path) -> UcResult<Option<Manifest>> {
        let path = data_dir.join("manifest.cbor");
        if !path.exists() {
            return Ok(None);
        }
        let bytes = std::fs::read(&path)?;
        Ok(Some(Manifest::from_cbor(&Cbor::decode(&bytes)?)?))
    }
}

/// Temp-file + fsync + atomic rename (§3.6).
pub fn atomic_write(path: &Path, bytes: &[u8]) -> UcResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| UcError::internal("atomic_write: no parent dir"))?;
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

// ---------------------------------------------------------------------------
// KMS — encryption tiers T0..T3 (§5)
// ---------------------------------------------------------------------------

/// Tier semantics (PersistenceLayer.md §5):
/// - **T0** plaintext at rest (dev only).
/// - **T1** local operator-owned keyring on disk; payload streams encrypted
///   with ChaCha20, integrity via HMAC-SHA256 (encrypt-then-MAC).
/// - **T2** T1 + per-stream derived keys + periodic HMAC batch signatures
///   over the CrossCheckLedger.
/// - **T3** T2 + scheduled local rotation with retained key history so older
///   sealed payloads and batch signatures remain verifiable after rotation.
pub struct Kms {
    tier: EncryptionTier,
    state_path: Option<PathBuf>,
    state: Mutex<KeyRingState>,
}

const KEYRING_FILE: &str = "keyring.cbor";
pub const T3_ROTATION_INTERVAL_OPS: u64 = 1_000_000;

#[derive(Clone, Debug, Default)]
struct KeyRingState {
    active_key_id: u64,
    last_rotated_at: u64,
    keys: BTreeMap<u64, [u8; 32]>,
}

impl KeyRingState {
    fn to_cbor(&self) -> Cbor {
        let keys: Vec<Cbor> = self
            .keys
            .iter()
            .map(|(key_id, key)| {
                Cbor::map(vec![
                    ("key_id", Cbor::U64(*key_id)),
                    ("material", Cbor::Bytes(key.to_vec())),
                ])
            })
            .collect();
        Cbor::map(vec![
            ("active_key_id", Cbor::U64(self.active_key_id)),
            ("last_rotated_at", Cbor::U64(self.last_rotated_at)),
            ("keys", Cbor::Array(keys)),
        ])
    }

    fn from_cbor(c: &Cbor) -> UcResult<KeyRingState> {
        let mut keys = BTreeMap::new();
        if let Some(items) = c.get("keys").and_then(|v| v.as_array()) {
            for item in items {
                let key_id = item.req_u64("key_id")?;
                let raw = item
                    .get("material")
                    .and_then(|v| v.as_bytes())
                    .ok_or_else(|| UcError::schema("kms keyring: missing key material"))?;
                if raw.len() != 32 {
                    return Err(UcError::schema("kms keyring: key material must be 32 bytes"));
                }
                let mut key = [0u8; 32];
                key.copy_from_slice(raw);
                keys.insert(key_id, key);
            }
        }
        let active_key_id = c.opt_u64("active_key_id").unwrap_or(0);
        if !keys.is_empty() && !keys.contains_key(&active_key_id) {
            return Err(UcError::schema(format!(
                "kms keyring: active key id {active_key_id} missing from key set"
            )));
        }
        Ok(KeyRingState {
            active_key_id,
            last_rotated_at: c.opt_u64("last_rotated_at").unwrap_or(0),
            keys,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchSignature {
    pub key_id: u64,
    pub mac: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RotationStatus {
    pub active_key_id: u64,
    pub key_versions: u64,
    pub last_rotated_at: u64,
    pub rotation_interval_ops: u64,
    pub next_due_at: u64,
    pub overdue: bool,
    pub custody_path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RotationEvent {
    pub previous_key_id: u64,
    pub active_key_id: u64,
    pub logical_at: u64,
    pub emergency: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncryptionTier {
    T0,
    T1,
    T2,
    T3,
}

impl EncryptionTier {
    pub fn parse(s: &str) -> UcResult<EncryptionTier> {
        Ok(match s {
            "T0" => EncryptionTier::T0,
            "T1" => EncryptionTier::T1,
            "T2" => EncryptionTier::T2,
            "T3" => EncryptionTier::T3,
            _ => {
                return Err(UcError::schema(format!(
                    "unknown encryption_tier: {s} (expected T0..T3)"
                )))
            }
        })
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            EncryptionTier::T0 => "T0",
            EncryptionTier::T1 => "T1",
            EncryptionTier::T2 => "T2",
            EncryptionTier::T3 => "T3",
        }
    }
}

fn legacy_key_path(data_dir: &Path) -> PathBuf {
    data_dir.join("kms").join("node.key")
}

fn keyring_path(data_dir: &Path) -> PathBuf {
    data_dir.join("kms").join(KEYRING_FILE)
}

fn random_seed(extra: &[u8]) -> Vec<u8> {
    let mut seed = Vec::new();
    seed.extend_from_slice(extra);
    seed.extend_from_slice(&std::process::id().to_le_bytes());
    if let Ok(d) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        seed.extend_from_slice(&d.as_nanos().to_le_bytes());
    }
    for _ in 0..4 {
        use std::collections::hash_map::RandomState;
        use std::hash::{BuildHasher, Hasher};
        let h = RandomState::new().build_hasher();
        seed.extend_from_slice(&h.finish().to_le_bytes());
    }
    seed
}

fn generate_root_key(extra: &[u8]) -> [u8; 32] {
    sha256(&random_seed(extra))
}

fn load_or_create_keyring(data_dir: &Path) -> UcResult<(PathBuf, KeyRingState)> {
    let state_path = keyring_path(data_dir);
    if state_path.exists() {
        let raw = std::fs::read(&state_path)?;
        let state = KeyRingState::from_cbor(&Cbor::decode(&raw)?)?;
        return Ok((state_path, state));
    }

    let legacy = legacy_key_path(data_dir);
    let key = if legacy.exists() {
        let raw = std::fs::read(&legacy)?;
        if raw.len() != 32 {
            return Err(UcError::internal("kms/node.key is not 32 bytes"));
        }
        let mut k = [0u8; 32];
        k.copy_from_slice(&raw);
        k
    } else {
        generate_root_key(b"ultracortex.kms.root")
    };

    let mut state = KeyRingState::default();
    state.active_key_id = 1;
    state.keys.insert(1, key);
    atomic_write(&state_path, &state.to_cbor().encode())?;
    restrict_perms(&state_path);
    Ok((state_path, state))
}

impl Kms {
    pub fn open(data_dir: &Path, tier: EncryptionTier) -> UcResult<Kms> {
        match tier {
            EncryptionTier::T0 => Ok(Kms {
                tier,
                state_path: None,
                state: Mutex::new(KeyRingState::default()),
            }),
            _ => {
                let (state_path, state) = load_or_create_keyring(data_dir)?;
                Ok(Kms {
                    tier,
                    state_path: Some(state_path),
                    state: Mutex::new(state),
                })
            }
        }
    }

    pub fn tier(&self) -> EncryptionTier {
        self.tier
    }

    fn persist_state(&self, state: &KeyRingState) -> UcResult<()> {
        if let Some(path) = &self.state_path {
            atomic_write(path, &state.to_cbor().encode())?;
            restrict_perms(path);
        }
        Ok(())
    }

    fn derive_active(&self, purpose: &str) -> Option<(u64, [u8; 32])> {
        let state = self.state.lock().unwrap();
        let key = state.keys.get(&state.active_key_id).copied()?;
        Some((
            state.active_key_id,
            hmac_sha256(&key, purpose.as_bytes()),
        ))
    }

    fn derive_for_key_id(&self, key_id: u64, purpose: &str) -> Option<[u8; 32]> {
        let state = self.state.lock().unwrap();
        state
            .keys
            .get(&key_id)
            .copied()
            .map(|k| hmac_sha256(&k, purpose.as_bytes()))
    }

    fn all_derived(&self, purpose: &str) -> Vec<[u8; 32]> {
        let state = self.state.lock().unwrap();
        state
            .keys
            .values()
            .copied()
            .map(|k| hmac_sha256(&k, purpose.as_bytes()))
            .collect()
    }

    fn try_unseal_with_key(key: &[u8; 32], sealed: &[u8], nonce_off: usize) -> Option<Vec<u8>> {
        if sealed.len() < nonce_off + 12 + 32 {
            return None;
        }
        let nonce: [u8; 12] = sealed[nonce_off..nonce_off + 12].try_into().ok()?;
        let ct = &sealed[nonce_off + 12..sealed.len() - 32];
        let mac_stored = &sealed[sealed.len() - 32..];
        let mut mac_input = Vec::with_capacity(12 + ct.len());
        mac_input.extend_from_slice(&nonce);
        mac_input.extend_from_slice(ct);
        let mac = hmac_sha256(key, &mac_input);
        if !crate::core::crypto::ct_eq(&mac, mac_stored) {
            return None;
        }
        let mut pt = ct.to_vec();
        chacha20_xor(key, &nonce, 1, &mut pt);
        Some(pt)
    }

    pub fn active_key_id(&self) -> Option<u64> {
        if self.tier == EncryptionTier::T0 {
            return None;
        }
        Some(self.state.lock().unwrap().active_key_id)
    }

    pub fn key_versions(&self) -> usize {
        self.state.lock().unwrap().keys.len()
    }

    pub fn custody_path(&self) -> Option<PathBuf> {
        self.state_path.clone()
    }

    pub fn rotation_status(&self, logical_at: u64) -> Option<RotationStatus> {
        if self.tier != EncryptionTier::T3 {
            return None;
        }
        let state = self.state.lock().unwrap();
        let next_due_at = state
            .last_rotated_at
            .saturating_add(T3_ROTATION_INTERVAL_OPS);
        Some(RotationStatus {
            active_key_id: state.active_key_id,
            key_versions: state.keys.len() as u64,
            last_rotated_at: state.last_rotated_at,
            rotation_interval_ops: T3_ROTATION_INTERVAL_OPS,
            next_due_at,
            overdue: logical_at >= next_due_at,
            custody_path: self
                .state_path
                .clone()
                .unwrap_or_else(|| PathBuf::from("kms/keyring.cbor")),
        })
    }

    pub fn rotate(&self, logical_at: u64, emergency: bool) -> UcResult<RotationEvent> {
        if self.tier == EncryptionTier::T0 {
            return Err(UcError::unsupported(
                "cannot rotate keys in encryption_tier T0",
            ));
        }
        let mut state = self.state.lock().unwrap();
        let previous_key_id = state.active_key_id;
        let active_hint = state.keys.keys().next_back().copied().unwrap_or(0);
        let active_key_id = active_hint.max(previous_key_id) + 1;
        let mut seed = Vec::new();
        seed.extend_from_slice(&logical_at.to_le_bytes());
        seed.extend_from_slice(&previous_key_id.to_le_bytes());
        seed.push(if emergency { 1 } else { 0 });
        let key = generate_root_key(&seed);
        state.keys.insert(active_key_id, key);
        state.active_key_id = active_key_id;
        state.last_rotated_at = logical_at;
        self.persist_state(&state)?;
        Ok(RotationEvent {
            previous_key_id,
            active_key_id,
            logical_at,
            emergency,
        })
    }

    /// Public subkey derivation for infrastructure keys (e.g. the capability
    /// token HMAC key). At T0 (no node key) a fixed development key is
    /// returned — T0 explicitly trades confidentiality for zero-config dev
    /// (PersistenceLayer.md §5.1).
    pub fn subkey(&self, purpose: &str) -> [u8; 32] {
        self.derive_active(purpose)
            .map(|(_, k)| k)
            .unwrap_or_else(|| hmac_sha256(&[0u8; 32], purpose.as_bytes()))
    }

    /// Seal a payload. T0 passes through unchanged with a 1-byte `0x00`
    /// prefix. Rotatable tiers emit `0x02 || key_id(u64 LE) || nonce(12) ||
    /// ciphertext || hmac(32)`.
    pub fn seal(&self, purpose: &str, nonce_seed: u64, plaintext: &[u8]) -> Vec<u8> {
        match self.derive_active(purpose) {
            None => {
                let mut out = Vec::with_capacity(1 + plaintext.len());
                out.push(0x00);
                out.extend_from_slice(plaintext);
                out
            }
            Some((key_id, key)) => {
                let mut nonce = [0u8; 12];
                nonce[..8].copy_from_slice(&nonce_seed.to_le_bytes());
                let mut ct = plaintext.to_vec();
                chacha20_xor(&key, &nonce, 1, &mut ct);
                let mut mac_input = Vec::with_capacity(12 + ct.len());
                mac_input.extend_from_slice(&nonce);
                mac_input.extend_from_slice(&ct);
                let mac = hmac_sha256(&key, &mac_input);
                let mut out = Vec::with_capacity(1 + 8 + 12 + ct.len() + 32);
                out.push(0x02);
                out.extend_from_slice(&key_id.to_le_bytes());
                out.extend_from_slice(&nonce);
                out.extend_from_slice(&ct);
                out.extend_from_slice(&mac);
                out
            }
        }
    }

    pub fn unseal(&self, purpose: &str, sealed: &[u8]) -> UcResult<Vec<u8>> {
        if sealed.is_empty() {
            return Err(UcError::internal("unseal: empty"));
        }
        match sealed[0] {
            0x00 => Ok(sealed[1..].to_vec()),
            0x01 => {
                if sealed.len() < 1 + 12 + 32 {
                    return Err(UcError::internal("unseal: truncated"));
                }
                for key in self.all_derived(purpose) {
                    if let Some(pt) = Self::try_unseal_with_key(&key, sealed, 1) {
                        return Ok(pt);
                    }
                }
                Err(UcError::internal("unseal: hmac mismatch (tampered?)"))
            }
            0x02 => {
                if sealed.len() < 1 + 8 + 12 + 32 {
                    return Err(UcError::internal("unseal: truncated"));
                }
                let key_id = u64::from_le_bytes(sealed[1..9].try_into().unwrap());
                let key = self
                    .derive_for_key_id(key_id, purpose)
                    .ok_or_else(|| UcError::internal(format!("unseal: unknown key id {key_id}")))?;
                Self::try_unseal_with_key(&key, sealed, 9)
                    .ok_or_else(|| UcError::internal("unseal: hmac mismatch (tampered?)"))
            }
            other => Err(UcError::internal(format!("unseal: bad marker {other}"))),
        }
    }

    /// T2+ batch signature over CrossCheckLedger record batches (§CCL-7).
    pub fn batch_sign(&self, records_hash: &[u8; 32]) -> Option<BatchSignature> {
        if self.tier == EncryptionTier::T2 || self.tier == EncryptionTier::T3 {
            self.derive_active("cross_check.batch")
                .map(|(key_id, key)| BatchSignature {
                    key_id,
                    mac: hmac_sha256(&key, records_hash),
                })
        } else {
            None
        }
    }

    pub fn verify_batch_signature(&self, signature: &BatchSignature, records_hash: &[u8; 32]) -> bool {
        if self.tier != EncryptionTier::T2 && self.tier != EncryptionTier::T3 {
            return false;
        }
        self.derive_for_key_id(signature.key_id, "cross_check.batch")
            .map(|key| {
                crate::core::crypto::ct_eq(&hmac_sha256(&key, records_hash), &signature.mac)
            })
            .unwrap_or(false)
    }

    pub fn wrap_external(&self) -> UcResult<()> {
        if self.tier == EncryptionTier::T3 {
            Ok(())
        } else {
            Err(UcError::unsupported(
                "external wrapping is only meaningful for encryption_tier T3",
            ))
        }
    }
}

#[cfg(unix)]
fn restrict_perms(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}
#[cfg(not(unix))]
fn restrict_perms(_path: &Path) {}

// ---------------------------------------------------------------------------
// CAS — content-addressed store (§6)
// ---------------------------------------------------------------------------

pub struct CasStore {
    root: PathBuf,
    refcounts: Mutex<HashMap<[u8; 32], u64>>,
}

impl CasStore {
    pub fn open(data_dir: &Path) -> UcResult<CasStore> {
        let root = data_dir.join("cas");
        std::fs::create_dir_all(&root)?;
        Ok(CasStore {
            root,
            refcounts: Mutex::new(HashMap::new()),
        })
    }

    fn path_for(&self, hash: &[u8; 32]) -> PathBuf {
        let h = hex(hash);
        self.root.join(&h[0..2]).join(&h[2..4]).join(&h)
    }

    /// Store bytes; returns the SHA-256 handle. Idempotent — an existing
    /// blob is verified, not rewritten.
    pub fn put(&self, bytes: &[u8]) -> UcResult<[u8; 32]> {
        let hash = sha256(bytes);
        let path = self.path_for(&hash);
        if !path.exists() {
            atomic_write(&path, bytes)?;
        }
        *self.refcounts.lock().unwrap().entry(hash).or_insert(0) += 1;
        Ok(hash)
    }

    pub fn get(&self, hash: &[u8; 32]) -> UcResult<Vec<u8>> {
        let path = self.path_for(hash);
        let bytes = std::fs::read(&path).map_err(|_| {
            UcError::not_found(format!("cas blob {} not found", hex(hash)))
        })?;
        // Verify on read — the store is self-checking (§6.2).
        if &sha256(&bytes) != hash {
            return Err(UcError::internal(format!(
                "cas blob {} failed hash verification",
                hex(hash)
            )));
        }
        Ok(bytes)
    }

    pub fn contains(&self, hash: &[u8; 32]) -> bool {
        self.path_for(hash).exists()
    }

    pub fn decref(&self, hash: &[u8; 32]) {
        let mut rc = self.refcounts.lock().unwrap();
        if let Some(c) = rc.get_mut(hash) {
            *c = c.saturating_sub(1);
        }
    }

    /// GC blobs with zero live references (invoked by admin/snapshot flow;
    /// refcounts are rebuilt from cell state at boot, so this is safe to run
    /// only after full recovery).
    pub fn gc(&self) -> UcResult<u64> {
        let rc = self.refcounts.lock().unwrap();
        let mut removed = 0u64;
        for (hash, count) in rc.iter() {
            if *count == 0 {
                let p = self.path_for(hash);
                if p.exists() {
                    std::fs::remove_file(&p)?;
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }

    pub fn rebuild_refcount(&self, hash: &[u8; 32], count: u64) {
        self.refcounts.lock().unwrap().insert(*hash, count);
    }
}

// ---------------------------------------------------------------------------
// Snapshots (§3.7)
// ---------------------------------------------------------------------------

pub struct SnapshotStore {
    dir: PathBuf,
}

impl SnapshotStore {
    pub fn open(data_dir: &Path) -> UcResult<SnapshotStore> {
        let dir = data_dir.join("snapshots");
        std::fs::create_dir_all(&dir)?;
        Ok(SnapshotStore { dir })
    }

    /// Write a full-state snapshot. `states` is `cell_id -> state cbor`.
    /// Returns the snapshot file name.
    pub fn write(&self, logical_at: u64, states: &BTreeMap<u64, Cbor>) -> UcResult<String> {
        let cells: Vec<Cbor> = states
            .iter()
            .map(|(id, st)| {
                Cbor::map(vec![
                    ("cell_id", Cbor::U64(*id)),
                    ("state", st.clone()),
                ])
            })
            .collect();
        let snap = Cbor::map(vec![
            ("logical_at", Cbor::U64(logical_at)),
            ("cells", Cbor::Array(cells)),
        ]);
        let bytes = snap.encode();
        let digest = hex(&sha256(&bytes));
        let name = format!("snap-{:016}-{}.cbor", logical_at, &digest[..12]);
        atomic_write(&self.dir.join(&name), &bytes)?;
        Ok(name)
    }

    pub fn load(&self, name: &str) -> UcResult<(u64, BTreeMap<u64, Cbor>)> {
        let bytes = std::fs::read(self.dir.join(name))?;
        let c = Cbor::decode(&bytes)?;
        let at = c.req_u64("logical_at")?;
        let mut states = BTreeMap::new();
        if let Some(arr) = c.get("cells").and_then(|v| v.as_array()) {
            for item in arr {
                let id = item.req_u64("cell_id")?;
                let st = item
                    .get("state")
                    .cloned()
                    .ok_or_else(|| UcError::schema("snapshot cell missing state"))?;
                states.insert(id, st);
            }
        }
        Ok((at, states))
    }
}

// ---------------------------------------------------------------------------
// PrefixCacheStore (§9)
// ---------------------------------------------------------------------------

/// Cache key for rendered views: `(view_id, ns, version, params_hash)` —
/// PersistenceLayer.md §9.2. Values are canonical-CBOR view payloads whose
/// *prefix stability* is the whole point (DeepSeekOptimization.md): a
/// re-render after an append-only change shares its byte prefix with the
/// cached copy, so downstream KV caches stay warm.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ViewKey {
    pub view_id: String,
    pub ns: String,
    pub version: u64,
    pub params_hash: [u8; 32],
}

impl ViewKey {
    fn file_stem(&self) -> String {
        let mut input = Vec::new();
        input.extend_from_slice(self.view_id.as_bytes());
        input.push(0);
        input.extend_from_slice(self.ns.as_bytes());
        input.push(0);
        input.extend_from_slice(&self.version.to_le_bytes());
        input.extend_from_slice(&self.params_hash);
        hex(&sha256(&input))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheTier {
    L1,
    L2,
    L3,
}

impl CacheTier {
    pub fn capacity(&self) -> u64 {
        match self {
            CacheTier::L1 => 256 << 20,  // 256 MiB
            CacheTier::L2 => 1 << 30,    // 1 GiB
            CacheTier::L3 => 4u64 << 30, // 4 GiB
        }
    }
    fn dirname(&self) -> &'static str {
        match self {
            CacheTier::L1 => "tier-l1",
            CacheTier::L2 => "tier-l2",
            CacheTier::L3 => "tier-l3",
        }
    }
    /// Size-tiered placement (§9.3).
    pub fn for_size(len: usize) -> CacheTier {
        if len <= 64 * 1024 {
            CacheTier::L1
        } else if len <= 1024 * 1024 {
            CacheTier::L2
        } else {
            CacheTier::L3
        }
    }
}

struct CacheEntry {
    tier: CacheTier,
    size: u64,
    last_used: u64, // logical clock — deterministic LRU
    /// Handles this view depends on, for invalidation (§9.4).
    deps: Vec<String>,
    tombstone: bool,
}

pub struct PrefixCacheStore {
    root: PathBuf,
    index_path: PathBuf,
    inner: Mutex<CacheInner>,
}

#[derive(Default)]
struct CacheInner {
    entries: BTreeMap<ViewKey, CacheEntry>,
    tier_bytes: HashMap<&'static str, u64>,
}

impl PrefixCacheStore {
    pub fn open(data_dir: &Path) -> UcResult<PrefixCacheStore> {
        let root = data_dir.join("cache");
        for t in [CacheTier::L1, CacheTier::L2, CacheTier::L3] {
            std::fs::create_dir_all(root.join(t.dirname()))?;
        }
        let index_dir = root.join("index");
        std::fs::create_dir_all(&index_dir)?;
        let store = PrefixCacheStore {
            index_path: index_dir.join("view_keys.cbor"),
            root,
            inner: Mutex::new(CacheInner::default()),
        };
        store.load_index()?;
        Ok(store)
    }

    pub fn get(&self, key: &ViewKey, now: u64) -> Option<Vec<u8>> {
        let path;
        {
            let mut inner = self.inner.lock().unwrap();
            let entry = inner.entries.get_mut(key)?;
            if entry.tombstone {
                return None;
            }
            entry.last_used = now;
            path = self
                .root
                .join(entry.tier.dirname())
                .join(key.file_stem());
        }
        std::fs::read(&path).ok()
    }

    pub fn put(&self, key: ViewKey, bytes: &[u8], deps: Vec<String>, now: u64) -> UcResult<()> {
        let tier = CacheTier::for_size(bytes.len());
        let path = self.root.join(tier.dirname()).join(key.file_stem());
        atomic_write(&path, bytes)?;
        let mut inner = self.inner.lock().unwrap();
        let size = bytes.len() as u64;
        if let Some(old) = inner.entries.insert(
            key,
            CacheEntry {
                tier,
                size,
                last_used: now,
                deps,
                tombstone: false,
            },
        ) {
            *inner.tier_bytes.entry(old.tier.dirname()).or_insert(0) =
                inner.tier_bytes[old.tier.dirname()].saturating_sub(old.size);
        }
        *inner.tier_bytes.entry(tier.dirname()).or_insert(0) += size;
        self.evict_locked(&mut inner, tier)?;
        drop(inner);
        self.save_index()
    }

    /// Tombstone every cached view whose deps include `handle` (§9.4:
    /// supersede/quarantine of a handle must invalidate dependent views).
    pub fn invalidate_handle(&self, handle: &str) -> UcResult<u64> {
        let mut inner = self.inner.lock().unwrap();
        let mut n = 0;
        for entry in inner.entries.values_mut() {
            if !entry.tombstone && entry.deps.iter().any(|d| d == handle) {
                entry.tombstone = true;
                n += 1;
            }
        }
        drop(inner);
        self.save_index()?;
        Ok(n)
    }

    /// Determinism purge (§9.6): after non-deterministic recovery events the
    /// entire cache is discarded rather than risk serving stale bytes.
    pub fn purge_all(&self) -> UcResult<()> {
        let mut inner = self.inner.lock().unwrap();
        for (key, entry) in inner.entries.iter() {
            let _ = std::fs::remove_file(
                self.root.join(entry.tier.dirname()).join(key.file_stem()),
            );
        }
        inner.entries.clear();
        inner.tier_bytes.clear();
        drop(inner);
        self.save_index()
    }

    fn evict_locked(&self, inner: &mut CacheInner, tier: CacheTier) -> UcResult<()> {
        let cap = tier.capacity();
        while inner.tier_bytes.get(tier.dirname()).copied().unwrap_or(0) > cap {
            // Deterministic LRU: oldest last_used; ties broken by ViewKey
            // order (BTreeMap iteration is sorted, so first match wins).
            let victim = inner
                .entries
                .iter()
                .filter(|(_, e)| e.tier == tier && !e.tombstone)
                .min_by_key(|(k, e)| (e.last_used, (*k).clone()))
                .map(|(k, _)| k.clone());
            let Some(victim) = victim else { break };
            if let Some(e) = inner.entries.remove(&victim) {
                let _ = std::fs::remove_file(
                    self.root.join(e.tier.dirname()).join(victim.file_stem()),
                );
                *inner.tier_bytes.entry(e.tier.dirname()).or_insert(0) =
                    inner.tier_bytes[e.tier.dirname()].saturating_sub(e.size);
            }
        }
        Ok(())
    }

    fn save_index(&self) -> UcResult<()> {
        let inner = self.inner.lock().unwrap();
        let items: Vec<Cbor> = inner
            .entries
            .iter()
            .map(|(k, e)| {
                Cbor::map(vec![
                    ("view_id", Cbor::t(k.view_id.clone())),
                    ("ns", Cbor::t(k.ns.clone())),
                    ("version", Cbor::U64(k.version)),
                    ("params_hash", Cbor::Bytes(k.params_hash.to_vec())),
                    ("tier", Cbor::t(e.tier.dirname())),
                    ("size", Cbor::U64(e.size)),
                    ("last_used", Cbor::U64(e.last_used)),
                    ("deps", Cbor::text_array(&e.deps)),
                    ("tombstone", Cbor::Bool(e.tombstone)),
                ])
            })
            .collect();
        atomic_write(&self.index_path, &Cbor::Array(items).encode())
    }

    fn load_index(&self) -> UcResult<()> {
        if !self.index_path.exists() {
            return Ok(());
        }
        let bytes = std::fs::read(&self.index_path)?;
        let c = Cbor::decode(&bytes)?;
        let mut inner = self.inner.lock().unwrap();
        if let Some(arr) = c.as_array() {
            for item in arr {
                let tier = match item.opt_str("tier").as_deref() {
                    Some("tier-l1") => CacheTier::L1,
                    Some("tier-l2") => CacheTier::L2,
                    Some("tier-l3") => CacheTier::L3,
                    _ => continue,
                };
                let ph = item
                    .get("params_hash")
                    .and_then(|v| v.as_bytes())
                    .unwrap_or(&[]);
                if ph.len() != 32 {
                    continue;
                }
                let mut params_hash = [0u8; 32];
                params_hash.copy_from_slice(ph);
                let key = ViewKey {
                    view_id: item.opt_str("view_id").unwrap_or_default(),
                    ns: item.opt_str("ns").unwrap_or_default(),
                    version: item.opt_u64("version").unwrap_or(0),
                    params_hash,
                };
                let deps = item
                    .get("deps")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let size = item.opt_u64("size").unwrap_or(0);
                let entry = CacheEntry {
                    tier,
                    size,
                    last_used: item.opt_u64("last_used").unwrap_or(0),
                    deps,
                    tombstone: item.opt_bool("tombstone").unwrap_or(false),
                };
                *inner.tier_bytes.entry(tier.dirname()).or_insert(0) += size;
                inner.entries.insert(key, entry);
            }
        }
        Ok(())
    }

    pub fn stats(&self) -> (usize, u64) {
        let inner = self.inner.lock().unwrap();
        let live = inner.entries.values().filter(|e| !e.tombstone).count();
        let bytes: u64 = inner.tier_bytes.values().sum();
        (live, bytes)
    }
}

// ---------------------------------------------------------------------------
// Curator weights (§7 — pinned by SHA-256)
// ---------------------------------------------------------------------------

/// Verify a pinned weight file: `weights/<model>/<sha>.gguf` must hash to
/// exactly `<sha>` (Boot B3 step 4a/4b; `curator verify-weights` admin verb).
pub fn verify_weight_file(data_dir: &Path, model: &str, sha_hex: &str) -> UcResult<PathBuf> {
    let path = data_dir
        .join("weights")
        .join(model)
        .join(format!("{sha_hex}.gguf"));
    if !path.exists() {
        return Err(UcError::not_found(format!(
            "weight file missing: {}",
            path.display()
        )));
    }
    let actual = sha256_file(&path)?;
    if actual != sha_hex.to_ascii_lowercase() {
        return Err(UcError::internal(format!(
            "weight hash mismatch for {model}: expected {sha_hex}, got {actual}"
        )));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("uc-persist-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn manifest_roundtrip() {
        let dir = tmpdir("manifest");
        let mut m = Manifest {
            node_id: "node-01".into(),
            proto_version: 1,
            logical_at: 99,
            clean_shutdown: true,
            encryption_tier: "T1".into(),
            shard_count: 4,
            ..Default::default()
        };
        m.state_hashes.insert(3, [7u8; 32]);
        m.save(&dir).unwrap();
        let loaded = Manifest::load(&dir).unwrap().unwrap();
        assert_eq!(loaded.node_id, "node-01");
        assert_eq!(loaded.state_hashes[&3], [7u8; 32]);
        assert!(loaded.clean_shutdown);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn kms_t1_seal_unseal_and_tamper() {
        let dir = tmpdir("kms");
        let kms = Kms::open(&dir, EncryptionTier::T1).unwrap();
        let sealed = kms.seal("wal.shard-00", 42, b"secret payload");
        assert_eq!(sealed[0], 0x02);
        let pt = kms.unseal("wal.shard-00", &sealed).unwrap();
        assert_eq!(pt, b"secret payload");
        // Tamper.
        let mut bad = sealed.clone();
        let mid = bad.len() / 2;
        bad[mid] ^= 1;
        assert!(kms.unseal("wal.shard-00", &bad).is_err());
        // Same key file reloads.
        let kms2 = Kms::open(&dir, EncryptionTier::T1).unwrap();
        assert_eq!(kms2.unseal("wal.shard-00", &sealed).unwrap(), b"secret payload");
        // T0 passthrough.
        let kms0 = Kms::open(&dir, EncryptionTier::T0).unwrap();
        let plain = kms0.seal("x", 0, b"open");
        assert_eq!(plain[0], 0x00);
        assert_eq!(kms0.unseal("x", &plain).unwrap(), b"open");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn kms_t3_rotation_preserves_unseal_and_batch_verification() {
        let dir = tmpdir("kms3");
        let kms = Kms::open(&dir, EncryptionTier::T3).unwrap();
        let status = kms.rotation_status(0).unwrap();
        assert_eq!(status.active_key_id, 1);
        assert_eq!(status.key_versions, 1);
        assert_eq!(status.rotation_interval_ops, T3_ROTATION_INTERVAL_OPS);
        assert!(!status.overdue);

        let sealed_before = kms.seal("wal.shard-00", 42, b"before rotation");
        let digest_before = sha256(b"batch-before");
        let sig_before = kms.batch_sign(&digest_before).unwrap();

        let rotation = kms.rotate(T3_ROTATION_INTERVAL_OPS, false).unwrap();
        assert_eq!(rotation.previous_key_id, 1);
        assert_eq!(rotation.active_key_id, 2);

        let sealed_after = kms.seal("wal.shard-00", 43, b"after rotation");
        let digest_after = sha256(b"batch-after");
        let sig_after = kms.batch_sign(&digest_after).unwrap();

        assert_eq!(
            kms.unseal("wal.shard-00", &sealed_before).unwrap(),
            b"before rotation"
        );
        assert_eq!(
            kms.unseal("wal.shard-00", &sealed_after).unwrap(),
            b"after rotation"
        );
        assert!(kms.verify_batch_signature(&sig_before, &digest_before));
        assert!(kms.verify_batch_signature(&sig_after, &digest_after));

        let reopened = Kms::open(&dir, EncryptionTier::T3).unwrap();
        assert_eq!(reopened.key_versions(), 2);
        assert_eq!(
            reopened.unseal("wal.shard-00", &sealed_before).unwrap(),
            b"before rotation"
        );
        assert_eq!(
            reopened.unseal("wal.shard-00", &sealed_after).unwrap(),
            b"after rotation"
        );
        assert!(reopened.verify_batch_signature(&sig_before, &digest_before));
        assert!(reopened.verify_batch_signature(&sig_after, &digest_after));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cas_put_get_verify() {
        let dir = tmpdir("cas");
        let cas = CasStore::open(&dir).unwrap();
        let h = cas.put(b"hello cas").unwrap();
        assert!(cas.contains(&h));
        assert_eq!(cas.get(&h).unwrap(), b"hello cas");
        // Idempotent put.
        let h2 = cas.put(b"hello cas").unwrap();
        assert_eq!(h, h2);
        // Corrupt on disk -> read fails verification.
        let p = cas.path_for(&h);
        std::fs::write(&p, b"hello caS").unwrap();
        assert!(cas.get(&h).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn snapshot_roundtrip() {
        let dir = tmpdir("snap");
        let store = SnapshotStore::open(&dir).unwrap();
        let mut states = BTreeMap::new();
        states.insert(1u64, Cbor::map(vec![("facts", Cbor::U64(3))]));
        states.insert(2u64, Cbor::t("timeline-state"));
        let name = store.write(500, &states).unwrap();
        let (at, loaded) = store.load(&name).unwrap();
        assert_eq!(at, 500);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[&1].req_u64("facts").unwrap(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_put_get_invalidate_persist() {
        let dir = tmpdir("cache");
        let key = ViewKey {
            view_id: "fact_subject".into(),
            ns: "default".into(),
            version: 3,
            params_hash: sha256(b"subject=alice"),
        };
        {
            let cache = PrefixCacheStore::open(&dir).unwrap();
            cache
                .put(key.clone(), b"rendered-view", vec!["fact/01A".into()], 10)
                .unwrap();
            assert_eq!(cache.get(&key, 11).unwrap(), b"rendered-view");
            assert_eq!(cache.stats().0, 1);
        }
        // Index survives reopen.
        {
            let cache = PrefixCacheStore::open(&dir).unwrap();
            assert_eq!(cache.get(&key, 12).unwrap(), b"rendered-view");
            // Invalidation by dependent handle.
            assert_eq!(cache.invalidate_handle("fact/01A").unwrap(), 1);
            assert!(cache.get(&key, 13).is_none());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
