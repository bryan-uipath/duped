//! Functions whose bodies are near-copies, whatever they are called: Jaccard similarity of
//! normalised token shingles, with MinHash + LSH banding to find candidate pairs.
//!
//! Local names are abstracted, so renamed copies match; syntax, property names and literals
//! are kept, so `switch` lookup tables with different labels don't.

use std::collections::{HashMap, HashSet};
use std::fmt::Write;

use rayon::prelude::*;
use serde_json::{Value, json};

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
    /// Report pairs acknowledged as deliberate.
    pub include_acknowledged: bool,
}

#[derive(Debug)]
pub struct BodyPair {
    /// Indices into the analysed records; `a` comes first in file order.
    pub a: usize,
    pub b: usize,
    /// Jaccard similarity of the two shingle sets.
    pub similarity: f64,
    /// Shingles both bodies have: roughly how many tokens are duplicated.
    pub shared: usize,
    /// Module relationship; `None` when modules aren't tracked.
    pub tag: Option<Tag>,
    /// Why the pair is deliberate, when it is.
    pub acknowledged: Option<String>,
}

/// Two files linked by reported pairs between at least two functions on each side:
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

struct Candidate {
    record: usize,
    /// Sorted, distinct shingle hashes.
    shingles: Vec<u64>,
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
            Record::Function(f) if is_candidate(r, options) => Some(Candidate {
                record,
                shingles: shingles(&f.body, options.shingle),
            }),
            _ => None,
        })
        .collect();

    let mut scored: Vec<(usize, usize, usize, f64)> =
        candidate_pairs(&candidates, options.threshold)
            .into_par_iter()
            .filter_map(|(i, j)| {
                let (a, b) = (&candidates[i], &candidates[j]);
                let shared = shared(&a.shingles, &b.shingles);
                let similarity = jaccard(a.shingles.len(), b.shingles.len(), shared);
                (similarity >= options.threshold)
                    .then_some((a.record, b.record, shared, similarity))
            })
            .collect();
    scored.sort_unstable_by_key(|&(a, b, ..)| (a, b));

    let mut pairs = Vec::new();
    let (mut hidden_same_file, mut hidden_acknowledged) = (0, 0);
    for (a, b, shared, similarity) in scored {
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
            similarity,
            shared,
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

/// Hashes of each `k` consecutive normalised tokens; a body shorter than `k` is one shingle.
/// Repeats are numbered, so a ten-case `switch` doesn't collapse into a three-case one.
fn shingles(body: &[Token], k: usize) -> Vec<u64> {
    let normalised: Vec<u32> = body
        .iter()
        .map(|&t| match t {
            Token::Identifier => IDENTIFIER,
            Token::Text(h) => h,
        })
        .collect();
    let mut out: Vec<u64> = normalised
        .windows(k.clamp(1, normalised.len()))
        .map(|window| {
            let h = window.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &v| {
                (h ^ u64::from(v)).wrapping_mul(0x0100_0000_01b3)
            });
            mix(h)
        })
        .collect();
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
fn candidate_pairs(candidates: &[Candidate], threshold: f64) -> Vec<(usize, usize)> {
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
    pairs.into_iter().collect()
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

/// File pairs whose reported pairs link at least two distinct functions on each side.
fn file_pairs(records: &[Record], pairs: &[BodyPair]) -> Vec<FilePair> {
    let mut groups: HashMap<(&str, &str), Vec<usize>> = HashMap::new();
    for (i, p) in pairs.iter().enumerate() {
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
            "\n{}. {:.2} similar, {} shared: {}{}",
            rank + 1,
            pair.similarity,
            pair.shared,
            relation_text(records, pair),
            pair_notes(&pair.tag, &pair.acknowledged)
        )
        .ok();
        for index in [pair.a, pair.b] {
            writeln!(out, "   {}", member_line(records, index, judge)).ok();
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
                "similarity": p.similarity,
                "shared": p.shared,
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

fn function_at(records: &[Record], index: usize) -> &FunctionRecord {
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

/// e.g. `Api.load (120 tokens)  [@x/core] src/api.ts:12`
fn member_line(records: &[Record], index: usize, judge: &dyn Judge) -> String {
    let f = function_at(records, index);
    format!(
        "{} ({} tokens)  {}{}:{}",
        display_name(f),
        f.body.len(),
        module_prefix(judge, index),
        f.location.file,
        f.location.start_line
    )
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
    use crate::lang::typescript;
    use crate::record::Language;
    use tree_sitter::Parser;

    fn options() -> BodyOptions {
        BodyOptions {
            shingle: 5,
            threshold: 0.5,
            min_tokens: 20,
            include_same_file: false,
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
}
