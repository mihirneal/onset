//! A tiny seeded random number generator (SplitMix64).
//!
//! We use our own instead of the `rand` crate so a seed recorded in a log
//! reproduces the exact same trial order forever, regardless of library
//! upgrades.

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform integer in `0..n` (n > 0), without modulo bias.
    pub fn below(&mut self, n: u64) -> u64 {
        let zone = u64::MAX - u64::MAX % n;
        loop {
            let x = self.next_u64();
            if x < zone {
                return x % n;
            }
        }
    }

    /// Fisher–Yates shuffle.
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i as u64 + 1) as usize;
            items.swap(i, j);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_order() {
        let mut a: Vec<u32> = (0..50).collect();
        let mut b = a.clone();
        Rng::new(42).shuffle(&mut a);
        Rng::new(42).shuffle(&mut b);
        assert_eq!(a, b);
        assert_ne!(a, (0..50).collect::<Vec<_>>());
    }

    #[test]
    fn shuffle_keeps_every_item() {
        let mut v: Vec<u32> = (0..100).collect();
        Rng::new(7).shuffle(&mut v);
        v.sort();
        assert_eq!(v, (0..100).collect::<Vec<_>>());
    }
}
