//! FxHash (rustc's) maps: the sim and the draw hook hash small integer keys
//! (cells, bodies, GL ids) many times per tick/draw, and SipHash's DoS
//! resistance buys nothing for keys taken from game memory or GL.

#[derive(Default, Clone, Copy)]
pub(crate) struct FxHasher(u64);
impl FxHasher {
    #[inline]
    fn add(&mut self, w: u64) {
        self.0 = (self.0.rotate_left(5) ^ w).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
}
impl std::hash::Hasher for FxHasher {
    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut b = [0u8; 8];
            b[..chunk.len()].copy_from_slice(chunk);
            self.add(u64::from_le_bytes(b));
        }
    }
    fn write_u8(&mut self, i: u8) {
        self.add(i as u64)
    }
    fn write_u32(&mut self, i: u32) {
        self.add(i as u64)
    }
    fn write_i32(&mut self, i: i32) {
        self.add(i as u32 as u64)
    }
    fn write_u64(&mut self, i: u64) {
        self.add(i)
    }
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64)
    }
    fn finish(&self) -> u64 {
        // The multiply leaves aligned keys (pointers) with zero low bits, and
        // the table indexes by low bits: rotate high bits down (rustc-hash 2).
        self.0.rotate_left(26)
    }
}
pub(crate) type FxBuild = std::hash::BuildHasherDefault<FxHasher>;
pub(crate) type HashMap<K, V> = std::collections::HashMap<K, V, FxBuild>;
pub(crate) type HashSet<K> = std::collections::HashSet<K, FxBuild>;

#[cfg(test)]
mod tests {
    use std::hash::{BuildHasher, Hash, Hasher};

    #[test]
    fn aligned_pointers_spread_over_low_bits() {
        let b = super::FxBuild::default();
        let mut seen = std::collections::HashSet::new();
        for i in 0..256usize {
            let mut h = b.build_hasher();
            (0x1_0000_0000 + i * 16).hash(&mut h);
            seen.insert(h.finish() & 0xff);
        }
        assert!(seen.len() > 128, "{} of 256 low-byte buckets", seen.len());
    }
}
