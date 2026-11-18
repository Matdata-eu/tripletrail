//! A hasher for long strings that reads a bounded sample of their bytes.
//!
//! Geometry literals run to hundreds of kilobytes (administrative boundaries, land use), and
//! the parse cache and the spatial rewrite's literal sets look them up once per candidate.
//! SipHash over the whole text costs more than the test the lookup serves. [`SampledHasher`]
//! hashes the length, the first and last 64 bytes and 64 bytes spread over the rest; equal
//! texts still hash equal, and the table's full equality check (a `memcmp`) settles collisions
//! between texts that differ only outside the sample.

use std::hash::{BuildHasherDefault, Hasher};

/// See the module documentation.
#[derive(Default, Clone, Copy)]
pub struct SampledHasher(u64);

/// A `BuildHasher` for `HashMap`s and `HashSet`s keyed by long strings or literals.
pub type FastBuildHasher = BuildHasherDefault<SampledHasher>;

const K: u64 = 0x9E37_79B9_7F4A_7C15;

impl SampledHasher {
    #[inline]
    fn mix(&mut self, v: u64) {
        self.0 = (self.0.rotate_left(5) ^ v).wrapping_mul(K);
    }

    fn chunk(&mut self, bytes: &[u8]) {
        let (chunks, rest) = bytes.as_chunks::<8>();
        for c in chunks {
            self.mix(u64::from_le_bytes(*c));
        }
        if !rest.is_empty() {
            let mut buf = [0u8; 8];
            buf[..rest.len()].copy_from_slice(rest);
            self.mix(u64::from_le_bytes(buf));
        }
    }
}

impl Hasher for SampledHasher {
    fn write(&mut self, bytes: &[u8]) {
        let n = bytes.len();
        self.mix(n as u64);
        if n <= 192 {
            self.chunk(bytes);
            return;
        }
        self.chunk(&bytes[..64]);
        self.chunk(&bytes[n - 64..]);
        // 8 samples of 8 bytes spread over the middle.
        let step = (n - 128) / 8;
        for i in 0..8 {
            let at = 64 + i * step;
            self.chunk(&bytes[at..at + 8]);
        }
    }

    fn write_u8(&mut self, i: u8) {
        self.mix(u64::from(i));
    }

    fn write_u64(&mut self, i: u64) {
        self.mix(i);
    }

    fn write_usize(&mut self, i: usize) {
        self.mix(i as u64);
    }

    fn finish(&self) -> u64 {
        // Final avalanche (from SplitMix64) so the low bits the table uses depend on all input.
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn long_and_short_strings() {
        let long: Vec<String> = (0..1000)
            .map(|i| format!("POLYGON(({}))", "1 2, ".repeat(100) + &i.to_string()))
            .collect();
        let mut set: HashSet<&str, FastBuildHasher> = HashSet::default();
        for s in &long {
            set.insert(s);
        }
        assert_eq!(set.len(), 1000);
        assert!(long.iter().all(|s| set.contains(s.as_str())));
        assert!(!set.contains("POINT(1 2)"));
    }
}
