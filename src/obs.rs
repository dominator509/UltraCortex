//! Observability — SPEC-DERIVED-§1 (ObservabilityAudit.md).
//!
//! Three facilities:
//! 1. [`Metrics`] — lock-cheap counter/gauge/histogram registry. Curator
//!    metrics named per CURATOR_PAIR_PROTOCOL.md §9 (e.g.
//!    `curator.rationale_access_denied`, `curator.suspicious_agreement`).
//! 2. [`Logger`] — JSON-lines structured log to a file (and optionally
//!    stderr), timestamped with the *logical* clock, never wall-clock, so
//!    replay is byte-stable.
//! 3. [`AuditChain`] — append-only hash chain. Each record is canonical CBOR
//!    `{event, fields, logical_at, prev, seq}`; the on-disk line is
//!    `hex(cbor) SP hex(sha256(cbor))`. `verify()` walks the chain and
//!    confirms every `prev` equals the previous record's hash — this is what
//!    `ultracortex audit verify` runs (BootstrapOperator.md §7).

use crate::core::cbor::Cbor;
use crate::core::crypto::{hex, sha256};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct Metrics {
    counters: Mutex<BTreeMap<String, AtomicU64>>,
    gauges: Mutex<BTreeMap<String, AtomicI64>>,
    histos: Mutex<BTreeMap<String, Histo>>,
}

#[derive(Default)]
struct Histo {
    count: u64,
    sum: u64,
    max: u64,
    // Power-of-two microsecond buckets: [1,2,4,...,2^31].
    buckets: [u64; 32],
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn inc(&self, name: &str) {
        self.add(name, 1);
    }

    pub fn add(&self, name: &str, v: u64) {
        let mut map = self.counters.lock().unwrap();
        map.entry(name.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(v, Ordering::Relaxed);
    }

    pub fn counter(&self, name: &str) -> u64 {
        self.counters
            .lock()
            .unwrap()
            .get(name)
            .map(|a| a.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    pub fn gauge_set(&self, name: &str, v: i64) {
        let mut map = self.gauges.lock().unwrap();
        map.entry(name.to_string())
            .or_insert_with(|| AtomicI64::new(0))
            .store(v, Ordering::Relaxed);
    }

    pub fn gauge(&self, name: &str) -> i64 {
        self.gauges
            .lock()
            .unwrap()
            .get(name)
            .map(|a| a.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    pub fn observe(&self, name: &str, value: u64) {
        let mut map = self.histos.lock().unwrap();
        let h = map.entry(name.to_string()).or_default();
        h.count += 1;
        h.sum += value;
        if value > h.max {
            h.max = value;
        }
        let idx = (64 - value.max(1).leading_zeros() as usize - 1).min(31);
        h.buckets[idx] += 1;
    }

    /// Flat snapshot for `curator status` / debugging: `name -> value`.
    pub fn snapshot(&self) -> BTreeMap<String, i64> {
        let mut out = BTreeMap::new();
        for (k, v) in self.counters.lock().unwrap().iter() {
            out.insert(k.clone(), v.load(Ordering::Relaxed) as i64);
        }
        for (k, v) in self.gauges.lock().unwrap().iter() {
            out.insert(format!("{k}(gauge)"), v.load(Ordering::Relaxed));
        }
        for (k, h) in self.histos.lock().unwrap().iter() {
            out.insert(format!("{k}.count"), h.count as i64);
            if h.count > 0 {
                out.insert(
                    format!("{k}.mean"),
                    h.sum.checked_div(h.count).unwrap_or(0) as i64,
                );
                out.insert(format!("{k}.max"), h.max as i64);
            }
        }
        out
    }

    /// Encode the current in-process registry as OTLP/HTTP JSON. The exporter
    /// is intentionally explicit: metric increments never perform network IO.
    pub fn otlp_json(&self) -> String {
        let snapshot = self.snapshot();
        let mut metrics = String::new();
        for (idx, (name, value)) in snapshot.iter().enumerate() {
            if idx > 0 {
                metrics.push(',');
            }
            let (metric_name, kind) = if let Some(name) = name.strip_suffix("(gauge)") {
                (name, "gauge")
            } else {
                (name.as_str(), "sum")
            };
            metrics.push('{');
            push_json_kv(&mut metrics, "name", metric_name, true);
            metrics.push(',');
            if kind == "gauge" {
                metrics.push_str("\"gauge\":{\"dataPoints\":[{\"asInt\":");
                metrics.push_str(&value.to_string());
                metrics.push_str("}]}");
            } else {
                metrics.push_str("\"sum\":{\"aggregationTemporality\":2,\"isMonotonic\":true,\"dataPoints\":[{\"asInt\":");
                metrics.push_str(&value.to_string());
                metrics.push_str("}]}");
            }
            metrics.push('}');
        }
        format!(
            "{{\"resourceMetrics\":[{{\"resource\":{{\"attributes\":[{{\"key\":\"service.name\",\"value\":{{\"stringValue\":\"ultracortex\"}}}}]}},\"scopeMetrics\":[{{\"scope\":{{\"name\":\"ultracortex.obs\"}},\"metrics\":[{metrics}]}}]}}]}}"
        )
    }
}

// ---------------------------------------------------------------------------
// OTLP/HTTP exporter
// ---------------------------------------------------------------------------

/// Dependency-free OTLP/HTTP JSON configuration. The endpoints match the
/// OpenTelemetry Collector defaults and can be overridden per node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OtlpConfig {
    pub enabled: bool,
    pub metrics_endpoint: String,
    pub traces_endpoint: String,
    pub logs_endpoint: String,
    pub timeout_ms: u64,
}

impl Default for OtlpConfig {
    fn default() -> Self {
        OtlpConfig {
            enabled: true,
            metrics_endpoint: "http://127.0.0.1:4318/v1/metrics".into(),
            traces_endpoint: "http://127.0.0.1:4318/v1/traces".into(),
            logs_endpoint: "http://127.0.0.1:4318/v1/logs".into(),
            timeout_ms: 1_000,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OtlpExportReceipt {
    pub endpoint: String,
    pub status_code: Option<u16>,
    pub bytes: usize,
    pub skipped: bool,
}

pub struct OtlpExporter {
    config: Mutex<OtlpConfig>,
}

impl OtlpExporter {
    pub fn new(config: OtlpConfig) -> OtlpExporter {
        OtlpExporter {
            config: Mutex::new(config),
        }
    }

    pub fn config(&self) -> OtlpConfig {
        self.config.lock().unwrap().clone()
    }

    pub fn configure(&self, config: OtlpConfig) {
        *self.config.lock().unwrap() = config;
    }

    /// Export a point-in-time metric snapshot. This is a best-effort,
    /// operator-triggered path so a collector outage cannot stall the Router.
    pub fn export_metrics(&self, metrics: &Metrics) -> Result<OtlpExportReceipt, String> {
        let config = self.config();
        if !config.enabled {
            return Ok(OtlpExportReceipt {
                endpoint: config.metrics_endpoint,
                status_code: None,
                bytes: 0,
                skipped: true,
            });
        }
        let body = metrics.otlp_json();
        let (status_code, bytes) = post_json(
            &config.metrics_endpoint,
            &body,
            Duration::from_millis(config.timeout_ms.max(1)),
        )?;
        if !(200..300).contains(&status_code) {
            return Err(format!(
                "OTLP metrics endpoint {} returned HTTP {status_code}",
                config.metrics_endpoint
            ));
        }
        Ok(OtlpExportReceipt {
            endpoint: config.metrics_endpoint,
            status_code: Some(status_code),
            bytes,
            skipped: false,
        })
    }
}

fn post_json(endpoint: &str, body: &str, timeout: Duration) -> Result<(u16, usize), String> {
    let (host, port, path) = parse_http_endpoint(endpoint)?;
    let mut addrs = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| format!("OTLP endpoint {endpoint}: resolve failed: {e}"))?;
    let addr = addrs
        .next()
        .ok_or_else(|| format!("OTLP endpoint {endpoint}: no resolved address"))?;
    let mut stream = TcpStream::connect_timeout(&addr, timeout)
        .map_err(|e| format!("OTLP endpoint {endpoint}: connect failed: {e}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| format!("OTLP endpoint {endpoint}: read timeout failed: {e}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| format!("OTLP endpoint {endpoint}: write timeout failed: {e}"))?;
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("OTLP endpoint {endpoint}: write failed: {e}"))?;
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|e| format!("OTLP endpoint {endpoint}: read failed: {e}"))?;
    let status_line = response
        .split(|b| *b == b'\n')
        .next()
        .ok_or_else(|| format!("OTLP endpoint {endpoint}: empty response"))?;
    let status_code = status_line
        .split(|b| *b == b' ')
        .nth(1)
        .and_then(|raw| std::str::from_utf8(raw).ok())
        .and_then(|raw| raw.trim().parse::<u16>().ok())
        .ok_or_else(|| format!("OTLP endpoint {endpoint}: malformed HTTP status"))?;
    Ok((status_code, body.len()))
}

fn parse_http_endpoint(endpoint: &str) -> Result<(String, u16, String), String> {
    let rest = endpoint
        .strip_prefix("http://")
        .ok_or_else(|| format!("OTLP endpoint must use http://: {endpoint}"))?;
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let path = if path.is_empty() {
        "/".to_string()
    } else {
        format!("/{path}")
    };
    let (host, port) = if let Some((host, port)) = authority.rsplit_once(':') {
        (
            host.trim_matches(['[', ']']).to_string(),
            port.parse::<u16>()
                .map_err(|_| format!("OTLP endpoint has invalid port: {endpoint}"))?,
        )
    } else {
        (authority.to_string(), 80)
    };
    if host.is_empty() {
        return Err(format!("OTLP endpoint has empty host: {endpoint}"));
    }
    Ok((host, port, path))
}

// ---------------------------------------------------------------------------
// Logger
// ---------------------------------------------------------------------------

pub struct Logger {
    file: Mutex<Option<BufWriter<File>>>,
    also_stderr: bool,
}

impl Logger {
    pub fn new(path: Option<&Path>, also_stderr: bool) -> std::io::Result<Logger> {
        let file = match path {
            Some(p) => {
                if let Some(parent) = p.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                Some(BufWriter::new(
                    OpenOptions::new().create(true).append(true).open(p)?,
                ))
            }
            None => None,
        };
        Ok(Logger {
            file: Mutex::new(file),
            also_stderr,
        })
    }

    pub fn null() -> Logger {
        Logger {
            file: Mutex::new(None),
            also_stderr: false,
        }
    }

    pub fn log(&self, logical_at: u64, level: &str, event: &str, fields: &[(&str, String)]) {
        let mut line = String::with_capacity(128);
        line.push('{');
        push_json_kv(&mut line, "at", &logical_at.to_string(), false);
        line.push(',');
        push_json_kv(&mut line, "level", level, true);
        line.push(',');
        push_json_kv(&mut line, "event", event, true);
        for (k, v) in fields {
            line.push(',');
            push_json_kv(&mut line, k, v, true);
        }
        line.push('}');
        line.push('\n');
        if self.also_stderr {
            eprint!("{line}");
        }
        let mut guard = self.file.lock().unwrap();
        if let Some(w) = guard.as_mut() {
            let _ = w.write_all(line.as_bytes());
            let _ = w.flush();
        }
    }

    pub fn info(&self, at: u64, event: &str, fields: &[(&str, String)]) {
        self.log(at, "info", event, fields);
    }
    pub fn warn(&self, at: u64, event: &str, fields: &[(&str, String)]) {
        self.log(at, "warn", event, fields);
    }
    pub fn error(&self, at: u64, event: &str, fields: &[(&str, String)]) {
        self.log(at, "error", event, fields);
    }
}

fn push_json_kv(buf: &mut String, k: &str, v: &str, quote_value: bool) {
    buf.push('"');
    json_escape_into(buf, k);
    buf.push_str("\":");
    if quote_value {
        buf.push('"');
        json_escape_into(buf, v);
        buf.push('"');
    } else {
        buf.push_str(v);
    }
}

fn json_escape_into(buf: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '"' => buf.push_str("\\\""),
            '\\' => buf.push_str("\\\\"),
            '\n' => buf.push_str("\\n"),
            '\r' => buf.push_str("\\r"),
            '\t' => buf.push_str("\\t"),
            c if (c as u32) < 0x20 => buf.push_str(&format!("\\u{:04x}", c as u32)),
            c => buf.push(c),
        }
    }
}

// ---------------------------------------------------------------------------
// AuditChain
// ---------------------------------------------------------------------------

pub struct AuditChain {
    path: PathBuf,
    inner: Mutex<ChainState>,
}

struct ChainState {
    writer: Option<BufWriter<File>>,
    prev_hash: [u8; 32],
    seq: u64,
    #[cfg(test)]
    fail_next_append: bool,
}

impl AuditChain {
    /// Open (or create) the audit chain at `path`. On open, the existing
    /// chain is scanned to recover `(prev_hash, seq)`; a corrupt tail is a
    /// hard error — the operator must run `audit verify` and intervene.
    pub fn open(path: &Path) -> Result<AuditChain, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let (prev_hash, seq) = if path.exists() {
            let (h, s, ok) = Self::scan(path)?;
            if !ok {
                return Err(format!(
                    "audit chain at {} is corrupt; run `ultracortex audit verify`",
                    path.display()
                ));
            }
            (h, s)
        } else {
            ([0u8; 32], 0)
        };
        let writer = BufWriter::new(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| e.to_string())?,
        );
        Ok(AuditChain {
            path: path.to_path_buf(),
            inner: Mutex::new(ChainState {
                writer: Some(writer),
                prev_hash,
                seq,
                #[cfg(test)]
                fail_next_append: false,
            }),
        })
    }

    /// In-memory chain for tests.
    pub fn ephemeral() -> AuditChain {
        AuditChain {
            path: PathBuf::new(),
            inner: Mutex::new(ChainState {
                writer: None,
                prev_hash: [0u8; 32],
                seq: 0,
                #[cfg(test)]
                fail_next_append: false,
            }),
        }
    }

    #[cfg(test)]
    pub fn fail_next_append(&self) {
        self.inner.lock().unwrap().fail_next_append = true;
    }

    /// Append an audit record. Returns the record's hash (the new chain head).
    pub fn append(
        &self,
        logical_at: u64,
        event: &str,
        fields: &[(&str, Cbor)],
    ) -> Result<[u8; 32], String> {
        let mut st = self.inner.lock().unwrap();
        #[cfg(test)]
        if st.fail_next_append {
            st.fail_next_append = false;
            return Err("injected audit append failure".into());
        }
        // encode() applies canonical key ordering, so insertion order is
        // irrelevant here.
        let fmap = Cbor::map(fields.iter().map(|(k, v)| (*k, v.clone())).collect());
        let record = Cbor::map(vec![
            ("event", Cbor::t(event)),
            ("fields", fmap),
            ("logical_at", Cbor::U64(logical_at)),
            ("prev", Cbor::Bytes(st.prev_hash.to_vec())),
            ("seq", Cbor::U64(st.seq)),
        ]);
        let bytes = record.encode();
        let hash = sha256(&bytes);
        let line = format!("{} {}\n", hex(&bytes), hex(&hash));
        if let Some(w) = st.writer.as_mut() {
            w.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
            w.flush().map_err(|e| e.to_string())?;
            w.get_ref().sync_data().map_err(|e| e.to_string())?;
        }
        st.prev_hash = hash;
        st.seq += 1;
        Ok(hash)
    }

    /// Flush and sync the audit file for callers that need an explicit
    /// durability barrier after a batch of forensic records.
    pub fn sync(&self) -> Result<(), String> {
        let mut st = self.inner.lock().unwrap();
        if let Some(w) = st.writer.as_mut() {
            w.flush().map_err(|e| e.to_string())?;
            w.get_ref().sync_data().map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn head(&self) -> ([u8; 32], u64) {
        let st = self.inner.lock().unwrap();
        (st.prev_hash, st.seq)
    }

    /// Verify the entire chain on disk. Returns (records_verified, ok).
    pub fn verify(path: &Path) -> Result<(u64, bool), String> {
        if !path.exists() {
            return Ok((0, true));
        }
        let (_, seq, ok) = Self::scan(path)?;
        Ok((seq, ok))
    }

    /// Scan the chain file; returns (last_hash, count, chain_intact).
    fn scan(path: &Path) -> Result<([u8; 32], u64, bool), String> {
        let f = File::open(path).map_err(|e| e.to_string())?;
        let reader = BufReader::new(f);
        let mut prev = [0u8; 32];
        let mut seq: u64 = 0;
        for (lineno, line) in reader.lines().enumerate() {
            let line = line.map_err(|e| e.to_string())?;
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let mut parts = line.splitn(2, ' ');
            let body_hex = parts.next().unwrap_or("");
            let hash_hex = parts.next().unwrap_or("");
            let body = unhex(body_hex)
                .ok_or_else(|| format!("audit line {}: bad hex body", lineno + 1))?;
            let claimed = unhex(hash_hex)
                .ok_or_else(|| format!("audit line {}: bad hex hash", lineno + 1))?;
            let actual = sha256(&body);
            if claimed.len() != 32 || actual[..] != claimed[..] {
                return Ok((prev, seq, false));
            }
            // Verify prev linkage + seq.
            let rec = Cbor::decode(&body)
                .map_err(|e| format!("audit line {}: cbor: {}", lineno + 1, e))?;
            let rec_prev = rec.get("prev").and_then(|c| c.as_bytes()).unwrap_or(&[]);
            let rec_seq = rec.get("seq").and_then(|c| c.as_u64()).unwrap_or(u64::MAX);
            if rec_prev != prev || rec_seq != seq {
                return Ok((prev, seq, false));
            }
            prev.copy_from_slice(&actual);
            seq += 1;
        }
        Ok((prev, seq, true))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = hexval(bytes[i])?;
        let lo = hexval(bytes[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Some(out)
}

fn hexval(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_roundtrip() {
        let m = Metrics::new();
        m.inc("curator.rationale_access_denied");
        m.add("curator.rationale_access_denied", 2);
        assert_eq!(m.counter("curator.rationale_access_denied"), 3);
        m.gauge_set("quarantine.depth", 7);
        assert_eq!(m.gauge("quarantine.depth"), 7);
        m.observe("router.write_us", 100);
        m.observe("router.write_us", 300);
        let snap = m.snapshot();
        assert_eq!(snap["router.write_us.count"], 2);
    }

    #[test]
    fn audit_chain_verify_detects_tamper() {
        let dir = std::env::temp_dir().join(format!("uc-audit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("audit.chain");
        {
            let chain = AuditChain::open(&path).unwrap();
            chain
                .append(1, "boot.start", &[("node", Cbor::t("n1"))])
                .unwrap();
            chain
                .append(2, "boot.ready", &[("cells", Cbor::U64(25))])
                .unwrap();
        }
        let (n, ok) = AuditChain::verify(&path).unwrap();
        assert_eq!(n, 2);
        assert!(ok);
        // Reopen resumes the chain.
        {
            let chain = AuditChain::open(&path).unwrap();
            assert_eq!(chain.head().1, 2);
            chain.append(3, "shutdown.clean", &[]).unwrap();
        }
        let (n, ok) = AuditChain::verify(&path).unwrap();
        assert_eq!(n, 3);
        assert!(ok);
        // Tamper with byte in the middle.
        let mut data = std::fs::read(&path).unwrap();
        let mid = data.len() / 2;
        data[mid] = data[mid].wrapping_add(1);
        std::fs::write(&path, &data).unwrap();
        let (_, ok) = AuditChain::verify(&path).unwrap();
        assert!(!ok);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn logger_writes_json_lines() {
        let dir = std::env::temp_dir().join(format!("uc-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("node.log");
        let log = Logger::new(Some(&path), false).unwrap();
        log.info(42, "test.event", &[("k", "v\"q".to_string())]);
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"at\":42"));
        assert!(content.contains("\\\"q"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn otlp_defaults_are_conventional_and_overrideable() {
        let cfg = OtlpConfig::default();
        assert_eq!(cfg.metrics_endpoint, "http://127.0.0.1:4318/v1/metrics");
        assert_eq!(cfg.traces_endpoint, "http://127.0.0.1:4318/v1/traces");
        assert_eq!(cfg.logs_endpoint, "http://127.0.0.1:4318/v1/logs");
        assert!(cfg.enabled);
        assert!(parse_http_endpoint("http://localhost:4318/v1/metrics").is_ok());
        assert!(parse_http_endpoint("https://localhost:4318/v1/metrics").is_err());
    }

    #[test]
    fn otlp_metrics_smoke_posts_json_to_loopback_collector() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            let mut expected = None;
            loop {
                let n = stream.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
                if expected.is_none() {
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]);
                        expected = headers.lines().find_map(|line| {
                            line.strip_prefix("Content-Length: ")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        });
                    }
                }
                if let Some(body_len) = expected {
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        if request.len() >= end + 4 + body_len {
                            break;
                        }
                    }
                }
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .unwrap();
            String::from_utf8_lossy(&request).into_owned()
        });

        let cfg = OtlpConfig {
            metrics_endpoint: format!("http://{addr}/v1/metrics"),
            ..OtlpConfig::default()
        };
        let exporter = OtlpExporter::new(cfg);
        let metrics = Metrics::new();
        metrics.inc("ultracortex.test");
        let receipt = exporter.export_metrics(&metrics).unwrap();
        assert_eq!(receipt.status_code, Some(200));
        assert!(!receipt.skipped);
        let request = server.join().unwrap();
        assert!(request.contains("POST /v1/metrics HTTP/1.1"));
        assert!(request.contains("\"resourceMetrics\""));
        assert!(request.contains("ultracortex.test"));
    }
}
