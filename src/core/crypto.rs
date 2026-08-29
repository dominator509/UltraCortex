//! Cryptographic and integrity primitives — pure std, no dependencies.
//!
//! - SHA-256 (FIPS 180-4) — CAS keys, weight pinning, audit hash chain.
//!   SPEC-DERIVED-§5 (PersistenceLayer.md), SPEC-DERIVED-§A.1 (PersistenceLayer.md).
//! - HMAC-SHA256 (RFC 2104/4231) — capability-token signing (v0 stand-in for
//!   Ed25519 behind the `Signer` seam; see IMPLEMENTATION_STATUS.md §4),
//!   KMS wrap tags, CrossCheckLedger batch signing at T2+.
//! - CRC32C (Castagnoli) — WAL frame checksums. SPEC-DERIVED-§3.2 (PersistenceLayer.md).
//! - ChaCha20 (RFC 8439) — envelope encryption for tiers T1/T2.
//!
//! Every primitive carries its standard test vector; `cargo test` proves them.

// ---------------------------------------------------------------------------
// hex
// ---------------------------------------------------------------------------

pub fn to_hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(H[(b >> 4) as usize] as char);
        s.push(H[(b & 0xF) as usize] as char);
    }
    s
}

pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if b.len() % 2 != 0 {
        return None;
    }
    let nib = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity(b.len() / 2);
    for ch in b.chunks(2) {
        out.push((nib(ch[0])? << 4) | nib(ch[1])?);
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// SHA-256
// ---------------------------------------------------------------------------

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

#[derive(Clone)]
pub struct Sha256 {
    h: [u32; 8],
    buf: [u8; 64],
    buf_len: usize,
    total_len: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    pub fn new() -> Self {
        Sha256 {
            h: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buf: [0u8; 64],
            buf_len: 0,
            total_len: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total_len = self.total_len.wrapping_add(data.len() as u64);
        if self.buf_len > 0 {
            let need = 64 - self.buf_len;
            let take = need.min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == 64 {
                let block = self.buf;
                self.compress(&block);
                self.buf_len = 0;
            }
        }
        while data.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&data[..64]);
            self.compress(&block);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buf_len = data.len();
        }
    }

    pub fn finalize(mut self) -> [u8; 32] {
        let bit_len = self.total_len.wrapping_mul(8);
        // padding
        self.update(&[0x80]);
        while self.buf_len != 56 {
            self.update(&[0x00]);
        }
        // length (big-endian) — bypass update's total_len accounting by
        // compressing directly.
        self.buf[56..64].copy_from_slice(&bit_len.to_be_bytes());
        let block = self.buf;
        self.compress(&block);
        let mut out = [0u8; 32];
        for (i, w) in self.h.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&w.to_be_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        self.h[0] = self.h[0].wrapping_add(a);
        self.h[1] = self.h[1].wrapping_add(b);
        self.h[2] = self.h[2].wrapping_add(c);
        self.h[3] = self.h[3].wrapping_add(d);
        self.h[4] = self.h[4].wrapping_add(e);
        self.h[5] = self.h[5].wrapping_add(f);
        self.h[6] = self.h[6].wrapping_add(g);
        self.h[7] = self.h[7].wrapping_add(h);
    }
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize()
}

pub fn sha256_hex(data: &[u8]) -> String {
    to_hex(&sha256(data))
}

/// Lowercase hex of arbitrary bytes — the canonical rendering for hashes
/// in handles, CAS paths, and the audit log. Alias kept alongside
/// [`to_hex`] because call sites read better as `hex(&sha256(x))`.
pub fn hex(bytes: &[u8]) -> String {
    to_hex(bytes)
}

/// Streaming SHA-256 of a file (weight pinning; PersistenceLayer.md §A.1 W-1).
pub fn sha256_file(path: &std::path::Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(to_hex(&h.finalize()))
}

// ---------------------------------------------------------------------------
// HMAC-SHA256
// ---------------------------------------------------------------------------

pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        let kh = sha256(key);
        k[..32].copy_from_slice(&kh);
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for i in 0..64 {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Sha256::new();
    inner.update(&ipad);
    inner.update(msg);
    let ih = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(&opad);
    outer.update(&ih);
    outer.finalize()
}

/// Constant-time equality for MAC verification.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut d = 0u8;
    for i in 0..a.len() {
        d |= a[i] ^ b[i];
    }
    d == 0
}

// ---------------------------------------------------------------------------
// CRC32C (Castagnoli, reflected, poly 0x82F63B78)
// ---------------------------------------------------------------------------

fn crc32c_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut c = i as u32;
        let mut j = 0;
        while j < 8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ 0x82F6_3B78
            } else {
                c >> 1
            };
            j += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

pub fn crc32c(data: &[u8]) -> u32 {
    // Table computed once; std::sync::OnceLock keeps it dependency-free.
    use std::sync::OnceLock;
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    let table = TABLE.get_or_init(crc32c_table);
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = (crc >> 8) ^ table[((crc ^ b as u32) & 0xFF) as usize];
    }
    crc ^ 0xFFFF_FFFF
}

// ---------------------------------------------------------------------------
// ChaCha20 (RFC 8439)
// ---------------------------------------------------------------------------

#[inline]
fn quarter_round(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    s[a] = s[a].wrapping_add(s[b]);
    s[d] ^= s[a];
    s[d] = s[d].rotate_left(16);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] ^= s[c];
    s[b] = s[b].rotate_left(12);
    s[a] = s[a].wrapping_add(s[b]);
    s[d] ^= s[a];
    s[d] = s[d].rotate_left(8);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] ^= s[c];
    s[b] = s[b].rotate_left(7);
}

fn chacha20_block(key: &[u8; 32], counter: u32, nonce: &[u8; 12]) -> [u8; 64] {
    let mut state = [0u32; 16];
    state[0] = 0x61707865;
    state[1] = 0x3320646e;
    state[2] = 0x79622d32;
    state[3] = 0x6b206574;
    for i in 0..8 {
        state[4 + i] =
            u32::from_le_bytes([key[i * 4], key[i * 4 + 1], key[i * 4 + 2], key[i * 4 + 3]]);
    }
    state[12] = counter;
    for i in 0..3 {
        state[13 + i] = u32::from_le_bytes([
            nonce[i * 4],
            nonce[i * 4 + 1],
            nonce[i * 4 + 2],
            nonce[i * 4 + 3],
        ]);
    }
    let mut w = state;
    for _ in 0..10 {
        quarter_round(&mut w, 0, 4, 8, 12);
        quarter_round(&mut w, 1, 5, 9, 13);
        quarter_round(&mut w, 2, 6, 10, 14);
        quarter_round(&mut w, 3, 7, 11, 15);
        quarter_round(&mut w, 0, 5, 10, 15);
        quarter_round(&mut w, 1, 6, 11, 12);
        quarter_round(&mut w, 2, 7, 8, 13);
        quarter_round(&mut w, 3, 4, 9, 14);
    }
    let mut out = [0u8; 64];
    for i in 0..16 {
        let v = w[i].wrapping_add(state[i]);
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    out
}

/// XOR `data` with the ChaCha20 keystream. Symmetric: call twice to decrypt.
pub fn chacha20_xor(key: &[u8; 32], nonce: &[u8; 12], initial_counter: u32, data: &mut [u8]) {
    let mut counter = initial_counter;
    let mut off = 0usize;
    while off < data.len() {
        let ks = chacha20_block(key, counter, nonce);
        let n = (data.len() - off).min(64);
        for i in 0..n {
            data[off + i] ^= ks[i];
        }
        off += n;
        counter = counter.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_vectors() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // Multi-block: 1,000,000 'a' — well-known vector.
        let mut h = Sha256::new();
        let chunk = [b'a'; 1000];
        for _ in 0..1000 {
            h.update(&chunk);
        }
        assert_eq!(
            to_hex(&h.finalize()),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn hmac_rfc4231_case2() {
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            to_hex(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn crc32c_check_value() {
        // Standard CRC-32C check: "123456789" -> 0xE3069283.
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(b""), 0);
    }

    #[test]
    fn chacha20_rfc8439_quarter_round() {
        // RFC 8439 §2.1.1
        let mut s = [0u32; 16];
        s[0] = 0x11111111;
        s[1] = 0x01020304;
        s[2] = 0x9b8d6f43;
        s[3] = 0x01234567;
        quarter_round(&mut s, 0, 1, 2, 3);
        assert_eq!(s[0], 0xea2a92f4);
        assert_eq!(s[1], 0xcb1cf8ce);
        assert_eq!(s[2], 0x4581472e);
        assert_eq!(s[3], 0x5881c4bb);
    }

    #[test]
    fn chacha20_rfc8439_block() {
        // RFC 8439 §2.3.2
        let mut key = [0u8; 32];
        for (i, k) in key.iter_mut().enumerate() {
            *k = i as u8;
        }
        let nonce: [u8; 12] = [
            0x00, 0x00, 0x00, 0x09, 0x00, 0x00, 0x00, 0x4a, 0x00, 0x00, 0x00, 0x00,
        ];
        let block = chacha20_block(&key, 1, &nonce);
        let expected = from_hex(concat!(
            "10f1e7e4d13b5915500fdd1fa32071c4",
            "c7d1f4c733c068030422aa9ac3d46c4e",
            "d2826446079faa0914c2d705d98b02a2",
            "b5129cd1de164eb9cbd083e8a2503c4e"
        ))
        .unwrap();
        assert_eq!(&block[..], &expected[..]);
    }

    #[test]
    fn chacha20_roundtrip() {
        let key = [7u8; 32];
        let nonce = [3u8; 12];
        let mut data = b"UltraCortex substrate payload".to_vec();
        let orig = data.clone();
        chacha20_xor(&key, &nonce, 1, &mut data);
        assert_ne!(data, orig);
        chacha20_xor(&key, &nonce, 1, &mut data);
        assert_eq!(data, orig);
    }

    #[test]
    fn hex_roundtrip() {
        let b = vec![0u8, 1, 0xab, 0xff];
        assert_eq!(from_hex(&to_hex(&b)).unwrap(), b);
        assert!(from_hex("xyz").is_none());
    }
}
