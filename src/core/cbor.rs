//! Canonical CBOR — RFC 8949 core deterministic encoding.
//!
//! SPEC-DERIVED-§0 (McpProtocol.md): payloads are canonical CBOR — keys
//! lex-sorted (by encoded bytes), no indefinite-length items, no duplicate
//! keys. This module is the single serialization used by the WAL, the
//! Manifest, the MCP wire, the PrefixCacheStore, and the audit chain, which
//! is what makes byte-determinism (Architecture.md P-determinism, §9.5
//! PersistenceLayer.md) achievable end to end.
//!
//! Deviations, documented: floats are always encoded as 64-bit (major 7,
//! ai 27) rather than shortest-form. Both encoder and decoder live in this
//! binary, so determinism is preserved; interop with shortest-form encoders
//! is a Phase-2 item (tracked in IMPLEMENTATION_STATUS.md §5).

use super::{UcError, UcResult};

#[derive(Clone, Debug, PartialEq)]
pub enum Cbor {
    Null,
    Bool(bool),
    U64(u64),
    /// Negative integers only (value < 0). Non-negative values must use U64.
    I64(i64),
    F64(f64),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<Cbor>),
    /// Pairs; canonical ordering applied at encode time.
    Map(Vec<(Cbor, Cbor)>),
}

impl Cbor {
    // -- constructors ------------------------------------------------------

    pub fn u(v: u64) -> Cbor {
        Cbor::U64(v)
    }
    pub fn i(v: i64) -> Cbor {
        if v >= 0 {
            Cbor::U64(v as u64)
        } else {
            Cbor::I64(v)
        }
    }
    pub fn f(v: f64) -> Cbor {
        Cbor::F64(v)
    }
    pub fn t(v: impl Into<String>) -> Cbor {
        Cbor::Text(v.into())
    }
    pub fn by(v: Vec<u8>) -> Cbor {
        Cbor::Bytes(v)
    }
    pub fn arr(v: Vec<Cbor>) -> Cbor {
        Cbor::Array(v)
    }
    pub fn map(pairs: Vec<(&str, Cbor)>) -> Cbor {
        Cbor::Map(
            pairs
                .into_iter()
                .map(|(k, v)| (Cbor::Text(k.to_string()), v))
                .collect(),
        )
    }
    pub fn text_array(items: &[String]) -> Cbor {
        Cbor::Array(items.iter().map(|s| Cbor::Text(s.clone())).collect())
    }

    // -- accessors ---------------------------------------------------------

    pub fn get(&self, key: &str) -> Option<&Cbor> {
        if let Cbor::Map(pairs) = self {
            for (k, v) in pairs {
                if let Cbor::Text(t) = k {
                    if t == key {
                        return Some(v);
                    }
                }
            }
        }
        None
    }
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Cbor::U64(v) => Some(*v),
            _ => None,
        }
    }
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Cbor::U64(v) => i64::try_from(*v).ok(),
            Cbor::I64(v) => Some(*v),
            _ => None,
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Cbor::F64(v) => Some(*v),
            Cbor::U64(v) => Some(*v as f64),
            Cbor::I64(v) => Some(*v as f64),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Cbor::Text(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Cbor::Bytes(b) => Some(b),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Cbor::Bool(b) => Some(*b),
            _ => None,
        }
    }
    pub fn as_array(&self) -> Option<&[Cbor]> {
        match self {
            Cbor::Array(a) => Some(a),
            _ => None,
        }
    }
    pub fn as_map(&self) -> Option<&[(Cbor, Cbor)]> {
        match self {
            Cbor::Map(m) => Some(m),
            _ => None,
        }
    }

    /// Field helpers that produce structured errors.
    pub fn req_str(&self, key: &str) -> UcResult<String> {
        self.get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| UcError::internal(format!("missing/invalid text field `{key}`")))
    }
    pub fn req_u64(&self, key: &str) -> UcResult<u64> {
        self.get(key)
            .and_then(|v| v.as_u64())
            .ok_or_else(|| UcError::internal(format!("missing/invalid uint field `{key}`")))
    }
    pub fn opt_str(&self, key: &str) -> Option<String> {
        self.get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }
    pub fn opt_u64(&self, key: &str) -> Option<u64> {
        self.get(key).and_then(|v| v.as_u64())
    }
    pub fn opt_bool(&self, key: &str) -> Option<bool> {
        self.get(key).and_then(|v| v.as_bool())
    }

    /// Canonical encoding (deterministic).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        encode_into(self, &mut out);
        out
    }

    pub fn decode(bytes: &[u8]) -> UcResult<Cbor> {
        let mut cur = Cursor { b: bytes, pos: 0 };
        let v = decode_one(&mut cur)?;
        if cur.pos != bytes.len() {
            return Err(UcError::internal("cbor: trailing bytes"));
        }
        Ok(v)
    }

    pub fn decode_prefix(bytes: &[u8]) -> UcResult<(Cbor, usize)> {
        let mut cur = Cursor { b: bytes, pos: 0 };
        let v = decode_one(&mut cur)?;
        Ok((v, cur.pos))
    }

    /// Collect every Text leaf (used by the Warden to harvest handle-like
    /// strings from arbitrary payloads).
    pub fn collect_texts<'a>(&'a self, out: &mut Vec<&'a str>) {
        match self {
            Cbor::Text(s) => out.push(s),
            Cbor::Array(a) => {
                for v in a {
                    v.collect_texts(out);
                }
            }
            Cbor::Map(m) => {
                for (k, v) in m {
                    k.collect_texts(out);
                    v.collect_texts(out);
                }
            }
            _ => {}
        }
    }
}

fn head(major: u8, value: u64, out: &mut Vec<u8>) {
    let m = major << 5;
    if value < 24 {
        out.push(m | value as u8);
    } else if value <= 0xFF {
        out.push(m | 24);
        out.push(value as u8);
    } else if value <= 0xFFFF {
        out.push(m | 25);
        out.extend_from_slice(&(value as u16).to_be_bytes());
    } else if value <= 0xFFFF_FFFF {
        out.push(m | 26);
        out.extend_from_slice(&(value as u32).to_be_bytes());
    } else {
        out.push(m | 27);
        out.extend_from_slice(&value.to_be_bytes());
    }
}

fn encode_into(v: &Cbor, out: &mut Vec<u8>) {
    match v {
        Cbor::U64(n) => head(0, *n, out),
        Cbor::I64(n) => {
            debug_assert!(*n < 0, "I64 must be negative; use u() for non-negative");
            let m = (-1i128 - *n as i128) as u64;
            head(1, m, out);
        }
        Cbor::Bytes(b) => {
            head(2, b.len() as u64, out);
            out.extend_from_slice(b);
        }
        Cbor::Text(s) => {
            head(3, s.len() as u64, out);
            out.extend_from_slice(s.as_bytes());
        }
        Cbor::Array(a) => {
            head(4, a.len() as u64, out);
            for item in a {
                encode_into(item, out);
            }
        }
        Cbor::Map(pairs) => {
            // Canonical: sort by encoded key bytes; reject duplicates.
            let mut enc: Vec<(Vec<u8>, Vec<u8>)> = pairs
                .iter()
                .map(|(k, val)| {
                    let mut kb = Vec::new();
                    encode_into(k, &mut kb);
                    let mut vb = Vec::new();
                    encode_into(val, &mut vb);
                    (kb, vb)
                })
                .collect();
            enc.sort_by(|a, b| a.0.cmp(&b.0));
            enc.dedup_by(|a, b| a.0 == b.0);
            head(5, enc.len() as u64, out);
            for (kb, vb) in enc {
                out.extend_from_slice(&kb);
                out.extend_from_slice(&vb);
            }
        }
        Cbor::Bool(false) => out.push(0xF4),
        Cbor::Bool(true) => out.push(0xF5),
        Cbor::Null => out.push(0xF6),
        Cbor::F64(f) => {
            out.push(0xFB);
            out.extend_from_slice(&f.to_bits().to_be_bytes());
        }
    }
}

struct Cursor<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> UcResult<&'a [u8]> {
        if self.pos + n > self.b.len() {
            return Err(UcError::internal("cbor: truncated"));
        }
        let s = &self.b[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn byte(&mut self) -> UcResult<u8> {
        Ok(self.take(1)?[0])
    }
}

fn read_uint(cur: &mut Cursor, ai: u8) -> UcResult<u64> {
    Ok(match ai {
        0..=23 => ai as u64,
        24 => cur.byte()? as u64,
        25 => {
            let b = cur.take(2)?;
            u16::from_be_bytes([b[0], b[1]]) as u64
        }
        26 => {
            let b = cur.take(4)?;
            u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as u64
        }
        27 => {
            let b = cur.take(8)?;
            u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
        }
        _ => return Err(UcError::internal("cbor: indefinite-length forbidden")),
    })
}

fn f16_to_f64(h: u16) -> f64 {
    let sign = ((h >> 15) & 1) as u64;
    let exp = ((h >> 10) & 0x1F) as i32;
    let frac = (h & 0x3FF) as u64;
    let val = if exp == 0 {
        // subnormal
        (frac as f64) * 2f64.powi(-24)
    } else if exp == 31 {
        if frac == 0 {
            f64::INFINITY
        } else {
            f64::NAN
        }
    } else {
        (1.0 + frac as f64 / 1024.0) * 2f64.powi(exp - 15)
    };
    if sign == 1 {
        -val
    } else {
        val
    }
}

fn decode_one(cur: &mut Cursor) -> UcResult<Cbor> {
    let ib = cur.byte()?;
    let major = ib >> 5;
    let ai = ib & 0x1F;
    match major {
        0 => Ok(Cbor::U64(read_uint(cur, ai)?)),
        1 => {
            let m = read_uint(cur, ai)?;
            let v = -1i128 - m as i128;
            i64::try_from(v)
                .map(Cbor::I64)
                .map_err(|_| UcError::internal("cbor: negint out of i64 range"))
        }
        2 => {
            let n = read_uint(cur, ai)? as usize;
            Ok(Cbor::Bytes(cur.take(n)?.to_vec()))
        }
        3 => {
            let n = read_uint(cur, ai)? as usize;
            let s = std::str::from_utf8(cur.take(n)?)
                .map_err(|_| UcError::internal("cbor: invalid utf8"))?;
            Ok(Cbor::Text(s.to_string()))
        }
        4 => {
            let n = read_uint(cur, ai)? as usize;
            if n > 1_000_000 {
                return Err(UcError::internal("cbor: array too large"));
            }
            let mut a = Vec::with_capacity(n.min(4096));
            for _ in 0..n {
                a.push(decode_one(cur)?);
            }
            Ok(Cbor::Array(a))
        }
        5 => {
            let n = read_uint(cur, ai)? as usize;
            if n > 1_000_000 {
                return Err(UcError::internal("cbor: map too large"));
            }
            let mut m = Vec::with_capacity(n.min(4096));
            for _ in 0..n {
                let k = decode_one(cur)?;
                let v = decode_one(cur)?;
                m.push((k, v));
            }
            Ok(Cbor::Map(m))
        }
        6 => {
            // Tags: skip the tag value, decode the item transparently.
            let _tag = read_uint(cur, ai)?;
            decode_one(cur)
        }
        7 => match ai {
            20 => Ok(Cbor::Bool(false)),
            21 => Ok(Cbor::Bool(true)),
            22 | 23 => Ok(Cbor::Null),
            25 => {
                let b = cur.take(2)?;
                Ok(Cbor::F64(f16_to_f64(u16::from_be_bytes([b[0], b[1]]))))
            }
            26 => {
                let b = cur.take(4)?;
                Ok(Cbor::F64(
                    f32::from_bits(u32::from_be_bytes([b[0], b[1], b[2], b[3]])) as f64,
                ))
            }
            27 => {
                let b = cur.take(8)?;
                Ok(Cbor::F64(f64::from_bits(u64::from_be_bytes([
                    b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
                ]))))
            }
            _ => Err(UcError::internal("cbor: unsupported simple value")),
        },
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::crypto::to_hex;

    fn enc_hex(v: &Cbor) -> String {
        to_hex(&v.encode())
    }

    #[test]
    fn rfc8949_int_vectors() {
        assert_eq!(enc_hex(&Cbor::u(0)), "00");
        assert_eq!(enc_hex(&Cbor::u(23)), "17");
        assert_eq!(enc_hex(&Cbor::u(24)), "1818");
        assert_eq!(enc_hex(&Cbor::u(100)), "1864");
        assert_eq!(enc_hex(&Cbor::u(1000)), "1903e8");
        assert_eq!(enc_hex(&Cbor::u(1_000_000)), "1a000f4240");
        assert_eq!(enc_hex(&Cbor::i(-1)), "20");
        assert_eq!(enc_hex(&Cbor::i(-10)), "29");
        assert_eq!(enc_hex(&Cbor::i(-100)), "3863");
    }

    #[test]
    fn rfc8949_misc_vectors() {
        assert_eq!(enc_hex(&Cbor::t("a")), "6161");
        assert_eq!(enc_hex(&Cbor::t("IETF")), "6449455446");
        assert_eq!(enc_hex(&Cbor::arr(vec![])), "80");
        assert_eq!(
            enc_hex(&Cbor::arr(vec![Cbor::u(1), Cbor::u(2), Cbor::u(3)])),
            "83010203"
        );
        assert_eq!(enc_hex(&Cbor::Map(vec![])), "a0");
        assert_eq!(enc_hex(&Cbor::Bool(true)), "f5");
        assert_eq!(enc_hex(&Cbor::Bool(false)), "f4");
        assert_eq!(enc_hex(&Cbor::Null), "f6");
    }

    #[test]
    fn canonical_map_key_order() {
        // Keys must be sorted by encoded bytes regardless of insertion order.
        let a = Cbor::map(vec![("b", Cbor::u(2)), ("a", Cbor::u(1))]);
        let b = Cbor::map(vec![("a", Cbor::u(1)), ("b", Cbor::u(2))]);
        assert_eq!(a.encode(), b.encode());
        // Shorter keys sort before longer with same prefix ("a" < "aa").
        let c = Cbor::map(vec![("aa", Cbor::u(2)), ("a", Cbor::u(1))]);
        let bytes = c.encode();
        let dec = Cbor::decode(&bytes).unwrap();
        let m = dec.as_map().unwrap();
        assert_eq!(m[0].0.as_str(), Some("a"));
        assert_eq!(m[1].0.as_str(), Some("aa"));
    }

    #[test]
    fn roundtrip_nested() {
        let v = Cbor::map(vec![
            (
                "handles",
                Cbor::text_array(&["fact/AAA".into(), "blob/bbb".into()]),
            ),
            ("n", Cbor::u(42)),
            ("neg", Cbor::i(-7)),
            ("f", Cbor::f(1.5)),
            ("ok", Cbor::Bool(true)),
            ("none", Cbor::Null),
            ("raw", Cbor::by(vec![1, 2, 3])),
            (
                "inner",
                Cbor::map(vec![
                    ("k", Cbor::t("v")),
                    ("arr", Cbor::arr(vec![Cbor::u(1)])),
                ]),
            ),
        ]);
        let bytes = v.encode();
        let back = Cbor::decode(&bytes).unwrap();
        // Re-encode must be byte-identical (determinism §9.5).
        assert_eq!(back.encode(), bytes);
        assert_eq!(back.req_u64("n").unwrap(), 42);
        assert_eq!(back.get("neg").unwrap().as_i64(), Some(-7));
        assert_eq!(back.get("f").unwrap().as_f64(), Some(1.5));
    }

    #[test]
    fn rejects_trailing_and_truncated() {
        let mut bytes = Cbor::u(1).encode();
        bytes.push(0x00);
        assert!(Cbor::decode(&bytes).is_err());
        assert!(Cbor::decode(&[0x1a, 0x00]).is_err());
    }

    #[test]
    fn collect_texts_walks_everything() {
        let v = Cbor::map(vec![
            ("a", Cbor::t("fact/X")),
            ("b", Cbor::arr(vec![Cbor::t("blob/Y"), Cbor::u(1)])),
        ]);
        let mut out = Vec::new();
        v.collect_texts(&mut out);
        assert!(out.contains(&"fact/X"));
        assert!(out.contains(&"blob/Y"));
    }
}
