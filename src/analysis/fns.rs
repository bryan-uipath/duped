//! Functions whose signatures share most of their parameter names, whatever they are called.

use std::collections::HashMap;
use std::fmt::Write;

use serde_json::{Value, json};

use super::types::normalize_type;
use super::{Interner, Judge, pair_notes, seed_pairs, tag_rank};
use crate::modules::Tag;
use crate::record::{FunctionRecord, Param, Record, TypeKind, qualified};

pub struct FnOptions {
    /// Functions with fewer parameter names, counting a destructured object's keys, are skipped.
    pub min_params: usize,
    /// A pair needs at least this many shared parameter names.
    pub min_shared: usize,
    /// Minimum IDF-weighted Jaccard similarity of parameter names.
    pub threshold: f64,
    /// Skip pairs whose shared names all appear together on more than this many candidates:
    /// conventions such as `(a, b)` comparators, not copies.
    pub max_sharing: usize,
    /// Report pairs whose two sides are in one module.
    pub include_same_module: bool,
    /// Report pairs acknowledged as deliberate.
    pub include_acknowledged: bool,
    /// Report same-name methods whose classes share a base, e.g. two implementations of one interface.
    pub include_implementations: bool,
    /// Report pairs where one function calls the other, e.g. a hook around a fetcher.
    pub include_wrappers: bool,
}

/// A name only stops seeding pairs once it is on more functions than this, whatever the fraction.
const COMMON_NAME_FLOOR: usize = 50;
/// Names on more than this fraction of functions, e.g. `props`, don't seed pairs; they still score.
const COMMON_NAME_FRACTION: f64 = 0.01;

#[derive(Debug)]
pub struct FnPair {
    /// Indices into the analysed records; `a` comes first in file order.
    pub a: usize,
    pub b: usize,
    /// IDF-weighted Jaccard similarity of parameter names.
    pub similarity: f64,
    /// The same, counting a shared name only when its types match, plus the return type.
    pub typed: f64,
    /// Shared parameter names, as `a` writes them.
    pub shared: Vec<String>,
    /// Candidates that have every shared name, these two included.
    pub sharing: usize,
    pub tag: Option<Tag>,
    pub acknowledged: Option<String>,
}

#[derive(Debug)]
pub struct FnReport {
    /// Functions with enough parameters to compare.
    pub candidates: usize,
    /// Qualifying pairs, best first.
    pub pairs: Vec<FnPair>,
    pub hidden_same_module: usize,
    pub hidden_acknowledged: usize,
    pub hidden_implementations: usize,
    pub hidden_wrappers: usize,
}

struct Candidate<'r> {
    record: usize,
    file: &'r str,
    /// Sorted lower-cased name ids, so `pageSize` and `pagesize` match.
    names: Vec<u32>,
    /// Normalised type id of each entry in `names`; `None` when unannotated.
    types: Vec<Option<u32>>,
    /// Each entry in `names` as written.
    labels: Vec<&'r str>,
    returns: Option<u32>,
}

/// `source` reads a record's file, by its `file` path, to spot wrappers.
pub fn find_duplicate_fns<'r>(
    records: &'r [Record],
    options: &FnOptions,
    judge: &dyn Judge,
    source: &dyn Fn(&str) -> Option<String>,
) -> FnReport {
    let mut name_ids = Interner::default();
    let mut type_ids = Interner::default();
    let mut all = Vec::new();
    for (index, record) in records.iter().enumerate() {
        let Record::Function(f) = record else {
            continue;
        };
        let mut keyed: Vec<(u32, Option<u32>, &str)> = Vec::new();
        for (name, ty) in terms(f) {
            let id = name_ids.id(name.to_lowercase());
            if keyed.iter().all(|k| k.0 != id) {
                keyed.push((id, ty.map(|t| type_ids.id(normalize_type(t))), name));
            }
        }
        if keyed.is_empty() {
            continue;
        }
        keyed.sort_unstable_by_key(|k| k.0);
        let returns = f.returns.as_deref().map(|r| type_ids.id(normalize_type(r)));
        all.push(Candidate {
            record: index,
            file: &f.location.file,
            names: keyed.iter().map(|k| k.0).collect(),
            types: keyed.iter().map(|k| k.1).collect(),
            labels: keyed.iter().map(|k| k.2).collect(),
            returns,
        });
    }
    // Weights come from every function with a parameter, not just the candidates.
    let name_weight = idf(all.iter().flat_map(|c| c.names.iter().copied()), all.len());
    let return_weight = idf(
        all.iter().filter_map(|c| c.returns),
        all.iter().filter(|c| c.returns.is_some()).count(),
    );
    let candidates: Vec<&Candidate> = all
        .iter()
        .filter(|c| c.names.len() >= options.min_params)
        .collect();

    let mut postings: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, c) in candidates.iter().enumerate() {
        for &id in &c.names {
            postings.entry(id).or_default().push(i);
        }
    }
    let weights = Weights {
        name: name_weight,
        returns: return_weight,
        postings,
    };
    let bases = class_bases(records);
    let mut pairs = Vec::new();
    let (mut hidden_same_module, mut hidden_acknowledged) = (0, 0);
    let (mut hidden_implementations, mut hidden_wrappers) = (0, 0);
    let mut sources: HashMap<&'r str, String> = HashMap::new();
    let sets: Vec<&[u32]> = candidates.iter().map(|c| c.names.as_slice()).collect();
    seed_pairs(&sets, COMMON_NAME_FLOOR, COMMON_NAME_FRACTION, |i, j| {
        let (a, b) = (candidates[i], candidates[j]);
        if a.file == b.file {
            return;
        }
        let Some(mut pair) = score_pair(a, b, &weights, options) else {
            return;
        };
        if !options.include_implementations && implements_same(records, a.record, b.record, &bases)
        {
            hidden_implementations += 1;
            return;
        }
        if !options.include_wrappers {
            let (fa, fb) = (fn_at(records, a.record), fn_at(records, b.record));
            for f in [fa, fb] {
                let file = f.location.file.as_str();
                sources
                    .entry(file)
                    .or_insert_with(|| source(file).unwrap_or_default());
            }
            let text = |f: &FunctionRecord| sources[f.location.file.as_str()].as_str();
            if forwards(fa, text(fa), fb) || forwards(fb, text(fb), fa) {
                hidden_wrappers += 1;
                return;
            }
        }
        let verdict = judge.verdict(pair.a, pair.b);
        if matches!(verdict.tag, Some(Tag::SameModule { .. })) && !options.include_same_module {
            hidden_same_module += 1;
        } else if verdict.acknowledged.is_some() && !options.include_acknowledged {
            hidden_acknowledged += 1;
        } else {
            pair.tag = verdict.tag;
            pair.acknowledged = verdict.acknowledged;
            pairs.push(pair);
        }
    });
    // Most actionable tag first, then most similar.
    pairs.sort_by(|x, y| {
        tag_rank(&x.tag)
            .cmp(&tag_rank(&y.tag))
            .then(y.similarity.total_cmp(&x.similarity))
            .then(y.typed.total_cmp(&x.typed))
            .then(y.shared.len().cmp(&x.shared.len()))
            .then((x.a, x.b).cmp(&(y.a, y.b)))
    });
    FnReport {
        candidates: candidates.len(),
        pairs,
        hidden_same_module,
        hidden_acknowledged,
        hidden_implementations,
        hidden_wrappers,
    }
}

/// `caller` forwards to `callee`: it calls it, directly or through an import alias such as
/// `import { load as loadShared }`, passing every one of its own parameters; or it is a
/// bodiless `declare` (or `.d.ts`) signature of the same name.
fn forwards(caller: &FunctionRecord, source: &str, callee: &FunctionRecord) -> bool {
    let (start, end) = (caller.location.start_line, caller.location.end_line);
    let lines: Vec<&str> = source
        .lines()
        .skip(start - 1)
        .take(end + 1 - start)
        .collect();
    let text = lines.join("\n");
    let Some(at) = text.find(&caller.name) else {
        return false;
    };
    let (head, body) = (&text[..at], &text[at + caller.name.len()..]);
    let declared =
        caller.location.file.ends_with(".d.ts") || head.split_whitespace().any(|w| w == "declare");
    if declared {
        return caller.name == callee.name;
    }
    let params: Vec<&str> = flat_params(caller)
        .map(|p| p.name.trim_start_matches(['.', '*']))
        .collect();
    let forwarded = |args: &str| params.iter().all(|p| mentions(args, p).next().is_some());
    std::iter::once(callee.name.as_str())
        .chain(import_aliases(source, &callee.name))
        .any(|name| {
            // A bare call to one's own name is recursion; a same-name call through `x.` delegates.
            let own = name == caller.name;
            body.match_indices(name).any(|(i, _)| {
                let before = body[..i].chars().next_back();
                let qualified = before == Some('.');
                let bounded = if own {
                    qualified
                } else {
                    !before.is_some_and(is_ident)
                };
                bounded && call_args(&body[i + name.len()..]).is_some_and(forwarded)
            })
        })
}

/// `( a, f(b) ) …` → ` a, f(b) `: a call's argument text, when the parentheses close.
fn call_args(rest: &str) -> Option<&str> {
    let rest = rest.trim_start().strip_prefix('(')?;
    let mut depth = 1;
    for (i, c) in rest.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&rest[..i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Local names for `name` from `import { name as alias }` specifiers in `source`.
fn import_aliases<'t>(source: &'t str, name: &'t str) -> impl Iterator<Item = &'t str> {
    mentions(source, name).filter_map(move |rest| {
        let at = source.len() - rest.len();
        // Inside an import statement: no `from` or `;` since its `import` keyword.
        let statement = &source[source[..at].rfind("import")?..at];
        if statement.contains("from") || statement.contains(';') {
            return None;
        }
        let alias = rest
            .strip_prefix(" as ")?
            .split(|c: char| !is_ident(c))
            .next()?;
        (!alias.is_empty()).then_some(alias)
    })
}

/// The text after each whole-word occurrence of `name` in `text`.
fn mentions<'t>(text: &'t str, name: &'t str) -> impl Iterator<Item = &'t str> {
    text.match_indices(name).filter_map(move |(i, _)| {
        let rest = &text[i + name.len()..];
        let bounded = !text[..i].chars().next_back().is_some_and(is_ident)
            && !rest.chars().next().is_some_and(is_ident);
        bounded.then_some(rest)
    })
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// Base classes and interfaces of each class, keyed by file and `Scope.Name`, generics dropped.
fn class_bases(records: &[Record]) -> HashMap<(&str, String), Vec<&str>> {
    records
        .iter()
        .filter_map(|r| match r {
            Record::Type(t) if t.kind == TypeKind::Class => Some((
                (
                    t.location.file.as_str(),
                    qualified(t.scope.as_deref(), &t.name),
                ),
                t.extends.iter().map(|e| base_name(e)).collect(),
            )),
            _ => None,
        })
        .collect()
}

/// Two methods of one name whose classes share a base, or where one class extends the other:
/// implementations or overrides of one contract, not copies.
fn implements_same<'r>(
    records: &'r [Record],
    a: usize,
    b: usize,
    bases: &HashMap<(&'r str, String), Vec<&'r str>>,
) -> bool {
    let (fa, fb) = (fn_at(records, a), fn_at(records, b));
    let (Some(sa), Some(sb)) = (fa.scope.as_deref(), fb.scope.as_deref()) else {
        return false;
    };
    if fa.name != fb.name {
        return false;
    }
    let (ba, bb) = (
        bases_of(bases, &fa.location.file, sa),
        bases_of(bases, &fb.location.file, sb),
    );
    let class = |scope: &'r str| scope.rsplit('.').next().unwrap_or(scope);
    ba.iter().any(|x| bb.contains(x)) || ba.contains(&class(sb)) || bb.contains(&class(sa))
}

fn bases_of<'m>(
    bases: &'m HashMap<(&str, String), Vec<&str>>,
    file: &'m str,
    scope: &str,
) -> &'m [&'m str] {
    bases
        .get(&(file, scope.to_string()))
        .map_or(&[], Vec::as_slice)
}

/// `Store<Item>` → `Store`; `ns.Base` → `Base`.
fn base_name(written: &str) -> &str {
    let name = written.split('<').next().unwrap_or(written).trim();
    name.rsplit('.').next().unwrap_or(name)
}

/// Parameters with a destructured object replaced by its keys.
fn flat_params(f: &FunctionRecord) -> impl Iterator<Item = &Param> {
    f.params.iter().flat_map(|p| {
        if p.fields.is_empty() {
            std::slice::from_ref(p)
        } else {
            &p.fields[..]
        }
    })
}

/// Parameter names a caller sees: a destructured object's keys stand in for the object.
/// `...args`, `*args` and `_id` lose their prefix; unnamed patterns are left out.
fn terms(f: &FunctionRecord) -> impl Iterator<Item = (&str, Option<&str>)> {
    flat_params(f)
        .map(|p| (p.name.trim_start_matches(['.', '*', '_']), p.ty.as_deref()))
        .filter(|(name, _)| !name.is_empty() && !name.starts_with(['{', '[', '(']))
}

/// Smoothed IDF scaled to (0, 1]: a key on one of `n` sets weighs 1, a key on all of them
/// `ln 2 / ln(n + 1)`, so ubiquitous names like `props` count for little.
fn idf(keys: impl Iterator<Item = u32>, n: usize) -> HashMap<u32, f64> {
    let mut df: HashMap<u32, usize> = HashMap::new();
    for key in keys {
        *df.entry(key).or_default() += 1;
    }
    let scale = (1.0 + n as f64).ln();
    df.into_iter()
        .map(|(key, count)| (key, (1.0 + n as f64 / count as f64).ln() / scale))
        .collect()
}

/// Corpus statistics a pair is scored against.
struct Weights {
    /// IDF of each parameter name id and return type id.
    name: HashMap<u32, f64>,
    returns: HashMap<u32, f64>,
    /// Ascending candidate indices holding each name id.
    postings: HashMap<u32, Vec<usize>>,
}

impl Weights {
    /// How many candidates have every one of `ids`.
    fn sharing(&self, ids: &[u32]) -> usize {
        let mut lists: Vec<&Vec<usize>> = ids.iter().map(|id| &self.postings[id]).collect();
        lists.sort_by_key(|l| l.len());
        let Some((first, rest)) = lists.split_first() else {
            return 0;
        };
        first
            .iter()
            .filter(|i| rest.iter().all(|l| l.binary_search(i).is_ok()))
            .count()
    }
}

fn score_pair(
    a: &Candidate,
    b: &Candidate,
    weights: &Weights,
    options: &FnOptions,
) -> Option<FnPair> {
    // Walk both sorted name lists; a shared name's types match when equal or either is unknown.
    let (mut i, mut j) = (0, 0);
    let (mut union, mut shared_weight, mut matched_weight) = (0.0, 0.0, 0.0);
    let (mut shared, mut shared_ids) = (Vec::new(), Vec::new());
    while i < a.names.len() || j < b.names.len() {
        let order = match (a.names.get(i), b.names.get(j)) {
            (Some(x), Some(y)) => x.cmp(y),
            (Some(_), None) => std::cmp::Ordering::Less,
            _ => std::cmp::Ordering::Greater,
        };
        let id = if order.is_gt() {
            b.names[j]
        } else {
            a.names[i]
        };
        let w = weights.name[&id];
        union += w;
        match order {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                shared_weight += w;
                let (ta, tb) = (a.types[i], b.types[j]);
                if ta.is_none() || tb.is_none() || ta == tb {
                    matched_weight += w;
                }
                shared.push(a.labels[i].to_string());
                shared_ids.push(id);
                i += 1;
                j += 1;
            }
        }
    }
    // Candidates have at least one name and every weight is positive, so `union > 0`.
    let similarity = shared_weight / union;
    if shared.len() < options.min_shared || similarity < options.threshold {
        return None;
    }
    let sharing = weights.sharing(&shared_ids);
    if sharing > options.max_sharing {
        return None;
    }
    // The return type counts as one more name when both sides declare one.
    let (mut typed_num, mut typed_den) = (matched_weight, union);
    if let (Some(ra), Some(rb)) = (a.returns, b.returns) {
        typed_den += (weights.returns[&ra] + weights.returns[&rb]) / 2.0;
        if ra == rb {
            typed_num += weights.returns[&ra];
        }
    }
    Some(FnPair {
        a: a.record,
        b: b.record,
        similarity,
        typed: typed_num / typed_den,
        shared,
        sharing,
        tag: None,
        acknowledged: None,
    })
}

// ----- output -----

/// Human-readable ranked pairs.
pub fn render_text(
    records: &[Record],
    report: &FnReport,
    top: usize,
    options: &FnOptions,
    judge: &dyn Judge,
) -> String {
    let mut out = String::new();
    writeln!(
        out,
        "{} pairs from {} candidate functions (threshold {:.2}, min params {}, min shared {}, max sharing {}).",
        report.pairs.len(),
        report.candidates,
        options.threshold,
        options.min_params,
        options.min_shared,
        options.max_sharing
    )
    .ok();
    if let Some(summary) = judge.summary() {
        writeln!(out, "Modules: {summary}.").ok();
    }
    let mut hidden = Vec::new();
    if report.hidden_same_module > 0 {
        hidden.push(format!(
            "{} same-module (--include-same-module)",
            report.hidden_same_module
        ));
    }
    if report.hidden_acknowledged > 0 {
        hidden.push(format!(
            "{} acknowledged (--include-acknowledged)",
            report.hidden_acknowledged
        ));
    }
    if report.hidden_implementations > 0 {
        hidden.push(format!(
            "{} implementations of a shared base (--include-implementations)",
            report.hidden_implementations
        ));
    }
    if report.hidden_wrappers > 0 {
        hidden.push(format!(
            "{} wrappers (--include-wrappers)",
            report.hidden_wrappers
        ));
    }
    if !hidden.is_empty() {
        writeln!(out, "Hidden pairs: {}.", hidden.join(", ")).ok();
    }
    for (rank, pair) in report.pairs.iter().take(top).enumerate() {
        writeln!(
            out,
            "\n{}. {:.2} similar, {:.2} typed, shared {}{}",
            rank + 1,
            pair.similarity,
            pair.typed,
            pair.shared.join(", "),
            pair_notes(&pair.tag, &pair.acknowledged)
        )
        .ok();
        for index in [pair.a, pair.b] {
            let f = fn_at(records, index);
            let module = judge
                .module(index)
                .map(|m| format!("[{m}] "))
                .unwrap_or_default();
            writeln!(
                out,
                "   fn {}  {module}{}:{}",
                signature(f),
                f.location.file,
                f.location.start_line
            )
            .ok();
        }
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
pub fn to_json(records: &[Record], report: &FnReport, judge: &dyn Judge) -> Value {
    let member = |index: usize| {
        let f = fn_at(records, index);
        json!({
            "name": f.name,
            "scope": f.scope,
            "language": f.language,
            "module": judge.module(index),
            "file": f.location.file,
            "line": f.location.start_line,
            "params": terms(f).map(|(name, _)| name).collect::<Vec<_>>(),
            "returns": f.returns,
        })
    };
    let pairs: Vec<Value> = report
        .pairs
        .iter()
        .map(|p| {
            json!({
                "a": member(p.a),
                "b": member(p.b),
                "similarity": p.similarity,
                "typed": p.typed,
                "shared": p.shared,
                "sharing": p.sharing,
                "tag": p.tag,
                "acknowledged": p.acknowledged,
            })
        })
        .collect();
    json!({
        "summary": {
            "candidates": report.candidates,
            "pairs": report.pairs.len(),
            "modules": judge.summary(),
            "hidden": {
                "same_module": report.hidden_same_module,
                "acknowledged": report.hidden_acknowledged,
                "implementations": report.hidden_implementations,
                "wrappers": report.hidden_wrappers,
            },
        },
        "pairs": pairs,
    })
}

fn fn_at(records: &[Record], index: usize) -> &FunctionRecord {
    match &records[index] {
        Record::Function(f) => f,
        Record::Type(_) => unreachable!("pairs only reference function records"),
    }
}

/// e.g. `Api.open({ user, compact? }, id): User`; types are left out for brevity.
fn signature(f: &FunctionRecord) -> String {
    let name = |p: &Param| format!("{}{}", p.name, if p.optional { "?" } else { "" });
    let params: Vec<String> = f
        .params
        .iter()
        .map(|p| {
            if p.fields.is_empty() {
                name(p)
            } else {
                let keys: Vec<String> = p.fields.iter().map(name).collect();
                format!("{{ {} }}", keys.join(", "))
            }
        })
        .collect();
    let returns = f
        .returns
        .as_deref()
        .map(|r| format!(": {r}"))
        .unwrap_or_default();
    format!(
        "{}({}){returns}",
        qualified(f.scope.as_deref(), &f.name),
        params.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::NoJudge;
    use crate::record::{Language, Location, TypeRecord};

    fn options() -> FnOptions {
        FnOptions {
            min_params: 2,
            min_shared: 2,
            threshold: 0.6,
            max_sharing: 5,
            include_same_module: false,
            include_acknowledged: false,
            include_implementations: false,
            include_wrappers: false,
        }
    }

    fn param(spec: &str) -> Param {
        let (name, ty) = match spec.split_once(':') {
            Some((n, t)) => (n.trim(), Some(t.trim().to_string())),
            None => (spec, None),
        };
        Param {
            name: name.to_string(),
            ty,
            optional: false,
            fields: Vec::new(),
        }
    }

    /// `"id: string"` → positional; `"{a, b: T}"` → one destructured param.
    fn func(name: &str, file: &str, params: &[&str]) -> Record {
        let params = params
            .iter()
            .map(
                |p| match p.strip_prefix('{').and_then(|p| p.strip_suffix('}')) {
                    Some(inner) => Param {
                        fields: inner.split(',').map(|f| param(f.trim())).collect(),
                        ..param(p)
                    },
                    None => param(p),
                },
            )
            .collect();
        Record::Function(FunctionRecord {
            language: Language::TypeScript,
            name: name.to_string(),
            scope: None,
            params,
            returns: None,
            exported: true,
            location: Location {
                file: file.to_string(),
                start_line: 1,
                end_line: 1,
            },
            doc: None,
            body: Vec::new(),
        })
    }

    fn run(records: &[Record], options: &FnOptions) -> FnReport {
        find_duplicate_fns(records, options, &NoJudge, &|_| None)
    }

    fn names<'r>(records: &'r [Record], pair: &FnPair) -> (&'r str, &'r str) {
        (
            fn_at(records, pair.a).name.as_str(),
            fn_at(records, pair.b).name.as_str(),
        )
    }

    #[test]
    fn destructured_keys_compare_with_each_other_and_positional_params() {
        let records = vec![
            func("Panel", "a.tsx", &["{user: User, compact: boolean}"]),
            func(
                "PortedPanel",
                "b.tsx",
                &["{user: User, locale: Locale, compact: boolean}"],
            ),
            func("open", "c.ts", &["user: User", "compact: boolean"]),
        ];
        // Three functions make every name rare, so a 2-of-3 overlap scores 0.5 here.
        let loose = FnOptions {
            threshold: 0.5,
            ..options()
        };
        let report = run(&records, &loose);
        let mut found: Vec<_> = report.pairs.iter().map(|p| names(&records, p)).collect();
        found.sort_unstable();
        assert_eq!(
            found,
            vec![
                ("Panel", "PortedPanel"),
                ("Panel", "open"),
                ("PortedPanel", "open")
            ]
        );
        let ported = report
            .pairs
            .iter()
            .find(|p| names(&records, p) == ("Panel", "PortedPanel"))
            .unwrap();
        assert_eq!(ported.shared, vec!["user", "compact"]);
        assert!(ported.similarity < 1.0);
    }

    #[test]
    fn ubiquitous_names_weigh_less() {
        // `props` is on 20 functions, `rare` on two; each pair below shares one name of three.
        let mut records: Vec<Record> = (0..20)
            .map(|i| {
                func(
                    &format!("f{i}"),
                    &format!("f{i}.ts"),
                    &["props", &format!("own{i}")],
                )
            })
            .collect();
        records.push(func("x", "x.ts", &["rare", "mine"]));
        records.push(func("y", "y.ts", &["rare", "yours"]));
        let loose = FnOptions {
            threshold: 0.0,
            min_shared: 1,
            max_sharing: 100,
            ..options()
        };
        let report = run(&records, &loose);
        let score = |a: &str, b: &str| {
            report
                .pairs
                .iter()
                .find(|p| names(&records, p) == (a, b))
                .map(|p| p.similarity)
                .unwrap()
        };
        let (rare, common) = (score("x", "y"), score("f0", "f1"));
        assert!(rare > 2.0 * common, "{rare} vs {common}");
    }

    #[test]
    fn skips_small_signatures_same_files_and_conventions() {
        let mut records = vec![
            func("one", "a.ts", &["id"]),
            func("two", "b.ts", &["id"]),
            func("p", "same.ts", &["alpha", "beta"]),
            func("q", "same.ts", &["alpha", "beta"]),
        ];
        // Six comparators: `(a, b)` is a convention, not a copy.
        records.extend((0..6).map(|i| func(&format!("cmp{i}"), &format!("c{i}.ts"), &["a", "b"])));
        let report = run(&records, &options());
        assert!(report.pairs.is_empty(), "{:?}", report.pairs);
        assert_eq!(report.candidates, 8);
        let raised = FnOptions {
            max_sharing: 6,
            ..options()
        };
        assert_eq!(run(&records, &raised).pairs.len(), 15);
    }

    #[test]
    fn typed_counts_param_and_return_types() {
        let mut a = func("a", "a.ts", &["id: string", "count: number"]);
        let mut b = func("b", "b.ts", &["id: string", "count: string"]);
        for (r, ret) in [(&mut a, "Item"), (&mut b, "Item")] {
            if let Record::Function(f) = r {
                f.returns = Some(ret.into());
            }
        }
        let records = vec![a, b];
        let pair = &run(&records, &options()).pairs[0];
        assert_eq!(pair.similarity, 1.0);
        assert!(pair.typed < 1.0 && pair.typed > 0.5, "{}", pair.typed);
    }

    #[test]
    fn hides_implementations_of_one_base() {
        let method = |class: &str, file: &str| {
            let Record::Function(mut f) = func("save", file, &["key", "value"]) else {
                unreachable!()
            };
            f.scope = Some(class.into());
            Record::Function(f)
        };
        let class = |name: &str, file: &str| {
            Record::Type(TypeRecord {
                language: Language::TypeScript,
                name: name.into(),
                kind: TypeKind::Class,
                scope: None,
                fields: Vec::new(),
                extends: vec!["Store<string>".into()],
                exported: true,
                location: Location {
                    file: file.into(),
                    start_line: 1,
                    end_line: 1,
                },
                doc: None,
            })
        };
        let records = vec![
            class("DiskStore", "disk.ts"),
            method("DiskStore", "disk.ts"),
            class("MemoryStore", "memory.ts"),
            method("MemoryStore", "memory.ts"),
        ];
        let report = run(&records, &options());
        assert_eq!((report.pairs.len(), report.hidden_implementations), (0, 1));
        let shown = FnOptions {
            include_implementations: true,
            ..options()
        };
        assert_eq!(run(&records, &shown).pairs.len(), 1);
    }

    #[test]
    fn hides_wrappers_but_not_recursive_copies() {
        let at = |name: &str, file: &str, lines: (usize, usize)| {
            let Record::Function(mut f) = func(name, file, &["path", "value"]) else {
                unreachable!()
            };
            (f.location.start_line, f.location.end_line) = lines;
            Record::Function(f)
        };
        let sources = |file: &str| {
            Some(
                match file {
                    "hook.ts" => "import { load as loadShared } from 'x';\nexport function useLoad(path, value) {\n  return loadShared(path, value);\n}",
                    "client.ts" => "class C {\n  set(path, value) { return this.api.set(path, value); }\n}",
                    "walk.ts" => "export function walk(path, value) {\n  return walk(path.slice(1), value);\n}",
                    "shim.ts" => "export declare function load(path: string, value: unknown): void;",
                    // None of these forward to `load`.
                    "cast.ts" => "const f = load as (p: string) => void;\nexport function copy(path, value) {\n  return (path);\n}",
                    "method.ts" => "export function save(path, value) {\n  return cache.load(path);\n}",
                    "compare.ts" => "export function count(path, value) {\n  let load = 0;\n  return load < path.length;\n}",
                    "decl.ts" => "export declare function other(path: string, value: unknown): void;",
                    "text.ts" => "export function text(path, value) {\n  return 'load(' + path + value;\n}",
                    _ => "",
                }
                .to_string(),
            )
        };
        // Six functions below share `path, value`; allow that many.
        let wide = FnOptions {
            max_sharing: 6,
            ..options()
        };
        let report = |records: &[Record]| find_duplicate_fns(records, &wide, &NoJudge, &sources);
        // Through an import alias, and a bodiless `declare` of the same function.
        let records = vec![
            at("load", "load.ts", (1, 1)),
            at("useLoad", "hook.ts", (2, 4)),
            at("load", "shim.ts", (1, 1)),
        ];
        let r = report(&records);
        assert_eq!((r.pairs.len(), r.hidden_wrappers), (0, 3), "{:?}", r.pairs);
        let shown = FnOptions {
            include_wrappers: true,
            ..options()
        };
        assert_eq!(
            find_duplicate_fns(&records, &shown, &NoJudge, &sources)
                .pairs
                .len(),
            3
        );
        // A declaration of another name, a cast, a partial method call, a comparison and a string.
        let mut records = vec![at("load", "load.ts", (1, 1))];
        records.extend(
            [("copy", "cast.ts", (2, 4)), ("save", "method.ts", (1, 3))]
                .into_iter()
                .chain([("count", "compare.ts", (1, 4)), ("text", "text.ts", (1, 3))])
                .map(|(name, file, lines)| at(name, file, lines)),
        );
        records.push(at("other", "decl.ts", (1, 1)));
        let r = report(&records);
        let with_load = r
            .pairs
            .iter()
            .filter(|p| names(&records, p).0 == "load")
            .count();
        assert_eq!((with_load, r.hidden_wrappers), (5, 0), "{:?}", r.pairs);
        // `this.api.set` delegates; a bare `walk(` is recursion, so the copies stay.
        let records = vec![
            at("set", "client.ts", (2, 2)),
            at("set", "server.ts", (1, 1)),
            at("walk", "walk.ts", (1, 3)),
            at("walk", "walk2.ts", (1, 1)),
        ];
        let r = report(&records);
        let found: Vec<_> = r.pairs.iter().map(|p| names(&records, p)).collect();
        assert!(found.contains(&("walk", "walk")), "{found:?}");
        assert!(!found.contains(&("set", "set")), "{found:?}");
    }

    #[test]
    fn renders_signatures_with_destructured_keys() {
        let records = vec![
            func("Panel", "a.tsx", &["{user, compact}"]),
            func("Ported", "b.tsx", &["{user, compact}"]),
        ];
        let report = run(&records, &options());
        let text = render_text(&records, &report, 40, &options(), &NoJudge);
        assert!(
            text.contains("1 pairs from 2 candidate functions"),
            "{text}"
        );
        assert!(
            text.contains("fn Panel({ user, compact })  a.tsx:1"),
            "{text}"
        );
        let json = to_json(&records, &report, &NoJudge);
        assert_eq!(json["pairs"][0]["b"]["params"], json!(["user", "compact"]));
        assert_eq!(json["pairs"][0]["sharing"], 2);
    }
}
