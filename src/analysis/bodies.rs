//! Functions whose bodies are near-copies, whatever they are called: Jaccard similarity of
//! normalised token shingles, with MinHash + LSH banding to find candidate pairs. Pairs that
//! share rare shingles are also checked for a smaller body inside a larger one, or a long
//! block pasted into both.
//!
//! Local names are abstracted, so renamed copies match; syntax, property names and literals
//! are kept, so `switch` lookup tables with different labels don't.

use std::collections::{HashMap, HashSet};
use std::fmt::Write;

use rayon::prelude::*;
use serde::Serialize;
use serde_json::{Value, json};

use super::names::family;
use super::{Judge, jaccard, pair_notes, tag_rank};
use crate::modules::Tag;
use crate::record::{FunctionRecord, Record, Token, qualified};

pub struct BodyOptions {
    /// Tokens per shingle.
    pub shingle: usize,
    /// Minimum Jaccard similarity of shingle sets.
    pub threshold: f64,
    /// Bodies with fewer tokens are not compared.
    pub min_tokens: usize,
    /// Report pairs whose two functions are in one file.
    pub include_same_file: bool,
    /// Pair functions across language families.
    pub cross_language: bool,
    /// Report pairs acknowledged as deliberate.
    pub include_acknowledged: bool,
}

#[derive(Debug)]
pub struct BodyPair {
    /// Indices into the analysed records; `a` comes first in file order.
    pub a: usize,
    pub b: usize,
    pub kind: PairKind,
    /// Jaccard similarity of the two shingle sets.
    pub similarity: f64,
    /// Shingles both bodies have (in `region`, when there is one): roughly how many tokens
    /// are duplicated.
    pub shared: usize,
    /// Each side's shared lines, inclusive, for `contained` and `block` pairs.
    pub region: Option<Region>,
    /// Module relationship; `None` when modules aren't tracked.
    pub tag: Option<Tag>,
    /// Why the pair is deliberate, when it is.
    pub acknowledged: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PairKind {
    /// Jaccard similarity at the threshold: near-copies.
    Similar,
    /// Most of the smaller body is in the larger one, e.g. a wrapper or an inlined helper.
    Contained,
    /// A long run of tokens is in both, e.g. a block pasted into two larger functions.
    Block,
}

#[derive(Debug, Clone, Copy)]
pub struct Region {
    /// First and last lines, 1-based, inclusive.
    pub a: (usize, usize),
    pub b: (usize, usize),
    /// Shared windows over the longer side's region windows; gaps make it less than 1.
    pub density: f64,
}

/// Two files linked by reported `similar` pairs between at least two functions on each side:
/// likely a copied module rather than one copied helper.
#[derive(Debug)]
pub struct FilePair {
    pub a: String,
    pub b: String,
    /// Indices into [`BodyReport::pairs`], best first.
    pub pairs: Vec<usize>,
}

#[derive(Debug)]
pub struct BodyReport {
    /// Functions with enough body tokens to compare.
    pub candidates: usize,
    /// Qualifying pairs, most shared first.
    pub pairs: Vec<BodyPair>,
    /// File pairs sharing several functions, most shared first.
    pub files: Vec<FilePair>,
    /// Qualifying pairs left out because both functions are in one file.
    pub hidden_same_file: usize,
    /// Qualifying pairs left out because they are acknowledged.
    pub hidden_acknowledged: usize,
}

/// MinHash signature length; LSH splits it into bands.
const HASHES: usize = 128;

/// Every identifier becomes this placeholder; other tokens keep their text hash.
const IDENTIFIER: u32 = 1;

/// Shingles in more bodies than this are idioms or boilerplate, e.g. fetch scaffolding, and
/// don't link a pair for `contained` or `block`.
const MAX_SPREAD: usize = 10;

/// Rare shingles a pair must share to be checked for `contained` or `block`.
const MIN_LINKS: usize = 20;

/// A `contained` pair's region spans at least this fraction of one body's shingles.
const CONTAINED: f64 = 0.8;

/// A `block` pair's region has at least this many shared shingles, about 15 lines.
const BLOCK: usize = 100;

/// A region's shared windows over its span on the longer side; sparser chains are coincidence.
const MIN_DENSITY: f64 = 0.6;

/// Runs of equal shingles shorter than this don't start or extend a region.
const MIN_RUN: usize = 3;

/// A region bridges up to this many unshared shingles on each side, e.g. one edited line.
const GAP: usize = 10;

struct Candidate {
    record: usize,
    /// Sorted, distinct shingle hashes, repeats numbered.
    shingles: Vec<u64>,
    /// Each window's shingle hash, in body order, repeats not numbered.
    windows: Vec<u64>,
    /// Positions in `windows`, sorted by window.
    sorted: Vec<u32>,
}

pub fn find_duplicate_bodies(
    records: &[Record],
    options: &BodyOptions,
    judge: &dyn Judge,
) -> BodyReport {
    let candidates: Vec<Candidate> = records
        .iter()
        .enumerate()
        .filter_map(|(record, r)| match r {
            Record::Function(f) if is_candidate(r, options) => {
                let windows = windows(&f.body, options.shingle);
                let mut sorted: Vec<u32> = (0..windows.len() as u32).collect();
                sorted.sort_unstable_by_key(|&p| (windows[p as usize], p));
                Some(Candidate {
                    record,
                    shingles: numbered(&windows),
                    windows,
                    sorted,
                })
            }
            _ => None,
        })
        .collect();

    // Linked pairs are checked for every kind; LSH pairs only for `similar`.
    let linked = linked_pairs(&candidates);
    let mut checked = candidate_pairs(&candidates, options.threshold);
    checked.extend(&linked);
    let mut scored: Vec<Scored> = checked
        .into_par_iter()
        .filter_map(|(i, j)| {
            let (a, b) = (&candidates[i], &candidates[j]);
            // Off by default: a Rust body is rarely a copy of a TypeScript one, however similar.
            let language = |c: &Candidate| family(function_at(records, c.record).language);
            if !options.cross_language && language(a) != language(b) {
                return None;
            }
            score(records, a, b, options, linked.contains(&(i, j)))
        })
        .collect();
    scored.sort_unstable_by_key(|s| (s.a, s.b));

    let mut pairs = Vec::new();
    let (mut hidden_same_file, mut hidden_acknowledged) = (0, 0);
    for Scored {
        a,
        b,
        kind,
        shared,
        similarity,
        region,
    } in scored
    {
        let same_file = records[a].location().file == records[b].location().file;
        if same_file && !options.include_same_file {
            hidden_same_file += 1;
            continue;
        }
        let verdict = judge.verdict(a, b);
        if verdict.acknowledged.is_some() && !options.include_acknowledged {
            hidden_acknowledged += 1;
            continue;
        }
        pairs.push(BodyPair {
            a,
            b,
            kind,
            similarity,
            shared,
            region,
            tag: verdict.tag,
            acknowledged: verdict.acknowledged,
        });
    }
    // Most duplicated code first, so a large near-copy outranks a small exact one.
    pairs.sort_by(|x, y| {
        y.shared
            .cmp(&x.shared)
            .then(y.similarity.total_cmp(&x.similarity))
            .then(tag_rank(&x.tag).cmp(&tag_rank(&y.tag)))
            .then((x.a, x.b).cmp(&(y.a, y.b)))
    });
    let files = file_pairs(records, &pairs);
    BodyReport {
        candidates: candidates.len(),
        pairs,
        files,
        hidden_same_file,
        hidden_acknowledged,
    }
}

/// A function with enough body tokens to compare.
pub fn is_candidate(record: &Record, options: &BodyOptions) -> bool {
    matches!(record, Record::Function(f) if f.body.len() >= options.min_tokens.max(1))
}

/// A pair worth reporting, before same-file and acknowledged pairs are hidden.
struct Scored {
    a: usize,
    b: usize,
    kind: PairKind,
    shared: usize,
    similarity: f64,
    region: Option<Region>,
}

/// `similar` at the threshold; else, when `linked`, `contained` or `block` if a shared region
/// qualifies.
fn score(
    records: &[Record],
    a: &Candidate,
    b: &Candidate,
    options: &BodyOptions,
    linked: bool,
) -> Option<Scored> {
    let shared = shared(&a.shingles, &b.shingles);
    let similarity = jaccard(a.shingles.len(), b.shingles.len(), shared);
    let scored = |kind, shared, region| Scored {
        a: a.record,
        b: b.record,
        kind,
        shared,
        similarity,
        region,
    };
    if similarity >= options.threshold {
        return Some(scored(PairKind::Similar, shared, None));
    }
    if !linked {
        return None;
    }
    let (start, end, matched) = region(a, b)?;
    let density = matched as f64 / ((end.0 - start.0).max(end.1 - start.1) + 1) as f64;
    // The region's span over its body, not set overlap: a huge body has most shingles of any
    // small one. Span, so windows skipped as too repetitive still count.
    let containment = f64::max(
        (end.0 - start.0 + 1) as f64 / a.windows.len() as f64,
        (end.1 - start.1 + 1) as f64 / b.windows.len() as f64,
    );
    let kind = if containment >= CONTAINED {
        PairKind::Contained
    } else if matched >= BLOCK {
        PairKind::Block
    } else {
        return None;
    };
    // Window `w` covers tokens `w..w + k`. Lines the region only starts or ends in are left
    // out, e.g. the `) ;` ending the line before a pasted block.
    let lines = |record: usize, from: usize, to: usize| {
        let lines = &function_at(records, record).body_lines;
        let to = (to + options.shingle - 1).min(lines.len() - 1);
        let first = if from > 0 && lines[from - 1] == lines[from] {
            (from..=to).find(|&t| lines[t] != lines[from])
        } else {
            Some(from)
        };
        let last = if to + 1 < lines.len() && lines[to + 1] == lines[to] {
            (from..=to).rev().find(|&t| lines[t] != lines[to])
        } else {
            Some(to)
        };
        let (first, last) = match (first, last) {
            (Some(first), Some(last)) if first <= last => (first, last),
            _ => (from, to),
        };
        (lines[first] as usize, lines[last] as usize)
    };
    let region = Region {
        a: lines(a.record, start.0, end.0),
        b: lines(b.record, start.1, end.1),
        density,
    };
    Some(scored(kind, matched, Some(region)))
}

/// Hashes of each `k` consecutive normalised tokens, in body order; a body shorter than `k`
/// is one shingle.
fn windows(body: &[Token], k: usize) -> Vec<u64> {
    let normalised: Vec<u32> = body
        .iter()
        .map(|&t| match t {
            Token::Identifier => IDENTIFIER,
            Token::Text(h) => h,
        })
        .collect();
    normalised
        .windows(k.clamp(1, normalised.len()))
        .map(|window| {
            let h = window.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &v| {
                (h ^ u64::from(v)).wrapping_mul(0x0100_0000_01b3)
            });
            mix(h)
        })
        .collect()
}

/// The distinct shingles of `windows`. Repeats are numbered, so a ten-case `switch` doesn't
/// collapse into a three-case one.
fn numbered(windows: &[u64]) -> Vec<u64> {
    let mut out = windows.to_vec();
    out.sort_unstable();
    let (mut previous, mut repeat) = (None, 0u64);
    for shingle in &mut out {
        repeat = if previous == Some(*shingle) {
            repeat + 1
        } else {
            0
        };
        previous = Some(*shingle);
        *shingle = mix(*shingle ^ repeat);
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Index pairs `i < j` that share at least one LSH band of their MinHash signatures.
fn candidate_pairs(candidates: &[Candidate], threshold: f64) -> HashSet<(usize, usize)> {
    let coefficients: Vec<(u64, u64)> = (0..HASHES as u64)
        .map(|i| (mix(i * 2 + 1) | 1, mix(i * 2 + 2)))
        .collect();
    let signatures: Vec<Vec<u64>> = candidates
        .par_iter()
        .map(|c| {
            coefficients
                .iter()
                .map(|&(mul, add)| {
                    c.shingles
                        .iter()
                        .map(|&s| s.wrapping_mul(mul).wrapping_add(add))
                        .min()
                        .unwrap_or(u64::MAX)
                })
                .collect()
        })
        .collect();

    let rows = rows_per_band(threshold);
    let mut pairs = HashSet::new();
    for band in 0..HASHES / rows {
        let mut buckets: HashMap<u64, Vec<usize>> = HashMap::new();
        for (i, signature) in signatures.iter().enumerate() {
            let key = signature[band * rows..(band + 1) * rows]
                .iter()
                .fold(band as u64, |h, &v| mix(h ^ v));
            buckets.entry(key).or_default().push(i);
        }
        for bucket in buckets.values().filter(|b| b.len() > 1) {
            for (x, &i) in bucket.iter().enumerate() {
                pairs.extend(bucket[x + 1..].iter().map(|&j| (i, j)));
            }
        }
    }
    pairs
}

/// Index pairs `i < j` sharing at least [`MIN_LINKS`] distinct shingles that are each in at
/// most [`MAX_SPREAD`] bodies: an inverted index, so it doesn't depend on similarity.
fn linked_pairs(candidates: &[Candidate]) -> HashSet<(usize, usize)> {
    let mut postings: Vec<(u64, u32)> = candidates
        .par_iter()
        .enumerate()
        .flat_map_iter(|(i, c)| {
            let i = i as u32;
            let mut distinct: Vec<u64> = c.sorted.iter().map(|&p| c.windows[p as usize]).collect();
            distinct.dedup();
            distinct.into_iter().map(move |h| (h, i))
        })
        .collect();
    postings.par_sort_unstable();
    let mut links: Vec<(u32, u32)> = postings
        .chunk_by(|x, y| x.0 == y.0)
        .filter(|list| list.len() > 1 && list.len() <= MAX_SPREAD)
        .flat_map(|list| {
            list.iter()
                .enumerate()
                .flat_map(move |(x, &(_, i))| list[x + 1..].iter().map(move |&(_, j)| (i, j)))
        })
        .collect();
    links.par_sort_unstable();
    links
        .chunk_by(|x, y| x == y)
        .filter(|same| same.len() >= MIN_LINKS)
        .map(|same| (same[0].0 as usize, same[0].1 as usize))
        .collect()
}

/// First and last windows on each side, and shared windows.
type Chain = ((usize, usize), (usize, usize), usize);

/// The best chain of shared runs, in order on both sides with gaps of at most [`GAP`]
/// windows: its first and last windows on each side, and its shared windows. A run is a
/// maximal stretch of equal windows on one diagonal, at least [`MIN_RUN`] long.
fn region(a: &Candidate, b: &Candidate) -> Option<Chain> {
    let (a_order, b_order) = (&a.windows, &b.windows);
    let a: Vec<(u64, u32)> = a.sorted.iter().map(|&p| (a_order[p as usize], p)).collect();
    let b: Vec<(u64, u32)> = b.sorted.iter().map(|&p| (b_order[p as usize], p)).collect();
    // Matches as (diagonal, a position), from a merge of the sorted windows.
    let mut matches: Vec<(i64, u32)> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].0.cmp(&b[j].0) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                let h = a[i].0;
                let a_end = i + a[i..].iter().take_while(|w| w.0 == h).count();
                let b_end = j + b[j..].iter().take_while(|w| w.0 == h).count();
                // A window repeated all over both bodies, like `) ; } ) ;`, only adds noise.
                if (a_end - i) * (b_end - j) <= 64 {
                    for &(_, x) in &a[i..a_end] {
                        for &(_, y) in &b[j..b_end] {
                            matches.push((i64::from(y) - i64::from(x), x));
                        }
                    }
                }
                (i, j) = (a_end, b_end);
            }
        }
    }
    matches.sort_unstable();
    // Runs as (a start, b start, length), in a order, extended over the skipped repeats.
    let mut runs: Vec<(usize, usize, usize)> = matches
        .chunk_by(|x, y| x.0 == y.0 && x.1 + 1 == y.1)
        .filter(|run| run.len() >= MIN_RUN)
        .map(|run| {
            let (diagonal, x) = run[0];
            let (mut x, mut y, mut len) =
                (x as usize, (i64::from(x) + diagonal) as usize, run.len());
            while x > 0 && y > 0 && a_order[x - 1] == b_order[y - 1] {
                (x, y, len) = (x - 1, y - 1, len + 1);
            }
            while x + len < a_order.len()
                && y + len < b_order.len()
                && a_order[x + len] == b_order[y + len]
            {
                len += 1;
            }
            (x, y, len)
        })
        .collect();
    runs.sort_unstable();
    runs.dedup();
    // Best chain ending at each run: (shared windows, previous run, first run).
    let mut best: Vec<(usize, Option<usize>, usize)> = Vec::with_capacity(runs.len());
    for (r, &(x, y, len)) in runs.iter().enumerate() {
        let previous = (0..r)
            .filter(|&q| {
                let (px, py, plen) = runs[q];
                let (ex, ey) = (px + plen, py + plen);
                ex <= x && ey <= y && x - ex <= GAP && y - ey <= GAP
            })
            .max_by_key(|&q| best[q].0);
        let first = previous.map_or(r, |q| best[q].2);
        best.push((len + previous.map_or(0, |q| best[q].0), previous, first));
    }
    // Gaps can chain short coincidental runs across a whole body; a copy is mostly shared.
    // Each run alone is a candidate too, so a sparse chain can't hide a dense block in it.
    let dense = |(start, end, matched): Chain| {
        let span = (end.0 - start.0).max(end.1 - start.1) + 1;
        (matched as f64 / span as f64 >= MIN_DENSITY).then_some((start, end, matched))
    };
    let chain = |first: usize, last: usize, matched: usize| {
        let (x, y, len) = runs[last];
        (
            (runs[first].0, runs[first].1),
            (x + len - 1, y + len - 1),
            matched,
        )
    };
    best.iter()
        .enumerate()
        .flat_map(|(r, &(matched, _, first))| [chain(first, r, matched), chain(r, r, runs[r].2)])
        .filter_map(dense)
        .max_by_key(|&(.., matched)| matched)
}

/// Most rows per band (so fewest false candidates) that still finds a pair at `threshold`
/// 99% of the time: P = 1 - (1 - t^rows)^bands.
fn rows_per_band(threshold: f64) -> usize {
    (1..=8)
        .rev()
        .find(|&rows| {
            let bands = (HASHES / rows) as i32;
            1.0 - (1.0 - threshold.powi(rows as i32)).powi(bands) >= 0.99
        })
        .unwrap_or(1)
}

/// Size of the intersection of two sorted, distinct sets.
fn shared(a: &[u64], b: &[u64]) -> usize {
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

/// File pairs whose `similar` pairs link at least two distinct functions on each side.
fn file_pairs(records: &[Record], pairs: &[BodyPair]) -> Vec<FilePair> {
    let mut groups: HashMap<(&str, &str), Vec<usize>> = HashMap::new();
    for (i, p) in pairs.iter().enumerate() {
        if p.kind != PairKind::Similar {
            continue;
        }
        let (fa, fb) = (&records[p.a].location().file, &records[p.b].location().file);
        if fa != fb {
            groups.entry((fa, fb)).or_default().push(i);
        }
    }
    let mut files: Vec<FilePair> = groups
        .into_iter()
        .filter(|(_, linked)| {
            let first = &pairs[linked[0]];
            linked.iter().any(|&i| pairs[i].a != first.a)
                && linked.iter().any(|&i| pairs[i].b != first.b)
        })
        .map(|((a, b), linked)| FilePair {
            a: a.to_string(),
            b: b.to_string(),
            pairs: linked,
        })
        .collect();
    files.sort_by(|x, y| {
        file_shared(y, pairs)
            .cmp(&file_shared(x, pairs))
            .then((&x.a, &x.b).cmp(&(&y.a, &y.b)))
    });
    files
}

pub fn file_shared(file: &FilePair, pairs: &[BodyPair]) -> usize {
    file.pairs.iter().map(|&i| pairs[i].shared).sum()
}

/// splitmix64's finaliser: spreads every input bit over the output.
fn mix(mut x: u64) -> u64 {
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

// ----- output -----

/// Human-readable report: file pairs, then function pairs.
pub fn render_text(
    records: &[Record],
    report: &BodyReport,
    top: usize,
    options: &BodyOptions,
    judge: &dyn Judge,
) -> String {
    let mut out = String::new();
    writeln!(
        out,
        "{} function pairs and {} file pairs from {} candidate functions (threshold {:.2}, shingle {}, min tokens {}).",
        report.pairs.len(),
        report.files.len(),
        report.candidates,
        options.threshold,
        options.shingle,
        options.min_tokens
    )
    .ok();
    if let Some(summary) = judge.summary() {
        writeln!(out, "Modules: {summary}.").ok();
    }
    let mut hidden = Vec::new();
    if report.hidden_same_file > 0 {
        hidden.push(format!(
            "{} same-file (--include-same-file)",
            report.hidden_same_file
        ));
    }
    if report.hidden_acknowledged > 0 {
        hidden.push(format!(
            "{} acknowledged (--include-acknowledged)",
            report.hidden_acknowledged
        ));
    }
    if !hidden.is_empty() {
        writeln!(out, "Hidden pairs: {}.", hidden.join(", ")).ok();
    }

    if !report.files.is_empty() {
        writeln!(out, "\nFiles sharing several functions:").ok();
    }
    for (rank, file) in report.files.iter().take(top).enumerate() {
        let first = &report.pairs[file.pairs[0]];
        writeln!(
            out,
            "\n{}. {} pairs, {} shared{}",
            rank + 1,
            file.pairs.len(),
            file_shared(file, &report.pairs),
            // Acknowledgements are per pair, so the file line shows only the tag.
            pair_notes(&first.tag, &None)
        )
        .ok();
        for (path, index) in [(&file.a, first.a), (&file.b, first.b)] {
            writeln!(out, "   {}{path}", module_prefix(judge, index)).ok();
        }
        for &i in &file.pairs {
            let pair = &report.pairs[i];
            writeln!(
                out,
                "   - {:.2} similar, {} shared: {}",
                pair.similarity,
                pair.shared,
                relation_text(records, pair)
            )
            .ok();
        }
    }
    if report.files.len() > top {
        writeln!(
            out,
            "\n… {} more file pairs; raise --top to see them.",
            report.files.len() - top
        )
        .ok();
    }

    if !report.pairs.is_empty() {
        writeln!(out, "\nFunction pairs:").ok();
    }
    for (rank, pair) in report.pairs.iter().take(top).enumerate() {
        writeln!(
            out,
            "\n{}. {}, {} shared: {}{}",
            rank + 1,
            kind_text(pair),
            pair.shared,
            relation_text(records, pair),
            pair_notes(&pair.tag, &pair.acknowledged)
        )
        .ok();
        let region = pair.region.map(|r| (r.a, r.b));
        for (index, lines) in [(pair.a, region.map(|r| r.0)), (pair.b, region.map(|r| r.1))] {
            writeln!(out, "   {}", member_line(records, index, lines, judge)).ok();
        }
    }
    if report.pairs.len() > top {
        writeln!(
            out,
            "\n… {} more function pairs; raise --top to see them.",
            report.pairs.len() - top
        )
        .ok();
    }
    out
}

/// Everything, untruncated: `{ summary, pairs, files }`.
pub fn to_json(records: &[Record], report: &BodyReport, judge: &dyn Judge) -> Value {
    let pairs: Vec<Value> = report
        .pairs
        .iter()
        .map(|p| {
            json!({
                "a": member_json(records, p.a, judge),
                "b": member_json(records, p.b, judge),
                "kind": p.kind,
                "similarity": p.similarity,
                "shared": p.shared,
                "region": p.region.map(|r| json!({ "a": [r.a.0, r.a.1], "b": [r.b.0, r.b.1] })),
                "tag": p.tag,
                "acknowledged": p.acknowledged,
            })
        })
        .collect();
    let files: Vec<Value> = report
        .files
        .iter()
        .map(|f| {
            json!({
                "a": f.a,
                "b": f.b,
                "pairs": f.pairs,
                "shared": file_shared(f, &report.pairs),
                "tag": report.pairs[f.pairs[0]].tag,
            })
        })
        .collect();
    json!({
        "summary": {
            "candidates": report.candidates,
            "pairs": report.pairs.len(),
            "files": report.files.len(),
            "modules": judge.summary(),
            "hidden": { "same_file": report.hidden_same_file, "acknowledged": report.hidden_acknowledged },
        },
        "pairs": pairs,
        "files": files,
    })
}

pub fn function_at(records: &[Record], index: usize) -> &FunctionRecord {
    match &records[index] {
        Record::Function(f) => f,
        Record::Type(_) => unreachable!("pairs only reference function records"),
    }
}

fn display_name(f: &FunctionRecord) -> String {
    qualified(f.scope.as_deref(), &f.name)
}

fn module_prefix(judge: &dyn Judge, index: usize) -> String {
    judge
        .module(index)
        .map(|m| format!("[{m}] "))
        .unwrap_or_default()
}

/// e.g. `Api.load (120 tokens)  [@x/core] src/api.ts:12`, or `src/api.ts:30-48` for a
/// shared region.
fn member_line(
    records: &[Record],
    index: usize,
    region: Option<(usize, usize)>,
    judge: &dyn Judge,
) -> String {
    let f = function_at(records, index);
    let at = match region {
        Some((start, end)) => format!("{start}-{end}"),
        None => f.location.start_line.to_string(),
    };
    format!(
        "{} ({} tokens)  {}{}:{at}",
        display_name(f),
        f.body.len(),
        module_prefix(judge, index),
        f.location.file,
    )
}

/// `0.62 similar`, `contained` or `block`.
pub fn kind_text(pair: &BodyPair) -> String {
    match pair.kind {
        PairKind::Similar => format!("{:.2} similar", pair.similarity),
        PairKind::Contained => "contained".to_string(),
        PairKind::Block => "block".to_string(),
    }
}

fn relation_text(records: &[Record], pair: &BodyPair) -> String {
    format!(
        "{} ~ {}",
        display_name(function_at(records, pair.a)),
        display_name(function_at(records, pair.b))
    )
}

fn member_json(records: &[Record], index: usize, judge: &dyn Judge) -> Value {
    let f = function_at(records, index);
    json!({
        "name": f.name,
        "scope": f.scope,
        "language": f.language,
        "module": judge.module(index),
        "file": f.location.file,
        "line": f.location.start_line,
        "tokens": f.body.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::NoJudge;
    use crate::lang::{rust, typescript};
    use crate::record::Language;
    use tree_sitter::Parser;

    fn options() -> BodyOptions {
        BodyOptions {
            shingle: 5,
            threshold: 0.5,
            min_tokens: 20,
            include_same_file: false,
            cross_language: false,
            include_acknowledged: false,
        }
    }

    /// Function records from `(file, source)` pairs.
    fn extract(files: &[(&str, &str)]) -> Vec<Record> {
        files
            .iter()
            .flat_map(|(file, source)| {
                typescript::extract(&mut Parser::new(), source, file, Language::TypeScript)
            })
            .filter(|r| matches!(r, Record::Function(_)))
            .collect()
    }

    fn names(records: &[Record], report: &BodyReport) -> Vec<(String, String)> {
        report
            .pairs
            .iter()
            .map(|p| (records[p.a].name().into(), records[p.b].name().into()))
            .collect()
    }

    const LOAD: &str = "export function loadUser(id: string, cache: Map<string, User>) {
        const hit = cache.get(id);
        if (hit) return hit;
        const user = fetchUser(id).then((u) => { cache.set(id, u); return u; });
        if (!user.active) { throw new Error('inactive'); }
        return user;
    }";

    #[test]
    fn finds_renamed_copies_with_small_edits() {
        // Renamed locals, one added guard: still a copy.
        let copy = "export function fetchAccount(key: string, store: Map<string, User>, signal?: AbortSignal) {
            if (signal?.aborted) return null;
            const found = store.get(key);
            if (found) return found;
            const account = fetchUser(key).then((a) => { store.set(key, a); return a; });
            if (!account.active) { throw new Error('inactive'); }
            return account;
        }";
        let unrelated = "export function render(rows: Row[]) {
            for (const row of rows) { console.log(row.title, row.width * 2, row.height); }
            return rows.map((r) => r.title).join(', ') + String(rows.length);
        }";
        let records = extract(&[("a.ts", LOAD), ("b.ts", copy), ("c.ts", unrelated)]);
        let report = find_duplicate_bodies(&records, &options(), &NoJudge);
        assert_eq!(
            names(&records, &report),
            vec![("loadUser".into(), "fetchAccount".into())]
        );
        let pair = &report.pairs[0];
        assert!(pair.similarity > 0.5 && pair.similarity < 1.0, "{pair:?}");
        assert_eq!(report.candidates, 3);
    }

    #[test]
    fn lookup_tables_with_different_labels_do_not_match() {
        let table = |cases: &[&str]| {
            let arms: String = cases
                .iter()
                .map(|c| format!("case '{c}': return t('label_{c}', '{c}');\n"))
                .collect();
            format!("export function label(kind: string) {{ switch (kind) {{ {arms} }} }}")
        };
        let records = extract(&[
            (
                "a.ts",
                &table(&["app", "asset", "queue", "bucket", "index"]),
            ),
            (
                "b.ts",
                &table(&["block", "log", "filter", "warn", "escalate"]),
            ),
        ]);
        let report = find_duplicate_bodies(&records, &options(), &NoJudge);
        assert!(report.pairs.is_empty(), "{:?}", names(&records, &report));
    }

    #[test]
    fn languages_only_match_themselves() {
        let body = "{ let x = a.first(b, 1); let y = a.second(x, 2); let z = a.third(y, 3); return a.fourth(z, 4); }";
        let mut records = extract(&[("a.ts", &format!("export function f(a, b) {body}"))]);
        let rs = format!("pub fn f(a: &A, b: u32) -> u32 {body}");
        records.extend(rust::extract(&mut Parser::new(), &rs, "b.rs"));
        let shown = BodyOptions {
            min_tokens: 1,
            ..options()
        };
        let report = find_duplicate_bodies(&records, &shown, &NoJudge);
        assert!(report.pairs.is_empty(), "{:?}", names(&records, &report));

        let cross = BodyOptions {
            cross_language: true,
            ..shown
        };
        assert_eq!(
            find_duplicate_bodies(&records, &cross, &NoJudge)
                .pairs
                .len(),
            1
        );
    }

    #[test]
    fn repeats_count_toward_size() {
        // Without numbered repeats both bodies would be the same handful of shingles.
        let fill = |n| {
            let body = "list.push(item.value + 1);".repeat(n);
            format!("export function fill(list: number[], item: Item) {{ {body} }}")
        };
        let records = extract(&[("a.ts", &fill(4)), ("b.ts", &fill(12))]);
        let report = find_duplicate_bodies(&records, &options(), &NoJudge);
        assert!(report.pairs.is_empty(), "{:?}", report.pairs);
    }

    #[test]
    fn hides_same_file_pairs_and_tiny_bodies() {
        let twice = format!("{LOAD}\n{}", LOAD.replace("loadUser", "loadAgain"));
        let records = extract(&[("a.ts", &twice)]);
        let report = find_duplicate_bodies(&records, &options(), &NoJudge);
        assert_eq!((report.pairs.len(), report.hidden_same_file), (0, 1));

        let shown = BodyOptions {
            include_same_file: true,
            ..options()
        };
        let report = find_duplicate_bodies(&records, &shown, &NoJudge);
        assert_eq!(report.pairs[0].similarity, 1.0);

        let large = BodyOptions {
            min_tokens: 500,
            ..shown
        };
        assert_eq!(
            find_duplicate_bodies(&records, &large, &NoJudge).candidates,
            0
        );
    }

    #[test]
    fn rolls_up_files_that_share_several_functions() {
        let save = "export function saveUser(user: User, cache: Map<string, User>) {
            cache.set(user.id, user);
            return postUser(user).catch((err) => { cache.delete(user.id); throw err; });
        }";
        let pair = format!("{LOAD}\n{save}");
        let renamed = pair
            .replace("loadUser", "getUser")
            .replace("saveUser", "putUser");
        // `c.ts` copies only one function: a helper, not a module.
        let records = extract(&[("a.ts", &pair), ("b.ts", &renamed), ("c.ts", LOAD)]);
        let report = find_duplicate_bodies(&records, &options(), &NoJudge);
        assert_eq!(report.pairs.len(), 4);
        let [file] = &report.files[..] else {
            panic!("{:?}", report.files)
        };
        assert_eq!((file.a.as_str(), file.b.as_str()), ("a.ts", "b.ts"));
        assert_eq!(file.pairs.len(), 2);
        // Bigger shared bodies rank first.
        assert_eq!(names(&records, &report)[0].0, "loadUser");
    }

    #[test]
    fn banding_finds_pairs_at_the_threshold() {
        for threshold in [0.3, 0.5, 0.7, 0.9] {
            let rows = rows_per_band(threshold);
            let bands = (HASHES / rows) as i32;
            assert!(1.0 - (1.0 - f64::powi(threshold, rows as i32)).powi(bands) >= 0.99);
        }
        assert_eq!(rows_per_band(0.5), 3);
    }

    #[test]
    fn renders_text_and_json() {
        let copy = LOAD
            .replace("loadUser", "readUser")
            .replace("cache", "memo");
        let records = extract(&[("a.ts", LOAD), ("b.ts", &copy)]);
        let report = find_duplicate_bodies(&records, &options(), &NoJudge);
        let text = render_text(&records, &report, 40, &options(), &NoJudge);
        assert!(text.contains("1 function pairs and 0 file pairs from 2 candidate functions"));
        assert!(text.contains("1.00 similar"), "{text}");
        assert!(text.contains("loadUser ~ readUser"), "{text}");
        assert!(text.contains("a.ts:1"), "{text}");
        let json = to_json(&records, &report, &NoJudge);
        assert_eq!(json["pairs"][0]["b"]["name"], "readUser");
        assert_eq!(json["summary"]["files"], 0);
    }
    /// `n` one-line statements unique to `tag`, so they match nothing elsewhere.
    fn filler(tag: &str, n: usize) -> String {
        (0..n)
            .map(|i| format!("console.log('{tag}-{i}');\n"))
            .collect()
    }

    /// Twelve distinct lines, about 200 tokens.
    fn block() -> String {
        (0..12)
            .map(|i| format!("result.field{i} = source.read('key{i}', {i}).trim();\n"))
            .collect()
    }

    /// `function name() {` on line 1, `before` filler lines, the block, then `after` lines.
    fn host(name: &str, before: usize, after: usize) -> String {
        format!(
            "export function {name}(result, source) {{\n{}{}{}}}\n",
            filler(&format!("{name}-before"), before),
            block(),
            filler(&format!("{name}-after"), after)
        )
    }

    #[test]
    fn finds_a_block_pasted_into_two_large_functions() {
        let records = extract(&[("a.ts", &host("a", 20, 20)), ("b.ts", &host("b", 30, 10))]);
        let report = find_duplicate_bodies(&records, &options(), &NoJudge);
        let [pair] = &report.pairs[..] else {
            panic!("{:?}", report.pairs)
        };
        assert_eq!(pair.kind, PairKind::Block);
        assert!(pair.similarity < 0.5, "{pair:?}");
        // Only the block's lines, not the whole functions.
        let region = pair.region.expect("a block has a region");
        assert_eq!((region.a, region.b), ((22, 33), (32, 43)));
        let text = render_text(&records, &report, 40, &options(), &NoJudge);
        assert!(text.contains("1. block, "), "{text}");
        assert!(
            text.contains("a.ts:22-33") && text.contains("b.ts:32-43"),
            "{text}"
        );
        let json = to_json(&records, &report, &NoJudge);
        assert_eq!(json["pairs"][0]["kind"], "block");
        assert_eq!(json["pairs"][0]["region"]["b"], json!([32, 43]));
    }

    #[test]
    fn finds_a_wrapper_inside_a_larger_function() {
        let wrapper = format!("export function wrap(result, source) {{\n{}}}\n", block());
        let records = extract(&[("a.ts", &wrapper), ("b.ts", &host("big", 40, 40))]);
        let report = find_duplicate_bodies(&records, &options(), &NoJudge);
        let [pair] = &report.pairs[..] else {
            panic!("{:?}", report.pairs)
        };
        assert_eq!(pair.kind, PairKind::Contained);
        assert!(pair.similarity < 0.5, "{pair:?}");
        let region = pair.region.expect("contained has a region");
        assert_eq!((region.a, region.b), ((2, 13), (42, 53)));
    }

    #[test]
    fn sparse_chains_are_not_contained() {
        // Same shape, different literal on every line: 7 of each 12 windows shared, chained
        // across the whole body by gaps, but under MIN_DENSITY.
        let shape = |tag: &str| {
            let lines: String = (0..30)
                .map(|i| format!("result.field{i} = source.read('{tag}-{i}');\n"))
                .collect();
            format!("export function {tag}(result, source) {{\n{lines}}}\n")
        };
        let records = extract(&[("a.ts", &shape("a")), ("b.ts", &shape("b"))]);
        let report = find_duplicate_bodies(&records, &options(), &NoJudge);
        assert!(report.pairs.is_empty(), "{:?}", report.pairs);
    }

    #[test]
    fn a_dense_block_survives_a_sparse_chain_after_it() {
        // 12 identical lines, then a long same-shape tail whose sparse chain is larger.
        let body = |tag: &str| {
            let block: String = (0..12)
                .map(|i| format!("result.field{i} = source.read('key{i}', {i}).trim();\n"))
                .collect();
            let gap: String = (0..20)
                .map(|i| format!("console.log('{tag}-sep-{i}');\n"))
                .collect();
            let tail: String = (0..100)
                .map(|i| format!("result.other{i} = source.read('{tag}-{i}');\n"))
                .collect();
            format!("export function {tag}(result, source) {{\n{block}{gap}{tail}}}\n")
        };
        let records = extract(&[("a.ts", &body("a")), ("b.ts", &body("b"))]);
        let report = find_duplicate_bodies(&records, &options(), &NoJudge);
        let pair = report.pairs.first().expect("the block is reported");
        assert_eq!(pair.kind, PairKind::Block, "{pair:?}");
        let region = pair.region.expect("block has a region");
        assert_eq!(region.a, region.b);
        // The signature matches too (names are abstracted); the tail is left out.
        assert_eq!(region.a, (1, 13), "{region:?}");
    }

    #[test]
    fn boilerplate_in_many_functions_links_none() {
        // Each block shingle is in more than MAX_SPREAD bodies.
        let files: Vec<(String, String)> = (0..=MAX_SPREAD)
            .map(|i| (format!("f{i}.ts"), host(&format!("f{i}"), 30, 10)))
            .collect();
        let files: Vec<(&str, &str)> = files
            .iter()
            .map(|(f, s)| (f.as_str(), s.as_str()))
            .collect();
        let report = find_duplicate_bodies(&extract(&files), &options(), &NoJudge);
        assert!(report.pairs.is_empty(), "{:?}", report.pairs);
    }
}
