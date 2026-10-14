//! Sorted, non-overlapping byte ranges.

use std::ops::Range;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Extents(Vec<Range<u64>>);

impl Extents {
    pub fn new() -> Self {
        Self::default()
    }

    /// Normalizes arbitrary (unsorted, overlapping, empty) ranges.
    pub fn from_ranges(ranges: impl IntoIterator<Item = Range<u64>>) -> Self {
        let mut v: Vec<Range<u64>> = ranges.into_iter().filter(|r| r.start < r.end).collect();
        v.sort_by_key(|r| r.start);
        let mut out: Vec<Range<u64>> = Vec::with_capacity(v.len());
        for r in v {
            match out.last_mut() {
                Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
                _ => out.push(r),
            }
        }
        Self(out)
    }

    pub fn ranges(&self) -> &[Range<u64>] {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn total(&self) -> u64 {
        self.0.iter().map(|r| r.end - r.start).sum()
    }

    pub fn add(&mut self, r: Range<u64>) {
        *self = Self::from_ranges(self.0.drain(..).chain(std::iter::once(r)));
    }

    /// Removes everything in `other`.
    pub fn subtract(&self, other: &Extents) -> Extents {
        let mut out = Vec::new();
        let mut j = 0;
        for r in &self.0 {
            let mut start = r.start;
            while j < other.0.len() && other.0[j].end <= start {
                j += 1;
            }
            let mut k = j;
            while start < r.end {
                match other.0.get(k) {
                    Some(o) if o.start < r.end => {
                        if o.start > start {
                            out.push(start..o.start);
                        }
                        start = start.max(o.end);
                        k += 1;
                    }
                    _ => {
                        out.push(start..r.end);
                        break;
                    }
                }
            }
        }
        Self(out)
    }

    /// Restricts to `[0, len)`.
    pub fn clip(&self, len: u64) -> Extents {
        Self(self.0.iter().filter(|r| r.start < len).map(|r| r.start..r.end.min(len)).collect())
    }

    /// The parts of `window` that are covered, relative to `window.start`.
    pub fn within(&self, window: Range<u64>) -> Vec<Range<u64>> {
        let i = self.0.partition_point(|r| r.end <= window.start);
        self.0[i..]
            .iter()
            .take_while(|r| r.start < window.end)
            .map(|r| r.start.max(window.start) - window.start..r.end.min(window.end) - window.start)
            .collect()
    }

    pub fn intersects(&self, window: Range<u64>) -> bool {
        let i = self.0.partition_point(|r| r.end <= window.start);
        self.0.get(i).is_some_and(|r| r.start < window.end)
    }

    /// The complement within `[0, len)`.
    pub fn gaps(&self, len: u64) -> Extents {
        Extents::from_ranges(std::iter::once(0..len)).subtract(self)
    }
}

/// Zeroes the bytes of `buf` that are not in `keep` (offsets relative to `buf`).
pub(crate) fn zero_outside(buf: &mut [u8], keep: &[Range<u64>]) {
    let mut pos = 0usize;
    for r in keep {
        buf[pos..r.start as usize].fill(0);
        pos = r.end as usize;
    }
    buf[pos..].fill(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_subtract_within() {
        let e = Extents::from_ranges([10..20, 0..5, 15..30, 40..40, 50..60]);
        assert_eq!(e.ranges(), [0..5, 10..30, 50..60]);
        assert_eq!(e.total(), 35);
        let s = e.subtract(&Extents::from_ranges([3..12, 25..55]));
        assert_eq!(s.ranges(), [0..3, 12..25, 55..60]);
        assert_eq!(e.within(4..52), [0..1, 6..26, 46..48]);
        assert!(e.intersects(29..31));
        assert!(!e.intersects(30..50));
        assert_eq!(e.gaps(70).ranges(), [5..10, 30..50, 60..70]);
        assert_eq!(e.clip(55).ranges(), [0..5, 10..30, 50..55]);
        let mut e = e;
        e.add(28..52);
        assert_eq!(e.ranges(), [0..5, 10..60]);
    }

    #[test]
    fn zero_outside_mask() {
        let mut b = [1u8; 10];
        zero_outside(&mut b, &[2..4, 6..7]);
        assert_eq!(b, [0, 0, 1, 1, 0, 0, 1, 0, 0, 0]);
    }
}
