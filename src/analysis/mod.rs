//! Duplicate analyses over extracted records, plus the pair and cluster machinery they share.

pub mod types;

use std::collections::HashMap;

/// Interns strings to dense ids so sets become sorted `u32` slices.
#[derive(Default)]
pub struct Interner<'a> {
    ids: HashMap<&'a str, u32>,
}

impl<'a> Interner<'a> {
    pub fn id(&mut self, key: &'a str) -> u32 {
        let next = self.ids.len() as u32;
        *self.ids.entry(key).or_insert(next)
    }
}

/// Size of the intersection of two sorted, deduplicated id lists.
pub fn intersection_len(a: &[u32], b: &[u32]) -> usize {
    let (mut i, mut j, mut shared) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                shared += 1;
                i += 1;
                j += 1;
            }
        }
    }
    shared
}

/// `|A ∩ B| / |A ∪ B|` from the two set sizes and their intersection size.
pub fn jaccard(a_len: usize, b_len: usize, shared: usize) -> f64 {
    let union = a_len + b_len - shared;
    if union == 0 {
        0.0
    } else {
        shared as f64 / union as f64
    }
}

/// Connected components of `0..n` linked by `edges`; singletons are dropped.
/// Components keep their members in ascending order and come back ordered by first member.
pub fn components(n: usize, edges: impl IntoIterator<Item = (usize, usize)>) -> Vec<Vec<usize>> {
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }
    for (a, b) in edges {
        let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
        if ra != rb {
            parent[ra.max(rb)] = ra.min(rb);
        }
    }
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for x in 0..n {
        let root = find(&mut parent, x);
        groups.entry(root).or_default().push(x);
    }
    let mut out: Vec<Vec<usize>> = groups.into_values().filter(|g| g.len() > 1).collect();
    out.sort_by_key(|g| g[0]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersection_and_jaccard() {
        assert_eq!(intersection_len(&[1, 3, 5, 7], &[3, 4, 5]), 2);
        assert_eq!(intersection_len(&[], &[1]), 0);
        assert_eq!(jaccard(4, 3, 2), 0.4);
        assert_eq!(jaccard(0, 0, 0), 0.0);
    }

    #[test]
    fn groups_linked_items() {
        let groups = components(6, [(0, 2), (2, 4), (1, 5)]);
        assert_eq!(groups, vec![vec![0, 2, 4], vec![1, 5]]);
        assert!(components(3, []).is_empty());
    }

    #[test]
    fn interns_repeated_keys_once() {
        let mut interner = Interner::default();
        assert_eq!(
            (interner.id("a"), interner.id("b"), interner.id("a")),
            (0, 1, 0)
        );
    }
}
