//! A small seeded generator (SplitMix64), so a run's fault schedule follows
//! from its seed without a dependency.

/// SplitMix64.
#[derive(Debug, Clone)]
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number in `low..high`; `low` when the range is empty.
    pub(crate) fn range(&mut self, low: u64, high: u64) -> u64 {
        if high <= low {
            return low;
        }
        low + self.next_u64() % (high - low)
    }

    /// True `per_mille` times in a thousand.
    pub(crate) fn chance(&mut self, per_mille: u64) -> bool {
        self.range(0, 1000) < per_mille
    }

    /// One of `items`, each weighted by its first field.
    pub(crate) fn pick<'a, T>(&mut self, items: &'a [(u64, T)]) -> &'a T {
        let total: u64 = items.iter().map(|(w, _)| w).sum();
        let mut at = self.range(0, total);
        for (weight, item) in items {
            if at < *weight {
                return item;
            }
            at -= weight;
        }
        &items[items.len() - 1].1
    }
}

#[cfg(test)]
#[path = "rng_tests.rs"]
mod rng_tests;
