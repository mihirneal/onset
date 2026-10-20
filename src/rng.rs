//! Seeded random number generators.
//!
//! We use our own instead of the `rand` crate so a seed recorded in a log
//! reproduces the exact same trial order forever, regardless of library
//! upgrades. [`Rng`] (SplitMix64) shuffles trials at run time. [`PyRandom`]
//! draws exactly like Python's `random.Random(seed)`, so schedules that
//! `onset make-schedule` builds match ones made with Python for the same seed.

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

    /// Uniform float in `[0, 1)`, from the top 53 bits (all a float can hold).
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Fisher–Yates shuffle.
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i as u64 + 1) as usize;
            items.swap(i, j);
        }
    }
}

/// The Mersenne Twister (MT19937), seeded and used the way CPython's
/// `random.Random` does: `unit()` is `random()`, `below(n)` is
/// `_randbelow(n)` (so `choice`, `randint` and `shuffle` match too).
pub struct PyRandom {
    state: [u32; 624],
    next: usize,
}

impl PyRandom {
    /// Like `random.Random(seed)` for a non-negative integer seed.
    pub fn new(seed: u64) -> PyRandom {
        // The seed's 32-bit words, least significant first (`init_by_array`).
        let key: Vec<u32> =
            if seed >> 32 == 0 { vec![seed as u32] } else { vec![seed as u32, (seed >> 32) as u32] };
        let mut mt = [0u32; 624];
        mt[0] = 19_650_218;
        for i in 1..624 {
            mt[i] = 1_812_433_253u32.wrapping_mul(mt[i - 1] ^ (mt[i - 1] >> 30)).wrapping_add(i as u32);
        }
        let (mut i, mut j) = (1, 0);
        for _ in 0..624 {
            let prev = mt[i - 1] ^ (mt[i - 1] >> 30);
            mt[i] = (mt[i] ^ prev.wrapping_mul(1_664_525)).wrapping_add(key[j]).wrapping_add(j as u32);
            (i, j) = (i + 1, (j + 1) % key.len());
            if i == 624 {
                mt[0] = mt[623];
                i = 1;
            }
        }
        for _ in 0..623 {
            let prev = mt[i - 1] ^ (mt[i - 1] >> 30);
            mt[i] = (mt[i] ^ prev.wrapping_mul(1_566_083_941)).wrapping_sub(i as u32);
            i += 1;
            if i == 624 {
                mt[0] = mt[623];
                i = 1;
            }
        }
        mt[0] = 0x8000_0000;
        PyRandom { state: mt, next: 624 }
    }

    fn next_u32(&mut self) -> u32 {
        if self.next == 624 {
            for k in 0..624 {
                let y = (self.state[k] & 0x8000_0000) | (self.state[(k + 1) % 624] & 0x7fff_ffff);
                let mag = if y & 1 == 1 { 0x9908_b0df } else { 0 };
                self.state[k] = self.state[(k + 397) % 624] ^ (y >> 1) ^ mag;
            }
            self.next = 0;
        }
        let mut y = self.state[self.next];
        self.next += 1;
        y ^= y >> 11;
        y ^= (y << 7) & 0x9d2c_5680;
        y ^= (y << 15) & 0xefc6_0000;
        y ^ (y >> 18)
    }

    /// `getrandbits(k)` for k <= 64.
    fn bits(&mut self, k: u32) -> u64 {
        match k {
            0 => 0,
            1..=32 => (self.next_u32() >> (32 - k)) as u64,
            _ => {
                let low = self.next_u32() as u64;
                low | ((self.next_u32() >> (64 - k)) as u64) << 32
            }
        }
    }

    /// Uniform integer in `0..n` (n > 0).
    pub fn below(&mut self, n: u64) -> u64 {
        let k = 64 - n.leading_zeros();
        loop {
            let r = self.bits(k);
            if r < n {
                return r;
            }
        }
    }

    /// Uniform float in `[0, 1)`.
    pub fn unit(&mut self) -> f64 {
        let (a, b) = (self.next_u32() >> 5, self.next_u32() >> 6);
        (a as f64 * 67_108_864.0 + b as f64) / 9_007_199_254_740_992.0
    }

    /// Fisher–Yates shuffle, as `random.shuffle`.
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
    fn draws_like_python() {
        // Values from CPython's `random` module.
        let mut r = PyRandom::new(1001);
        assert_eq!(
            [r.unit(), r.unit(), r.unit()],
            [0.7966509679599704, 0.05862623274803458, 0.7602397739740907]
        );
        assert_eq!(PyRandom::new(0).unit(), 0.8444218515250481);
        assert_eq!(PyRandom::new((1 << 40) + 5).unit(), 0.5043802970418443);
        let mut r = PyRandom::new(7);
        let mut v: Vec<u32> = (0..10).collect();
        r.shuffle(&mut v);
        assert_eq!(v, [8, 3, 1, 4, 7, 0, 9, 6, 2, 5]);
        assert_eq!(9 + r.below(6), 13); // randint(9, 14)
        assert_eq!(r.below(1), 0);
        assert_eq!(r.below(5_000_000_000), 161_042_648);
    }

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
