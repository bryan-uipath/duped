//! Duplicate analyses over extracted records, plus the candidate-pair, scoring and cluster
//! helpers they share.

pub mod bodies;
pub mod imports;
pub mod types;

use std::collections::HashMap;
use std::hash::Hash;

use crate::modules::{Home, Tag};

/// What the project says about a pair: its module tag and any acknowledgement.
#[derive(Debug, Default)]
pub struct Verdict {
    /// `None` when modules aren't tracked.
    pub tag: Option<Tag>,
    /// Why the pair is deliberate, e.g. `doc: mirrors`.
    pub acknowledged: Option<String>,
}

/// Project context for analyses: module tags, acknowledgements, cluster homes.
pub trait Judge {
    fn verdict(&self, a: usize, b: usize) -> Verdict;
    /// The record's module name, when modules are tracked.
    fn module(&self, record: usize) -> Option<&str>;
    fn home(&self, members: &[usize]) -> Option<Home>;
    /// One line about the module graph, e.g. `23 modules`.
    fn summary(&self) -> Option<String>;
}

/// No project context: nothing is tagged, acknowledged or hidden.
#[cfg(test)]
pub struct NoJudge;

#[cfg(test)]
impl Judge for NoJudge {
    fn verdict(&self, _: usize, _: usize) -> Verdict {
        Verdict::default()
    }
    fn module(&self, _: usize) -> Option<&str> {
        None
    }
    fn home(&self, _: &[usize]) -> Option<Home> {
        None
    }
    fn summary(&self) -> Option<String> {
        None
    }
}

/// Interns keys to dense ids so sets become sorted `u32` slices.
pub struct Interner<K> {
    ids: HashMap<K, u32>,
}

impl<K> Default for Interner<K> {
    fn default() -> Self {
        Self {
            ids: HashMap::new(),
        }
    }
}

impl<K: Hash + Eq> Interner<K> {
    pub fn id(&mut self, key: K) -> u32 {
        let next = self.ids.len() as u32;
        *self.ids.entry(key).or_insert(next)
    }
}

/// Calls `visit(i, j)` once for each `i < j` whose sorted id sets share an uncommon id.
///
/// An id is common when it is in more than `common_floor` sets and more than
/// `common_fraction` of them; common ids don't link sets, which keeps `id`-style keys
/// from pairing everything. A set made only of common ids is linked through its rarest
/// one, so exact copies of a common shape are still found.
pub fn seed_pairs(
    sets: &[&[u32]],
    common_floor: usize,
    common_fraction: f64,
    mut visit: impl FnMut(usize, usize),
) {
    let mut postings: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, set) in sets.iter().enumerate() {
        for &id in *set {
            postings.entry(id).or_default().push(i);
        }
    }
    let fraction_limit = common_fraction * sets.len() as f64;
    let is_common = |len: usize| len > common_floor && len as f64 > fraction_limit;

    let seeds_of = |set: &[u32]| -> Vec<&Vec<usize>> {
        set.iter()
            .map(|id| &postings[id])
            .filter(|posting| !is_common(posting.len()))
            .collect()
    };
    let fallback: Vec<bool> = sets.iter().map(|set| seeds_of(set).is_empty()).collect();

    // `seen[j] == i + 1` marks `j` as visited for `i`, without clearing between rows.
    let mut seen = vec![0usize; sets.len()];
    for (i, set) in sets.iter().enumerate() {
        let mut seeds = seeds_of(set);
        if fallback[i] {
            seeds.extend(set.iter().map(|id| &postings[id]).min_by_key(|p| p.len()));
        }
        for posting in seeds {
            for &j in posting {
                // Rows visit later sets; a fallback row also visits earlier non-fallback
                // sets, which can't have reached it through an uncommon id.
                let forward = j > i;
                let backward = j < i && fallback[i] && !fallback[j];
                if (forward || backward) && seen[j] != i + 1 {
                    seen[j] = i + 1;
                    visit(i.min(j), i.max(j));
                }
            }
        }
    }
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

/// Untagged pairs (modules not tracked) all rank alike.
pub fn tag_rank(tag: &Option<Tag>) -> u8 {
    tag.as_ref().map_or(0, Tag::rank)
}

/// ` — importable: a can import b (acknowledged: doc: mirrors)`
pub fn pair_notes(tag: &Option<Tag>, acknowledged: &Option<String>) -> String {
    let mut notes = String::new();
    if let Some(tag) = tag {
        notes.push_str(&format!(" — {}", tag.describe()));
    }
    if let Some(reason) = acknowledged {
        notes.push_str(&format!(" (acknowledged: {reason})"));
    }
    notes
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
    fn jaccard_of_sizes() {
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
    fn seeds_through_uncommon_ids_or_the_rarest_one() {
        // id 9 is common (in 4 of 5 sets, floor 2, fraction 0.5); 1 and 2 are not.
        let sets: Vec<&[u32]> = vec![&[1, 9], &[1, 9], &[2, 9], &[9], &[9]];
        let mut seen = Vec::new();
        seed_pairs(&sets, 2, 0.5, |i, j| seen.push((i, j)));
        seen.sort_unstable();
        // 0–1 share uncommon 1; 2's only uncommon id is its own. Sets 3 and 4 hold only
        // the common id, so it links them to every set.
        let expected = vec![
            (0, 1),
            (0, 3),
            (0, 4),
            (1, 3),
            (1, 4),
            (2, 3),
            (2, 4),
            (3, 4),
        ];
        assert_eq!(seen, expected);
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
