//! Wire protocol — SPEC-DERIVED-§2–§6 (McpProtocol.md).
//!
//! Transport framing is deliberately dumb: `u32 LE length ‖ canonical CBOR
//! body`, 16 MiB cap. Messages are CBOR maps with a `type` field:
//!
//! - `hello` / `hello_ack` — handshake; the ack advertises
//!   `capability_bits` (e.g. `semantic_check_supported`) so clients can
//!   feature-detect without version sniffing (§3.2).
//! - `request` / `response` — an [`Envelope`] and its
//!   [`ResponseEnvelope`], each as canonical CBOR.
//! - `admin` / `admin_ack` — operator-plane verbs, dispatched to
//!   [`crate::bootstrap::admin_dispatch`].
//!
//! Listeners: a Unix domain socket (chmod 0600 — same-user only, §2.1) is
//! the preferred transport; a TCP listener bound to 127.0.0.1:7741 is
//! opt-in for tooling that can't speak UDS. No TLS in v0 — the node is a
//! localhost substrate; multi-host is out of scope
//! (IMPLEMENTATION_STATUS.md §4).

use crate::core::cbor::Cbor;
use crate::core::{UcError, UcResult};
use crate::node::Node;
use crate::router::envelope::{Envelope, ResponseEnvelope, PROTO_VERSION};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread::JoinHandle;

pub const MAX_FRAME: usize = 16 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Frame codec
// ---------------------------------------------------------------------------

pub fn write_frame(w: &mut (impl Write + ?Sized), bytes: &[u8]) -> std::io::Result<()> {
    if bytes.len() > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame of {} bytes exceeds MAX_FRAME", bytes.len()),
        ));
    }
    w.write_all(&(bytes.len() as u32).to_le_bytes())?;
    w.write_all(bytes)?;
    w.flush()
}

/// Read one frame. `Ok(None)` on clean EOF at a frame boundary.
pub fn read_frame(r: &mut (impl Read + ?Sized)) -> std::io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("incoming frame of {len} bytes exceeds MAX_FRAME"),
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(Some(buf))
}

// ---------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------

pub fn hello_ack(node: &Node) -> Cbor {
    Cbor::map(vec![
        ("type", Cbor::t("hello_ack")),
        ("node_id", Cbor::t(node.node_id.clone())),
        ("proto_version", Cbor::U64(PROTO_VERSION)),
        (
            "capability_bits",
            Cbor::map(vec![
                ("semantic_check_supported", Cbor::Bool(true)),
                (
                    "tiers",
                    Cbor::text_array(&["L0", "L1", "L2", "L3"].map(String::from)),
                ),
                ("views_builtin", Cbor::Bool(true)),
                ("cross_check_ledger", Cbor::Bool(true)),
            ]),
        ),
    ])
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

pub fn validate_tcp_listen_addr(addr: &str) -> UcResult<()> {
    let addrs: Vec<_> = addr
        .to_socket_addrs()
        .map_err(|e| UcError::schema(format!("bad listen.tcp address `{addr}`: {e}")))?
        .collect();
    if addrs.is_empty() {
        return Err(UcError::schema(format!(
            "listen.tcp address `{addr}` resolved to no socket addresses"
        )));
    }
    if addrs.iter().any(|sock| !sock.ip().is_loopback()) {
        return Err(UcError::schema(format!(
            "listen.tcp `{addr}` is non-loopback; this checkout only permits plaintext TCP on loopback"
        )));
    }
    Ok(())
}

fn handle_message(node: &Arc<Node>, msg: &Cbor) -> Cbor {
    match msg.opt_str("type").as_deref() {
        Some("hello") => hello_ack(node),
        Some("request") => {
            let body = msg.get("envelope").cloned().unwrap_or(Cbor::Null);
            match Envelope::from_cbor(&body) {
                Ok(env) => {
                    let resp = crate::router::handle_envelope(node, &env);
                    Cbor::map(vec![
                        ("type", Cbor::t("response")),
                        ("response", resp.to_cbor()),
                    ])
                }
                Err(e) => {
                    let resp =
                        ResponseEnvelope::err(crate::core::ulid::Ulid::nil(), node.now(), &e);
                    Cbor::map(vec![
                        ("type", Cbor::t("response")),
                        ("response", resp.to_cbor()),
                    ])
                }
            }
        }
        Some("admin") => {
            let result = crate::bootstrap::admin_dispatch(node, msg);
            match result {
                Ok(body) => Cbor::map(vec![
                    ("type", Cbor::t("admin_ack")),
                    ("ok", Cbor::Bool(true)),
                    ("result", body),
                ]),
                Err(e) => Cbor::map(vec![
                    ("type", Cbor::t("admin_ack")),
                    ("ok", Cbor::Bool(false)),
                    ("err_code", Cbor::t(e.code.as_str())),
                    ("err_message", Cbor::t(e.message.clone())),
                ]),
            }
        }
        other => Cbor::map(vec![
            ("type", Cbor::t("error")),
            (
                "err_message",
                Cbor::t(format!("unknown message type {other:?}")),
            ),
        ]),
    }
}

fn serve_stream(node: Arc<Node>, mut stream: impl Read + Write) {
    loop {
        if node.is_shutting_down() {
            return;
        }
        let frame = match read_frame(&mut stream) {
            Ok(Some(f)) => f,
            Ok(None) => return, // clean disconnect
            Err(_) => return,
        };
        let msg = match Cbor::decode(&frame) {
            Ok(m) => m,
            Err(_) => {
                let err = Cbor::map(vec![
                    ("type", Cbor::t("error")),
                    ("err_message", Cbor::t("malformed CBOR frame")),
                ]);
                let _ = write_frame(&mut stream, &err.encode());
                continue;
            }
        };
        let reply = handle_message(&node, &msg);
        if write_frame(&mut stream, &reply.encode()).is_err() {
            return;
        }
    }
}

/// Start the TCP listener thread (127.0.0.1 only by policy).
pub fn serve_tcp(node: Arc<Node>, addr: &str) -> UcResult<JoinHandle<()>> {
    validate_tcp_listen_addr(addr)?;
    let listener = TcpListener::bind(addr).map_err(UcError::from)?;
    listener.set_nonblocking(true).map_err(UcError::from)?;
    node.logger.info(
        node.now(),
        "proto.tcp_listening",
        &[("addr", addr.to_string())],
    );
    let handle = std::thread::spawn(move || loop {
        if node.shutting_down.load(Ordering::SeqCst) {
            return;
        }
        match listener.accept() {
            Ok((stream, peer)) => {
                if !peer.ip().is_loopback() {
                    node.metrics.inc("proto.tcp_non_loopback_rejected");
                    node.logger.warn(
                        node.now(),
                        "proto.tcp_non_loopback_rejected",
                        &[("peer", peer.to_string())],
                    );
                    continue;
                }
                let _ = stream.set_nonblocking(false);
                let node2 = node.clone();
                std::thread::spawn(move || serve_stream(node2, stream));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => return,
        }
    });
    Ok(handle)
}

/// Start the Unix-socket listener thread (0600, pre-existing socket file
/// removed).
#[cfg(unix)]
pub fn serve_uds(node: Arc<Node>, path: &Path) -> UcResult<JoinHandle<()>> {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    let _ = std::fs::remove_file(path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(UcError::from)?;
    }
    let listener = UnixListener::bind(path).map_err(UcError::from)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(UcError::from)?;
    listener.set_nonblocking(true).map_err(UcError::from)?;
    node.logger.info(
        node.now(),
        "proto.uds_listening",
        &[("path", path.display().to_string())],
    );
    let handle = std::thread::spawn(move || loop {
        if node.shutting_down.load(Ordering::SeqCst) {
            return;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let node2 = node.clone();
                std::thread::spawn(move || serve_stream(node2, stream));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => return,
        }
    });
    Ok(handle)
}

// ---------------------------------------------------------------------------
// Client (blocking; used by admin subcommands and the self-test)
// ---------------------------------------------------------------------------

enum ClientStream {
    Tcp(TcpStream),
    #[cfg(unix)]
    Uds(std::os::unix::net::UnixStream),
}

impl ClientStream {
    fn io(&mut self) -> &mut dyn ReadWrite {
        match self {
            ClientStream::Tcp(s) => s,
            #[cfg(unix)]
            ClientStream::Uds(s) => s,
        }
    }
}

trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

pub struct Client {
    stream: ClientStream,
}

impl Client {
    pub fn connect_tcp(addr: &str) -> UcResult<Client> {
        Ok(Client {
            stream: ClientStream::Tcp(TcpStream::connect(addr).map_err(UcError::from)?),
        })
    }

    #[cfg(unix)]
    pub fn connect_uds(path: &Path) -> UcResult<Client> {
        Ok(Client {
            stream: ClientStream::Uds(
                std::os::unix::net::UnixStream::connect(path).map_err(UcError::from)?,
            ),
        })
    }

    /// Try UDS first, then TCP — the standard admin connect order.
    pub fn connect(uds: Option<&Path>, tcp: Option<&str>) -> UcResult<Client> {
        #[cfg(unix)]
        if let Some(p) = uds {
            if p.exists() {
                if let Ok(c) = Client::connect_uds(p) {
                    return Ok(c);
                }
            }
        }
        #[cfg(not(unix))]
        let _ = uds;
        if let Some(addr) = tcp {
            return Client::connect_tcp(addr);
        }
        Err(UcError::internal(
            "no reachable listener (is the node running?)",
        ))
    }

    fn roundtrip(&mut self, msg: &Cbor) -> UcResult<Cbor> {
        let io = self.stream.io();
        write_frame(io, &msg.encode()).map_err(UcError::from)?;
        let frame = read_frame(io)
            .map_err(UcError::from)?
            .ok_or_else(|| UcError::internal("connection closed mid-request"))?;
        Cbor::decode(&frame)
    }

    pub fn hello(&mut self, agent_id: &str) -> UcResult<Cbor> {
        self.roundtrip(&Cbor::map(vec![
            ("type", Cbor::t("hello")),
            ("proto_version", Cbor::U64(PROTO_VERSION)),
            ("agent_id", Cbor::t(agent_id)),
        ]))
    }

    pub fn request(&mut self, env: &Envelope) -> UcResult<ResponseEnvelope> {
        let reply = self.roundtrip(&Cbor::map(vec![
            ("type", Cbor::t("request")),
            ("envelope", env.to_cbor()),
        ]))?;
        let body = reply
            .get("response")
            .ok_or_else(|| UcError::internal("malformed response frame"))?;
        ResponseEnvelope::from_cbor(body)
    }

    pub fn admin(&mut self, verb: &str, args: Cbor) -> UcResult<Cbor> {
        let reply = self.roundtrip(&Cbor::map(vec![
            ("type", Cbor::t("admin")),
            ("verb", Cbor::t(verb)),
            ("args", args),
        ]))?;
        if reply.opt_bool("ok").unwrap_or(false) {
            Ok(reply.get("result").cloned().unwrap_or(Cbor::Null))
        } else {
            Err(UcError::internal(
                reply
                    .opt_str("err_message")
                    .unwrap_or_else(|| "admin verb failed".into()),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip_and_limits() {
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, b"hello").unwrap();
        write_frame(&mut buf, b"").unwrap();
        let mut r = std::io::Cursor::new(buf);
        assert_eq!(read_frame(&mut r).unwrap().unwrap(), b"hello");
        assert_eq!(read_frame(&mut r).unwrap().unwrap(), b"");
        assert!(read_frame(&mut r).unwrap().is_none()); // clean EOF
                                                        // Oversize write rejected.
        let mut sink: Vec<u8> = Vec::new();
        let big = vec![0u8; MAX_FRAME + 1];
        assert!(write_frame(&mut sink, &big).is_err());
        // Oversize length header rejected on read.
        let mut evil = ((MAX_FRAME + 1) as u32).to_le_bytes().to_vec();
        evil.extend_from_slice(&[0u8; 8]);
        assert!(read_frame(&mut std::io::Cursor::new(evil)).is_err());
    }

    #[test]
    fn truncated_frame_is_an_error_not_a_hang() {
        // Length says 100, only 3 bytes present.
        let mut buf = 100u32.to_le_bytes().to_vec();
        buf.extend_from_slice(b"abc");
        assert!(read_frame(&mut std::io::Cursor::new(buf)).is_err());
    }

    #[test]
    fn tcp_listener_policy_is_loopback_only() {
        assert!(validate_tcp_listen_addr("127.0.0.1:7741").is_ok());
        assert!(validate_tcp_listen_addr("[::1]:7741").is_ok());
        assert!(validate_tcp_listen_addr("0.0.0.0:7741").is_err());
        assert!(validate_tcp_listen_addr("192.168.1.25:7741").is_err());
        assert!(validate_tcp_listen_addr("[::]:7741").is_err());
    }
}
