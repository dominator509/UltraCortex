//! Write-Ahead Log — SPEC-DERIVED-§3 (PersistenceLayer.md).
//!
//! Frame format (all integers little-endian):
//! ```text
//! magic      u32   0x57414C46  ("WALF")
//! frame_len  u32   length of everything after this field
//! logical_at u64
//! cell_id    u64
//! op         u8    (WalOp)
//! schema_ver u8
//! flags      u16   (bit0 = cross_check stream marker)
//! payload    [u8]  canonical CBOR
//! crc32c     u32   over frame_len..payload (i.e. all bytes after magic
//!                  except the trailing crc itself)
//! ```
//!
//! Durability: writers enqueue frames to a flusher thread; the flusher
//! group-commits on a 250µs window or 256KiB of buffered bytes, whichever
//! first (§3.2), then fsyncs and acks `(epoch, offset)` back over a oneshot
//! channel. Epochs roll at 1GiB (`wal/<stream>/epoch-NNNNNNNN.wal`).
//! Replay stops at the first torn/corrupt frame (crash tail) and reports it.

use crate::core::crypto::crc32c;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

pub const WAL_MAGIC: u32 = 0x5741_4C46; // "WALF"
pub const EPOCH_ROLL_BYTES: u64 = 1 << 30; // 1 GiB
pub const GROUP_COMMIT_WINDOW: Duration = Duration::from_micros(250);
pub const GROUP_COMMIT_BYTES: usize = 256 * 1024;
/// Bit 0 of `flags`: frame belongs to the CrossCheckLedger stream
/// (CrossCheckLedgerCell.md §5 — own WAL stream, same format).
pub const FLAG_CROSS_CHECK: u16 = 0b1;

/// Domain-separated KMS purpose for a WAL payload. Keeping this in the WAL
/// module makes every producer and the recovery path use the same value.
pub fn payload_purpose(cell_id: u64, flags: u16) -> String {
    if flags & FLAG_CROSS_CHECK != 0 {
        "wal.cross_check".into()
    } else {
        format!("wal.cell.{cell_id}")
    }
}

/// Derive a nonce seed that is stable for replay but differs for frames that
/// share a logical timestamp. The authenticated payload still carries the
/// frame CRC, so this is only nonce-domain separation.
pub fn payload_nonce(logical_at: u64, cell_id: u64, op: WalOp, payload: &[u8]) -> u64 {
    let mut seed = Vec::with_capacity(32 + payload.len());
    seed.extend_from_slice(&logical_at.to_le_bytes());
    seed.extend_from_slice(&cell_id.to_le_bytes());
    seed.push(op as u8);
    seed.extend_from_slice(payload);
    crate::core::fnv1a64(&seed)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum WalOp {
    Write = 1,
    Supersede = 2,
    QuarantineAbsorb = 3,
    QuarantineResolve = 4,
    Decision = 5,
    CuratorOutput = 6,
    CrossCheck = 7,
    AdminOp = 8,
    Checkpoint = 9,
}

impl WalOp {
    pub fn from_u8(v: u8) -> Option<WalOp> {
        Some(match v {
            1 => WalOp::Write,
            2 => WalOp::Supersede,
            3 => WalOp::QuarantineAbsorb,
            4 => WalOp::QuarantineResolve,
            5 => WalOp::Decision,
            6 => WalOp::CuratorOutput,
            7 => WalOp::CrossCheck,
            8 => WalOp::AdminOp,
            9 => WalOp::Checkpoint,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug)]
pub struct WalFrame {
    pub logical_at: u64,
    pub cell_id: u64,
    pub op: WalOp,
    pub schema_ver: u8,
    pub flags: u16,
    pub payload: Vec<u8>,
}

impl WalFrame {
    /// Serialize including magic and trailing crc.
    pub fn to_bytes(&self) -> Vec<u8> {
        // body = everything after frame_len field, minus crc.
        let mut body = Vec::with_capacity(8 + 8 + 1 + 1 + 2 + self.payload.len());
        body.extend_from_slice(&self.logical_at.to_le_bytes());
        body.extend_from_slice(&self.cell_id.to_le_bytes());
        body.push(self.op as u8);
        body.push(self.schema_ver);
        body.extend_from_slice(&self.flags.to_le_bytes());
        body.extend_from_slice(&self.payload);

        let frame_len = (body.len() + 4) as u32; // + trailing crc
        let mut crc_input = Vec::with_capacity(4 + body.len());
        crc_input.extend_from_slice(&frame_len.to_le_bytes());
        crc_input.extend_from_slice(&body);
        let crc = crc32c(&crc_input);

        let mut out = Vec::with_capacity(8 + body.len() + 4);
        out.extend_from_slice(&WAL_MAGIC.to_le_bytes());
        out.extend_from_slice(&frame_len.to_le_bytes());
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc.to_le_bytes());
        out
    }
}

/// Position of a durably committed frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WalPos {
    pub epoch: u64,
    pub offset: u64,
}

enum FlusherMsg {
    Frame {
        bytes: Vec<u8>,
        ack: SyncSender<Result<WalPos, String>>,
    },
    Sync {
        ack: SyncSender<Result<WalPos, String>>,
    },
    Shutdown,
}

/// One WAL stream (per shard, plus a dedicated cross_check stream).
pub struct WalWriter {
    tx: SyncSender<FlusherMsg>,
    handle: Mutex<Option<JoinHandle<()>>>,
    dir: PathBuf,
}

impl WalWriter {
    pub fn open(dir: &Path) -> Result<Arc<WalWriter>, String> {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        // Find latest epoch (or start at 0).
        let (epoch, offset) = latest_epoch_state(dir)?;
        let file = open_epoch(dir, epoch)?;
        let (tx, rx) = sync_channel::<FlusherMsg>(4096);
        let dir_owned = dir.to_path_buf();
        let handle = std::thread::Builder::new()
            .name("uc-wal-flusher".into())
            .spawn(move || flusher_loop(dir_owned, epoch, offset, file, rx))
            .map_err(|e| e.to_string())?;
        Ok(Arc::new(WalWriter {
            tx,
            handle: Mutex::new(Some(handle)),
            dir: dir.to_path_buf(),
        }))
    }

    /// Append and block until the group commit containing this frame is
    /// durable. Returns the durable position.
    pub fn append(&self, frame: &WalFrame) -> Result<WalPos, String> {
        let (ack_tx, ack_rx) = sync_channel(1);
        self.tx
            .send(FlusherMsg::Frame {
                bytes: frame.to_bytes(),
                ack: ack_tx,
            })
            .map_err(|_| "wal flusher gone".to_string())?;
        ack_rx
            .recv()
            .map_err(|_| "wal flusher dropped ack".to_string())?
    }

    /// Force an fsync barrier (used by clean shutdown, B6).
    pub fn sync(&self) -> Result<WalPos, String> {
        let (ack_tx, ack_rx) = sync_channel(1);
        self.tx
            .send(FlusherMsg::Sync { ack: ack_tx })
            .map_err(|_| "wal flusher gone".to_string())?;
        ack_rx
            .recv()
            .map_err(|_| "wal flusher dropped ack".to_string())?
    }

    pub fn shutdown(&self) {
        let _ = self.tx.send(FlusherMsg::Shutdown);
        if let Some(h) = self.handle.lock().unwrap().take() {
            let _ = h.join();
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

fn epoch_path(dir: &Path, epoch: u64) -> PathBuf {
    dir.join(format!("epoch-{epoch:08}.wal"))
}

fn open_epoch(dir: &Path, epoch: u64) -> Result<File, String> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(epoch_path(dir, epoch))
        .map_err(|e| e.to_string())
}

fn latest_epoch_state(dir: &Path) -> Result<(u64, u64), String> {
    let mut max_epoch: Option<u64> = None;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(num) = name
                .strip_prefix("epoch-")
                .and_then(|s| s.strip_suffix(".wal"))
            {
                if let Ok(e) = num.parse::<u64>() {
                    max_epoch = Some(max_epoch.map_or(e, |m: u64| m.max(e)));
                }
            }
        }
    }
    let epoch = max_epoch.unwrap_or(0);
    let offset = std::fs::metadata(epoch_path(dir, epoch))
        .map(|m| m.len())
        .unwrap_or(0);
    Ok((epoch, offset))
}

fn flusher_loop(
    dir: PathBuf,
    mut epoch: u64,
    mut offset: u64,
    mut file: File,
    rx: Receiver<FlusherMsg>,
) {
    let mut buf: Vec<u8> = Vec::with_capacity(GROUP_COMMIT_BYTES);
    let mut pending_acks: Vec<(SyncSender<Result<WalPos, String>>, u64)> = Vec::new();

    'outer: loop {
        // Block for the first message.
        let first = match rx.recv() {
            Ok(m) => m,
            Err(_) => break,
        };
        let mut shutdown = false;
        let mut force_sync = false;
        handle_msg(
            first,
            &mut buf,
            &mut pending_acks,
            offset,
            &mut shutdown,
            &mut force_sync,
        );

        // Group-commit window: gather more frames for up to 250µs / 256KiB.
        if !shutdown && !force_sync {
            let deadline = std::time::Instant::now() + GROUP_COMMIT_WINDOW;
            while buf.len() < GROUP_COMMIT_BYTES {
                let now = std::time::Instant::now();
                if now >= deadline {
                    break;
                }
                match rx.recv_timeout(deadline - now) {
                    Ok(m) => {
                        handle_msg(
                            m,
                            &mut buf,
                            &mut pending_acks,
                            offset,
                            &mut shutdown,
                            &mut force_sync,
                        );
                        if shutdown || force_sync {
                            break;
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        shutdown = true;
                        break;
                    }
                }
            }
        }

        // Commit the batch: write, fsync, ack.
        let result: Result<(), String> = (|| {
            if !buf.is_empty() {
                file.write_all(&buf).map_err(|e| e.to_string())?;
                offset += buf.len() as u64;
                buf.clear();
            }
            file.sync_data().map_err(|e| e.to_string())?;
            Ok(())
        })();

        for (ack, frame_end) in pending_acks.drain(..) {
            let _ = ack.send(match &result {
                Ok(()) => Ok(WalPos {
                    epoch,
                    offset: frame_end,
                }),
                Err(e) => Err(e.clone()),
            });
        }

        // Epoch rollover (§3.4).
        if offset >= EPOCH_ROLL_BYTES {
            epoch += 1;
            offset = 0;
            match open_epoch(&dir, epoch) {
                Ok(f) => file = f,
                Err(_) => break 'outer,
            }
        }

        if shutdown {
            let _ = file.sync_data();
            break;
        }
    }
}

fn handle_msg(
    msg: FlusherMsg,
    buf: &mut Vec<u8>,
    pending: &mut Vec<(SyncSender<Result<WalPos, String>>, u64)>,
    committed_offset: u64,
    shutdown: &mut bool,
    force_sync: &mut bool,
) {
    match msg {
        FlusherMsg::Frame { bytes, ack } => {
            buf.extend_from_slice(&bytes);
            let frame_end = committed_offset + buf.len() as u64;
            pending.push((ack, frame_end));
        }
        FlusherMsg::Sync { ack } => {
            let end = committed_offset + buf.len() as u64;
            pending.push((ack, end));
            *force_sync = true;
        }
        FlusherMsg::Shutdown => *shutdown = true,
    }
}

// ---------------------------------------------------------------------------
// Replay
// ---------------------------------------------------------------------------

/// Result of replaying one stream directory: frames in order, plus a note if
/// a torn tail was truncated (crash recovery, §3.5 — torn tail is normal
/// after a crash; corruption *before* the tail is not).
pub struct ReplayOutcome {
    pub frames: Vec<WalFrame>,
    pub torn_tail: Option<String>,
}

pub fn replay_dir(dir: &Path) -> Result<ReplayOutcome, String> {
    let mut epochs: Vec<u64> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(num) = name
                .strip_prefix("epoch-")
                .and_then(|s| s.strip_suffix(".wal"))
            {
                if let Ok(e) = num.parse::<u64>() {
                    epochs.push(e);
                }
            }
        }
    }
    epochs.sort_unstable();
    let mut frames = Vec::new();
    let mut torn_tail = None;
    for (idx, epoch) in epochs.iter().enumerate() {
        let path = epoch_path(dir, *epoch);
        let outcome = replay_file(&path)?;
        frames.extend(outcome.frames);
        if let Some(t) = outcome.torn_tail {
            if idx != epochs.len() - 1 {
                // Corruption in a non-final epoch is unrecoverable data loss.
                return Err(format!(
                    "wal corruption in non-final epoch {}: {}",
                    epoch, t
                ));
            }
            torn_tail = Some(t);
        }
    }
    Ok(ReplayOutcome { frames, torn_tail })
}

pub fn replay_file(path: &Path) -> Result<ReplayOutcome, String> {
    let f = File::open(path).map_err(|e| e.to_string())?;
    let len = f.metadata().map_err(|e| e.to_string())?.len();
    let mut r = BufReader::new(f);
    let mut frames = Vec::new();
    let mut pos: u64 = 0;
    loop {
        if pos == len {
            return Ok(ReplayOutcome {
                frames,
                torn_tail: None,
            });
        }
        match read_frame(&mut r, len - pos) {
            Ok((frame, consumed)) => {
                frames.push(frame);
                pos += consumed;
            }
            Err(e) => {
                return Ok(ReplayOutcome {
                    frames,
                    torn_tail: Some(format!("{} at offset {}", e, pos)),
                });
            }
        }
    }
}

fn read_frame<R: Read + Seek>(r: &mut R, remaining: u64) -> Result<(WalFrame, u64), String> {
    if remaining < 8 {
        return Err("short header".into());
    }
    let mut hdr = [0u8; 8];
    r.read_exact(&mut hdr).map_err(|e| e.to_string())?;
    let magic = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
    if magic != WAL_MAGIC {
        // Rewind so a torn tail is measured at the frame start.
        let _ = r.seek(SeekFrom::Current(-8));
        return Err("bad magic".into());
    }
    let frame_len = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as u64;
    // Sanity: fixed fields (20) + crc (4) minimum; 64 MiB max payload guard.
    if !(24..=64 * 1024 * 1024 + 24).contains(&frame_len) {
        return Err("implausible frame_len".into());
    }
    if remaining < 8 + frame_len {
        return Err("torn frame".into());
    }
    let mut body = vec![0u8; frame_len as usize];
    r.read_exact(&mut body).map_err(|e| e.to_string())?;
    let crc_stored = u32::from_le_bytes([
        body[frame_len as usize - 4],
        body[frame_len as usize - 3],
        body[frame_len as usize - 2],
        body[frame_len as usize - 1],
    ]);
    let mut crc_input = Vec::with_capacity(4 + body.len() - 4);
    crc_input.extend_from_slice(&(frame_len as u32).to_le_bytes());
    crc_input.extend_from_slice(&body[..frame_len as usize - 4]);
    if crc32c(&crc_input) != crc_stored {
        return Err("crc mismatch".into());
    }
    let logical_at = u64::from_le_bytes(body[0..8].try_into().unwrap());
    let cell_id = u64::from_le_bytes(body[8..16].try_into().unwrap());
    let op = WalOp::from_u8(body[16]).ok_or("unknown op")?;
    let schema_ver = body[17];
    let flags = u16::from_le_bytes([body[18], body[19]]);
    let payload = body[20..frame_len as usize - 4].to_vec();
    Ok((
        WalFrame {
            logical_at,
            cell_id,
            op,
            schema_ver,
            flags,
            payload,
        },
        8 + frame_len,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("uc-wal-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn frame_roundtrip_bytes() {
        let f = WalFrame {
            logical_at: 42,
            cell_id: 7,
            op: WalOp::Write,
            schema_ver: 1,
            flags: 0,
            payload: vec![0xA0], // empty CBOR map
        };
        let bytes = f.to_bytes();
        assert_eq!(&bytes[0..4], &WAL_MAGIC.to_le_bytes());
        // frame_len = 8+8+1+1+2 + 1 payload + 4 crc = 25
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 25);
    }

    #[test]
    fn write_replay_roundtrip() {
        let dir = tmpdir("rt");
        {
            let w = WalWriter::open(&dir).unwrap();
            for i in 0..10u64 {
                let pos = w
                    .append(&WalFrame {
                        logical_at: i,
                        cell_id: i % 3,
                        op: WalOp::Write,
                        schema_ver: 1,
                        flags: if i % 2 == 0 { 0 } else { FLAG_CROSS_CHECK },
                        payload: vec![0x41, i as u8], // bytes(1)
                    })
                    .unwrap();
                assert_eq!(pos.epoch, 0);
            }
            w.shutdown();
        }
        let out = replay_dir(&dir).unwrap();
        assert!(out.torn_tail.is_none());
        assert_eq!(out.frames.len(), 10);
        assert_eq!(out.frames[3].logical_at, 3);
        assert_eq!(out.frames[5].flags, FLAG_CROSS_CHECK);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn torn_tail_is_truncated_not_fatal() {
        let dir = tmpdir("torn");
        {
            let w = WalWriter::open(&dir).unwrap();
            for i in 0..3u64 {
                w.append(&WalFrame {
                    logical_at: i,
                    cell_id: 1,
                    op: WalOp::Write,
                    schema_ver: 1,
                    flags: 0,
                    payload: vec![0xF6], // null
                })
                .unwrap();
            }
            w.shutdown();
        }
        // Append garbage half-frame to simulate a crash mid-write.
        let path = epoch_path(&dir, 0);
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(&WAL_MAGIC.to_le_bytes()).unwrap();
        f.write_all(&100u32.to_le_bytes()).unwrap();
        f.write_all(&[0u8; 10]).unwrap(); // far short of 100
        drop(f);
        let out = replay_dir(&dir).unwrap();
        assert_eq!(out.frames.len(), 3);
        assert!(out.torn_tail.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_middle_frame_detected() {
        let dir = tmpdir("corrupt");
        {
            let w = WalWriter::open(&dir).unwrap();
            for i in 0..3u64 {
                w.append(&WalFrame {
                    logical_at: i,
                    cell_id: 1,
                    op: WalOp::Write,
                    schema_ver: 1,
                    flags: 0,
                    payload: vec![0xF6],
                })
                .unwrap();
            }
            w.shutdown();
        }
        let path = epoch_path(&dir, 0);
        let mut data = std::fs::read(&path).unwrap();
        // Flip a crc-covered byte inside frame 1. Each frame here is
        // 8 (magic+len) + 25 (body incl. crc) = 33 bytes, so frame 1 spans
        // [33, 66); offset 60 lands in its flags field, which the crc covers.
        data[60] ^= 0xFF;
        std::fs::write(&path, &data).unwrap();
        let out = replay_file(&path).unwrap();
        assert_eq!(out.frames.len(), 1); // only frame 0 survives
        assert!(out.torn_tail.unwrap().contains("crc mismatch"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
