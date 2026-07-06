//! ULID + deterministic RNG.
//!
//! SPEC-DERIVED-§14 (RouterScheduler.md): no thread-local PRNG — all
//! randomness derives from `envelope.seed`. ULIDs here are generated from
//! (logical_at, DetRng) so identical WAL replay yields identical IDs.

use std::fmt;

/// Crockford base32 alphabet (no I, L, O, U).
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ulid(pub u128);

impl Ulid {
    /// Compose from a 48-bit time component (logical clock in this system)
    /// and an 80-bit entropy component drawn from a deterministic RNG.
    pub fn from_parts(time48: u64, rng: &mut DetRng) -> Ulid {
        let t = (time48 & 0xFFFF_FFFF_FFFF) as u128;
        let hi = rng.next_u64() as u128;
        let lo = (rng.next_u64() & 0xFFFF) as u128;
        Ulid((t << 80) | (hi << 16) | lo)
    }

    pub fn nil() -> Ulid {
        Ulid(0)
    }

    pub fn time48(&self) -> u64 {
        ((self.0 >> 80) & 0xFFFF_FFFF_FFFF) as u64
    }

    pub fn to_base32(&self) -> String {
        let mut out = [0u8; 26];
        for (i, slot) in out.iter_mut().enumerate() {
            let shift = 5 * (25 - i);
            let idx = ((self.0 >> shift) & 0x1F) as usize;
            *slot = ALPHABET[idx];
        }
        // Safe: ALPHABET is ASCII.
        String::from_utf8(out.to_vec()).unwrap()
    }

    pub fn from_base32(s: &str) -> Option<Ulid> {
        let b = s.as_bytes();
        if b.len() != 26 {
            return None;
        }
        let mut v: u128 = 0;
        for &c in b {
            let d = decode_char(c)?;
            v = (v << 5) | d as u128;
        }
        Some(Ulid(v))
    }

    pub fn to_bytes(&self) -> [u8; 16] {
        self.0.to_be_bytes()
    }
    pub fn from_bytes(b: [u8; 16]) -> Ulid {
        Ulid(u128::from_be_bytes(b))
    }
}

fn decode_char(c: u8) -> Option<u8> {
    let c = c.to_ascii_uppercase();
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'A' => Some(10),
        b'B' => Some(11),
        b'C' => Some(12),
        b'D' => Some(13),
        b'E' => Some(14),
        b'F' => Some(15),
        b'G' => Some(16),
        b'H' => Some(17),
        b'J' => Some(18),
        b'K' => Some(19),
        b'M' => Some(20),
        b'N' => Some(21),
        b'P' => Some(22),
        b'Q' => Some(23),
        b'R' => Some(24),
        b'S' => Some(25),
        b'T' => Some(26),
        b'V' => Some(27),
        b'W' => Some(28),
        b'X' => Some(29),
        b'Y' => Some(30),
        b'Z' => Some(31),
        _ => None,
    }
}

impl fmt::Display for Ulid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_base32())
    }
}
impl fmt::Debug for Ulid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Ulid({})", self.to_base32())
    }
}

/// SplitMix64 — deterministic, seedable, well-distributed. Used everywhere a
/// "random" quantity is needed; always seeded from `envelope.seed`, node
/// seed, or the logical clock (probes / blind re-audits).
#[derive(Clone, Debug)]
pub struct DetRng {
    state: u64,
}

impl DetRng {
    pub fn new(seed: u64) -> Self {
        DetRng {
            state: seed ^ 0x9E3779B97F4A7C15,
        }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }

    /// Uniform in [0, n). n must be > 0.
    pub fn next_range(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }

    /// Uniform in [0, 1).
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ulid_roundtrip() {
        let mut rng = DetRng::new(42);
        let u = Ulid::from_parts(123456, &mut rng);
        let s = u.to_base32();
        assert_eq!(s.len(), 26);
        assert_eq!(Ulid::from_base32(&s), Some(u));
        assert_eq!(u.time48(), 123456);
    }

    #[test]
    fn ulid_deterministic() {
        let mut a = DetRng::new(7);
        let mut b = DetRng::new(7);
        assert_eq!(
            Ulid::from_parts(1, &mut a).to_base32(),
            Ulid::from_parts(1, &mut b).to_base32()
        );
    }

    #[test]
    fn ulid_sorts_by_time() {
        let mut rng = DetRng::new(1);
        let a = Ulid::from_parts(10, &mut rng);
        let b = Ulid::from_parts(11, &mut rng);
        assert!(a < b);
    }

    #[test]
    fn detrng_stable_sequence() {
        let mut r1 = DetRng::new(0xDEADBEEF);
        let mut r2 = DetRng::new(0xDEADBEEF);
        for _ in 0..16 {
            assert_eq!(r1.next_u64(), r2.next_u64());
        }
        let f = DetRng::new(9).next_f64();
        assert!((0.0..1.0).contains(&f));
    }
}
