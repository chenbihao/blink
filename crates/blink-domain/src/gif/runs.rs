//! Compact inclusive edit intervals. Snapshots share immutable runs; a range
//! selection never allocates one edit or undo record for every source frame.
use std::{collections::BTreeSet, sync::Arc};

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Runs(Arc<Vec<(u32, u32, u32)>>);

#[derive(Clone, Debug)]
pub(super) struct Change {
    a: u32,
    b: u32,
    before: Option<u32>,
    after: Option<u32>,
}

impl Runs {
    pub fn get(&self, index: &u32) -> Option<&u32> {
        let n = self.0.partition_point(|r| r.0 <= *index);
        n.checked_sub(1).and_then(|n| {
            let r = &self.0[n];
            (*index <= r.1).then_some(&r.2)
        })
    }
    pub fn contains(&self, index: &u32) -> bool {
        self.get(index).is_some()
    }
    pub fn ranges(&self) -> &[(u32, u32, u32)] {
        &self.0
    }
    pub fn insert(&mut self, index: u32, value: u32) -> bool {
        if self.get(&index) == Some(&value) {
            return false;
        }
        self.set(index, index, Some(value));
        true
    }
    pub fn remove(&mut self, index: &u32) -> bool {
        if self.get(index).is_none() {
            return false;
        }
        self.set(*index, *index, None);
        true
    }
    pub fn set(&mut self, a: u32, b: u32, value: Option<u32>) {
        let mut next = Vec::with_capacity(self.0.len() + 2);
        for &(start, end, v) in self.0.iter() {
            if end < a || start > b {
                next.push((start, end, v));
                continue;
            }
            if start < a {
                next.push((start, a - 1, v));
            }
            if end > b {
                next.push((b + 1, end, v));
            }
        }
        if let Some(v) = value {
            next.push((a, b, v));
        }
        next.sort_unstable_by_key(|r| r.0);
        let mut merged: Vec<(u32, u32, u32)> = Vec::with_capacity(next.len());
        for (start, end, v) in next {
            match merged.last_mut() {
                Some((_, last, previous))
                    if *previous == v && (*last as u64 + 1) == start as u64 =>
                {
                    *last = end
                }
                _ => merged.push((start, end, v)),
            }
        }
        self.0 = Arc::new(merged);
    }
    pub fn changes(&self, next: &Self) -> Vec<Change> {
        if Arc::ptr_eq(&self.0, &next.0) {
            return Vec::new();
        }
        let boundaries: BTreeSet<u64> = self
            .0
            .iter()
            .chain(next.0.iter())
            .flat_map(|r| [r.0 as u64, r.1 as u64 + 1])
            .collect();
        let boundaries: Vec<_> = boundaries.into_iter().collect();
        let mut out: Vec<Change> = Vec::new();
        for pair in boundaries.windows(2) {
            let a = pair[0] as u32;
            let b = (pair[1] - 1) as u32;
            let before = self.get(&a).copied();
            let after = next.get(&a).copied();
            if before == after {
                continue;
            }
            match out.last_mut() {
                Some(last)
                    if last.b as u64 + 1 == a as u64
                        && last.before == before
                        && last.after == after =>
                {
                    last.b = b
                }
                _ => out.push(Change {
                    a,
                    b,
                    before,
                    after,
                }),
            }
        }
        out
    }
    pub fn apply(&mut self, changes: &[Change], forward: bool) {
        for c in changes {
            self.set(c.a, c.b, if forward { c.after } else { c.before });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn overlapping_edits_and_deltas_match_a_small_point_reference() {
        let mut actual = Runs::default();
        let mut reference = std::collections::BTreeMap::new();
        let mut random = 13u32;
        for step in 0..500 {
            let before = actual.clone();
            random = random.wrapping_mul(1664525).wrapping_add(1013904223);
            let a = random % 200;
            let b = (a + (random >> 16) % 30).min(199);
            let value = (step % 3 != 0).then_some(step % 5);
            actual.set(a, b, value);
            for i in a..=b {
                if let Some(v) = value {
                    reference.insert(i, v);
                } else {
                    reference.remove(&i);
                }
            }
            for i in 0..200 {
                assert_eq!(actual.get(&i), reference.get(&i));
            }
            let changes = before.changes(&actual);
            let after = actual.clone();
            actual.apply(&changes, false);
            assert_eq!(actual, before);
            actual.apply(&changes, true);
            assert_eq!(actual, after);
        }
    }

    #[test]
    fn huge_ranges_split_merge_and_delta_without_expanding_frames() {
        let mut a = Runs::default();
        a.set(0, u32::MAX, Some(100));
        assert_eq!(a.ranges().len(), 1);
        let frozen = a.clone();
        a.set(50, 100, None);
        a.set(75, 80, Some(200));
        let changes = frozen.changes(&a);
        assert_eq!(changes.len(), 3);
        assert_eq!(a.get(&u32::MAX), Some(&100));
        a.apply(&changes, false);
        assert_eq!(a, frozen);
        a.apply(&changes, true);
        assert_eq!(a.get(&60), None);
        assert_eq!(a.get(&78), Some(&200));
        a.set(50, 100, Some(100));
        assert_eq!(a, frozen);
    }
}
