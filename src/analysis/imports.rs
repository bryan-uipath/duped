//! Files that import the same unusual set of things: often ported copies, even when every
//! declaration was renamed.

use std::collections::HashMap;
use std::fmt::Write;

use serde_json::{Value, json};

use super::{Interner, Judge};
use crate::lang::typescript::Import;
use crate::modules::Tag;

pub struct ImportOptions {
    /// A pair needs shared rare items from at least this many specifiers.
    pub min_shared: usize,
    /// Minimum IDF-weighted Jaccard similarity of the two files' import items.
    pub threshold: f64,
    /// Items in more than this fraction of files (and more than [`RARE_FLOOR`] of them) are
    /// not rare: they still score, but don't seed pairs or count toward `min_shared`.
    pub rare_fraction: f64,
    /// Report pairs whose two files are in one module.
    pub include_same_module: bool,
}

/// An item is only "common" once it is in more files than this, whatever the fraction.
const RARE_FLOOR: usize = 10;

/// One scanned file and its import items, e.g. `react#useState` or `./a.css`.
pub struct FileImports {
    pub file: String,
    /// Sorted and deduplicated.
    pub items: Vec<String>,
}

#[derive(Debug)]
pub struct ImportPair {
    /// Indices into the scanned files; `a` comes first in file order.
    pub a: usize,
    pub b: usize,
    /// IDF-weighted Jaccard similarity of the two item sets.
    pub score: f64,
    /// Shared items with how many files have each, rarest first.
    pub shared: Vec<(String, usize)>,
    /// How many shared items are rare.
    pub rare: usize,
    /// How many specifiers those rare items come from; one compound component's parts
    /// (`Dialog`, `DialogTitle`, …) count once.
    pub sources: usize,
    /// Module relationship; `None` when modules aren't tracked.
    pub tag: Option<Tag>,
}

#[derive(Debug)]
pub struct ImportReport {
    /// Files with at least one import item.
    pub files: usize,
    /// Qualifying pairs, best first.
    pub pairs: Vec<ImportPair>,
    /// Qualifying pairs left out because both files share a module.
    pub hidden_same_module: usize,
}

/// Each file's import items: `specifier#name` per imported name, or the bare specifier for a
/// side-effect import. Files with no items are dropped.
pub fn file_items(files: Vec<(String, Vec<Import>)>) -> Vec<FileImports> {
    files
        .into_iter()
        .filter_map(|(file, imports)| {
            let mut items = Vec::new();
            for import in imports {
                let specifier = normalize_specifier(&file, &import.specifier);
                if import.names.is_empty() {
                    items.push(specifier);
                } else {
                    items.extend(import.names.iter().map(|n| format!("{specifier}#{n}")));
                }
            }
            items.sort_unstable();
            items.dedup();
            (!items.is_empty()).then_some(FileImports { file, items })
        })
        .collect()
}

pub fn find_shared_imports(
    files: &[FileImports],
    options: &ImportOptions,
    judge: &dyn Judge,
) -> ImportReport {
    let mut interner = Interner::default();
    let mut names: Vec<&str> = Vec::new();
    let sets: Vec<Vec<u32>> = files
        .iter()
        .map(|f| {
            let mut ids: Vec<u32> = f
                .items
                .iter()
                .map(|item| {
                    let id = interner.id(item.as_str());
                    if id as usize == names.len() {
                        names.push(item);
                    }
                    id
                })
                .collect();
            ids.sort_unstable();
            ids
        })
        .collect();
    let mut postings: Vec<Vec<usize>> = vec![Vec::new(); names.len()];
    for (i, set) in sets.iter().enumerate() {
        for &id in set {
            postings[id as usize].push(i);
        }
    }
    // Smoothed IDF: an item in every file weighs almost nothing, but never exactly zero.
    let n = files.len() as f64;
    let weight: Vec<f64> = postings
        .iter()
        .map(|p| ((n + 1.0) / p.len() as f64).ln())
        .collect();
    let is_rare = |df: usize| df <= RARE_FLOOR || df as f64 <= options.rare_fraction * n;

    // Count shared rare items per pair through the inverted index; common items never pair.
    let mut counts: HashMap<(usize, usize), usize> = HashMap::new();
    for posting in postings.iter().filter(|p| p.len() >= 2 && is_rare(p.len())) {
        for (x, &a) in posting.iter().enumerate() {
            for &b in &posting[x + 1..] {
                *counts.entry((a, b)).or_default() += 1;
            }
        }
    }

    let mut pairs = Vec::new();
    let mut hidden_same_module = 0;
    for ((a, b), rare) in counts {
        if rare < options.min_shared {
            continue;
        }
        let mut shared: Vec<u32> = sets[a]
            .iter()
            .filter(|id| sets[b].binary_search(id).is_ok())
            .copied()
            .collect();
        let mut sources: Vec<&str> = shared
            .iter()
            .filter(|&&id| is_rare(postings[id as usize].len()))
            .map(|&id| specifier_of(names[id as usize]))
            .collect();
        sources.sort_unstable();
        sources.dedup();
        if sources.len() < options.min_shared {
            continue;
        }
        let total = |set: &[u32]| set.iter().map(|&id| weight[id as usize]).sum::<f64>();
        let common = total(&shared);
        let score = weighted_jaccard(total(&sets[a]), total(&sets[b]), common);
        if score < options.threshold {
            continue;
        }
        let tag = judge.verdict(a, b).tag;
        if matches!(tag, Some(Tag::SameModule { .. })) && !options.include_same_module {
            hidden_same_module += 1;
            continue;
        }
        shared.sort_by_key(|&id| (postings[id as usize].len(), names[id as usize]));
        pairs.push(ImportPair {
            a,
            b,
            score,
            shared: shared
                .iter()
                .map(|&id| (names[id as usize].to_string(), postings[id as usize].len()))
                .collect(),
            rare,
            sources: sources.len(),
            tag,
        });
    }
    pairs.sort_by(|x, y| {
        y.score
            .total_cmp(&x.score)
            .then(y.sources.cmp(&x.sources))
            .then((x.a, x.b).cmp(&(y.a, y.b)))
    });
    ImportReport {
        files: files.len(),
        pairs,
        hidden_same_module,
    }
}

/// `react` for `react#useState`; a side-effect `#lib/a.css` (a subpath import) is all specifier.
fn specifier_of(item: &str) -> &str {
    item.rsplit_once('#')
        .filter(|(specifier, _)| !specifier.is_empty())
        .map_or(item, |(specifier, _)| specifier)
}

/// `Σw(A ∩ B) / Σw(A ∪ B)` from the two weight totals and the shared total.
fn weighted_jaccard(a: f64, b: f64, shared: f64) -> f64 {
    let union = a + b - shared;
    if union <= 0.0 { 0.0 } else { shared / union }
}

/// A package specifier as written. A relative one becomes the target's last segment,
/// without its script extension or `/index`: `./a.js`, `../x/a` and `./a/index` are all
/// `./a`. A copied folder imports its own siblings, so their names still match across copies,
/// where resolved paths would differ; collisions on names like `./types` are common, so
/// they weigh little.
fn normalize_specifier(file: &str, specifier: &str) -> String {
    if !specifier.starts_with('.') {
        return specifier.to_string();
    }
    let mut parts: Vec<&str> = file.split('/').collect();
    parts.pop();
    for segment in specifier.split('/') {
        match segment {
            "." | "" => {}
            ".." if parts.last().is_some_and(|p| *p != "..") => {
                parts.pop();
            }
            _ => parts.push(segment),
        }
    }
    let joined = parts.join("/");
    let path = strip_script(&joined);
    let path = path.strip_suffix("/index").unwrap_or(path);
    format!("./{}", path.rsplit('/').next().unwrap_or(path))
}

fn strip_script(path: &str) -> &str {
    const SCRIPT: &[&str] = &[".tsx", ".ts", ".jsx", ".js", ".mts", ".cts", ".mjs", ".cjs"];
    SCRIPT
        .iter()
        .find_map(|ext| path.strip_suffix(ext))
        .unwrap_or(path)
}

// ----- output -----

/// Shared items listed per pair in text output.
const SHOWN_ITEMS: usize = 12;

pub fn render_text(
    files: &[FileImports],
    report: &ImportReport,
    top: usize,
    options: &ImportOptions,
    judge: &dyn Judge,
) -> String {
    let mut out = String::new();
    writeln!(
        out,
        "{} pairs from {} files with imports (threshold {:.2}, rare imports from at least {} specifiers).",
        report.pairs.len(),
        report.files,
        options.threshold,
        options.min_shared
    )
    .ok();
    if let Some(summary) = judge.summary() {
        writeln!(out, "Modules: {summary}.").ok();
    }
    if report.hidden_same_module > 0 {
        writeln!(
            out,
            "Hidden pairs: {} same-module (--include-same-module).",
            report.hidden_same_module
        )
        .ok();
    }
    for (rank, pair) in report.pairs.iter().take(top).enumerate() {
        let tag = pair
            .tag
            .as_ref()
            .map(|t| format!(" — {}", t.describe()))
            .unwrap_or_default();
        writeln!(
            out,
            "\n{}. {:.2} weighted, {} shared, {} rare from {} specifiers{tag}",
            rank + 1,
            pair.score,
            pair.shared.len(),
            pair.rare,
            pair.sources
        )
        .ok();
        for index in [pair.a, pair.b] {
            let module = judge
                .module(index)
                .map(|m| format!("[{m}] "))
                .unwrap_or_default();
            writeln!(
                out,
                "   {module}{} ({} items)",
                files[index].file,
                files[index].items.len()
            )
            .ok();
        }
        let shown: Vec<String> = pair
            .shared
            .iter()
            .take(SHOWN_ITEMS)
            .map(|(item, df)| format!("{item} ({df})"))
            .collect();
        let more = pair.shared.len().saturating_sub(SHOWN_ITEMS);
        let more = if more > 0 {
            format!(", … {more} more")
        } else {
            String::new()
        };
        writeln!(out, "   shared: {}{more}", shown.join(", ")).ok();
    }
    if report.pairs.len() > top {
        writeln!(
            out,
            "\n… {} more; raise --top to see them.",
            report.pairs.len() - top
        )
        .ok();
    }
    out
}

/// Everything, untruncated: `{ summary, pairs }`.
pub fn to_json(files: &[FileImports], report: &ImportReport, judge: &dyn Judge) -> Value {
    let side = |i: usize| {
        json!({
            "file": files[i].file,
            "module": judge.module(i),
            "items": files[i].items.len(),
        })
    };
    let pairs: Vec<Value> = report
        .pairs
        .iter()
        .map(|p| {
            json!({
                "a": side(p.a),
                "b": side(p.b),
                "score": p.score,
                "rare": p.rare,
                "sources": p.sources,
                "shared": p.shared.iter().map(|(item, df)| json!({ "item": item, "files": df })).collect::<Vec<_>>(),
                "tag": p.tag,
            })
        })
        .collect();
    json!({
        "summary": {
            "files": report.files,
            "pairs": report.pairs.len(),
            "modules": judge.summary(),
            "hidden": { "same_module": report.hidden_same_module },
        },
        "pairs": pairs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::NoJudge;

    fn options() -> ImportOptions {
        ImportOptions {
            min_shared: 3,
            threshold: 0.5,
            rare_fraction: 0.01,
            include_same_module: false,
        }
    }

    /// `"react#useState"` → named import; `"./a.css"` → side-effect import.
    fn file(name: &str, items: &[&str]) -> (String, Vec<Import>) {
        let imports = items
            .iter()
            .map(|item| match item.split_once('#') {
                Some((specifier, name)) => Import {
                    specifier: specifier.into(),
                    names: vec![name.into()],
                },
                None => Import {
                    specifier: item.to_string(),
                    names: Vec::new(),
                },
            })
            .collect();
        (name.to_string(), imports)
    }

    #[test]
    fn rare_shared_imports_pair_files_and_common_ones_do_not() {
        let mut input: Vec<(String, Vec<Import>)> = (0..40)
            .map(|i| {
                file(
                    &format!("f{i}.ts"),
                    &["react#useState", &format!("own{i}#x")],
                )
            })
            .collect();
        let chat = [
            "react#useState",
            "chat#Chat",
            "chat/chat.css",
            "icons#Spinner",
            "store#useStore",
        ];
        input.push(file("a.tsx", &chat));
        input.push(file("b.tsx", &chat));
        // Shares rare items from only two specifiers with `a` and `b`: below `min_shared`.
        input.push(file(
            "c.tsx",
            &["react#useState", "chat#Chat", "icons#Spinner"],
        ));
        // Three parts of one compound component are one specifier.
        let dialog = ["ui#Dialog", "ui#DialogTitle", "ui#DialogFooter"];
        input.push(file("d.tsx", &dialog));
        input.push(file("e.tsx", &dialog));
        let files = file_items(input);
        let report = find_shared_imports(&files, &options(), &NoJudge);
        assert_eq!(report.pairs.len(), 1);
        let pair = &report.pairs[0];
        assert_eq!(
            (files[pair.a].file.as_str(), files[pair.b].file.as_str()),
            ("a.tsx", "b.tsx")
        );
        assert_eq!((pair.rare, pair.sources, pair.score), (4, 4, 1.0));
        // Rarest first; `react#useState` is in every file, so it weighs nothing.
        assert_eq!(
            pair.shared.last().unwrap(),
            &("react#useState".to_string(), 43)
        );
    }

    #[test]
    fn score_is_weighted_by_rarity() {
        let mut input: Vec<(String, Vec<Import>)> = (0..20)
            .map(|i| file(&format!("f{i}.ts"), &["lib#common", &format!("own{i}#x")]))
            .collect();
        input.push(file("a.ts", &["r1#x", "r2#x", "r3#x", "lib#common"]));
        input.push(file("b.ts", &["r1#x", "r2#x", "r3#x", "lib#other"]));
        let files = file_items(input);
        let pair = &find_shared_imports(&files, &options(), &NoJudge).pairs[0];
        // `lib#common` (21 files) weighs little against three items in two files each.
        assert!(pair.score > 0.6 && pair.score < 1.0, "{}", pair.score);
        let strict = ImportOptions {
            threshold: 0.95,
            ..options()
        };
        assert!(
            find_shared_imports(&files, &strict, &NoJudge)
                .pairs
                .is_empty()
        );
    }

    #[test]
    fn specifiers_of_items() {
        assert_eq!(specifier_of("react#useState"), "react");
        assert_eq!(specifier_of("./a.css"), "./a.css");
        assert_eq!(specifier_of("#lib/a.css"), "#lib/a.css");
        assert_eq!(specifier_of("#lib/a#X"), "#lib/a");
    }

    #[test]
    fn relative_specifiers_keep_their_last_segment() {
        let norm = |spec| normalize_specifier("pkg/src/ui/a.tsx", spec);
        assert_eq!(norm("@x/ui/button"), "@x/ui/button");
        assert_eq!(norm("./b.js"), "./b");
        assert_eq!(norm("../util/index"), "./util");
        assert_eq!(norm("./a.css"), "./a.css");
        // `.` and `..` name the directory they import.
        assert_eq!(norm(".."), "./src");
        assert_eq!(norm("../../../../x"), "./x");
    }

    #[test]
    fn renders_text_and_json() {
        let input = vec![
            file("a.ts", &["x#1", "y#2", "./z"]),
            file("b.ts", &["x#1", "y#2", "./z"]),
        ];
        let files = file_items(input);
        let report = find_shared_imports(&files, &options(), &NoJudge);
        let text = render_text(&files, &report, 40, &options(), &NoJudge);
        assert!(text.contains("1 pairs from 2 files"), "{text}");
        assert!(
            text.contains("3 shared, 3 rare from 3 specifiers"),
            "{text}"
        );
        assert!(text.contains("shared: ./z (2), x#1 (2), y#2 (2)"), "{text}");
        let json = to_json(&files, &report, &NoJudge);
        assert_eq!(json["pairs"][0]["b"]["file"], "b.ts");
        assert_eq!(json["pairs"][0]["shared"][0]["files"], 2);
    }
}
