//! A small seeded PRNG (SplitMix64).
//!
//! Hand-rolled on purpose: the fixtures must stay byte-identical across
//! machines and over time, so their generator cannot change under a
//! dependency bump.

/// SplitMix64: fast, statistically fine for fixture data, fully determined
/// by its seed.
pub struct Rng(u64);

impl Rng {
    /// A generator for one named stream of `seed`, so each fixture has its
    /// own sequence and generating one alone gives the same result.
    pub fn new(seed: u64, stream: &str) -> Self {
        // FNV-1a of the stream name, mixed into the seed.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in stream.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        let mut rng = Self(seed ^ hash);
        rng.next_u64();
        rng
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A uniform value in `0..n` (`0` when `n` is `0`).
    pub fn below(&mut self, n: usize) -> usize {
        ((u128::from(self.next_u64()) * n as u128) >> 64) as usize
    }

    /// A uniform value in `low..=high`.
    pub fn between(&mut self, low: usize, high: usize) -> usize {
        low + self.below(high - low + 1)
    }

    /// A uniformly picked element of a non-empty slice.
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }

    /// Shuffles `items` in place (Fisher–Yates).
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i + 1);
            items.swap(i, j);
        }
    }

    /// `len` random bytes.
    pub fn bytes(&mut self, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len + 8);
        while out.len() < len {
            out.extend_from_slice(&self.next_u64().to_le_bytes());
        }
        out.truncate(len);
        out
    }
}
