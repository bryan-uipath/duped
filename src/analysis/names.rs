//! Files that declare the same names: a ported or forked module shows up as one file pair.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write;

use serde_json::{Value, json};

use super::types::{compact_whitespace, kind_name, normalize_type};
use super::{Interner, Judge, tag_rank};
use crate::modules::Tag;
use crate::record::{Language, Record, TypeKind, format_params};

pub struct NameOptions {
    /// A pair needs at least this many shared names.
    pub min_shared: usize,
    /// Minimum pair score, the sum of its shared names' weights.
    pub min_score: f64,
    /// Names declared in more files than this don't count, e.g. `render`.
    pub max_files: usize,
    /// Report pairs whose two files are in one module.
    pub include_same_module: bool,
}

#[derive(Debug)]
pub struct SharedName {
    pub name: String,
    /// Record indices on each side.
    pub a: usize,
    pub b: usize,
    /// Same signature (functions) or shape (types); `None` when a side is an alias,
    /// whose target isn't recorded.
    pub same: Option<bool>,
    /// Exported on both sides.
    pub exported: bool,
    /// Files declaring the name.
    pub files: usize,
    pub weight: f64,
}

#[derive(Debug)]
pub struct FilePair {
    /// Root-relative paths, `a` first in path order.
    pub a: String,
    pub b: String,
    /// Names each file declares.
    pub a_names: usize,
    pub b_names: usize,
    pub score: f64,
    /// In `a`'s declaration order.
    pub shared: Vec<SharedName>,
    /// Module relationship; `None` when modules aren't tracked.
    pub tag: Option<Tag>,
}

#[derive(Debug)]
pub struct NameReport {
    /// Files with at least one named declaration.
    pub files: usize,
    /// Qualifying pairs, best first.
    pub pairs: Vec<FilePair>,
    /// Qualifying pairs left out because both files share a module.
    pub hidden_same_module: usize,
}

pub fn find_shared_names(
    records: &[Record],
    options: &NameOptions,
    judge: &dyn Judge,
) -> NameReport {
    let mut ids = Interner::default();
    // Each file's `(name id, record index)`, first declaration of each name.
    let mut by_file: BTreeMap<&str, Vec<(u32, usize)>> = BTreeMap::new();
    let mut texts: Vec<String> = Vec::new();
    for (index, record) in records.iter().enumerate() {
        let (language, scope, name) = match record {
            Record::Function(f) => (f.language, f.scope.as_deref(), &f.name),
            Record::Type(t) => (t.language, t.scope.as_deref(), &t.name),
        };
        // Methods belong to their type's shape; `default` stands for an anonymous default export.
        if scope.is_some() || name == "default" {
            continue;
        }
        // A merged `interface X` and `function X` are two names, so like pairs with like.
        let is_type = matches!(record, Record::Type(_));
        let id = ids.id((family(language), is_type, name.clone()));
        if id as usize == texts.len() {
            texts.push(name.clone());
        }
        let names = by_file.entry(record.location().file.as_str()).or_default();
        if !names.iter().any(|&(n, _)| n == id) {
            names.push((id, index));
        }
    }
    let files: Vec<(&str, Vec<(u32, usize)>)> = by_file.into_iter().collect();

    // `postings[name]`: `(file, record)` for each file declaring it, in path order.
    let mut postings: Vec<Vec<(usize, usize)>> = vec![Vec::new(); texts.len()];
    for (f, (_, names)) in files.iter().enumerate() {
        for &(id, record) in names {
            postings[id as usize].push((f, record));
        }
    }
    let weight = idf(files.len());
    // `(file, file)` → `(name, record in the first, record in the second)`.
    type Shared = (usize, usize, usize);
    let mut shared: HashMap<(usize, usize), Vec<Shared>> = HashMap::new();
    for (id, posting) in postings.iter().enumerate() {
        if posting.len() > options.max_files {
            continue;
        }
        for (k, &(x, rx)) in posting.iter().enumerate() {
            for &(y, ry) in &posting[k + 1..] {
                shared.entry((x, y)).or_default().push((id, rx, ry));
            }
        }
    }

    let mut pairs = Vec::new();
    let mut hidden_same_module = 0;
    for ((x, y), names) in shared {
        if names.len() < options.min_shared {
            continue;
        }
        let (fa, fb) = (&files[x], &files[y]);
        let mut list: Vec<SharedName> = names
            .into_iter()
            .map(|(id, a, b)| {
                let same = signature(&records[a])
                    .zip(signature(&records[b]))
                    .map(|(x, y)| x == y);
                let exported = is_exported(&records[a]) && is_exported(&records[b]);
                let count = postings[id].len();
                let bonus = |on: bool| if on { 1.5 } else { 1.0 };
                SharedName {
                    name: texts[id].clone(),
                    a,
                    b,
                    same,
                    exported,
                    files: count,
                    weight: weight(count) * bonus(same == Some(true)) * bonus(exported),
                }
            })
            .collect();
        let score: f64 = list.iter().map(|s| s.weight).sum();
        if score < options.min_score {
            continue;
        }
        let tag = judge.tag(fa.1[0].1, fb.1[0].1);
        if matches!(tag, Some(Tag::SameModule { .. })) && !options.include_same_module {
            hidden_same_module += 1;
            continue;
        }
        list.sort_by_key(|s| records[s.a].location().start_line);
        pairs.push(FilePair {
            a: fa.0.to_string(),
            b: fb.0.to_string(),
            a_names: fa.1.len(),
            b_names: fb.1.len(),
            score,
            shared: list,
            tag,
        });
    }
    // Highest score first; the module tag only breaks ties, since a boundary copy is still a copy.
    pairs.sort_by(|x, y| {
        y.score
            .total_cmp(&x.score)
            .then(y.shared.len().cmp(&x.shared.len()))
            .then(tag_rank(&x.tag).cmp(&tag_rank(&y.tag)))
            .then((&x.a, &x.b).cmp(&(&y.a, &y.b)))
    });
    NameReport {
        files: files.len(),
        pairs,
        hidden_same_module,
    }
}

/// TypeScript and JavaScript share names; other languages only match themselves.
pub(super) fn family(language: Language) -> Language {
    match language {
        Language::JavaScript => Language::TypeScript,
        other => other,
    }
}

/// Inverse document frequency over `files`, scaled so a name in 2 files weighs 1:
/// `ln(files / count) / ln(files / 2)`.
fn idf(files: usize) -> impl Fn(usize) -> f64 {
    let n = files as f64;
    move |count| {
        if files <= 2 {
            1.0
        } else {
            ((n / count as f64).ln() / (n / 2.0).ln()).clamp(0.0, 1.0)
        }
    }
}

fn is_exported(record: &Record) -> bool {
    match record {
        Record::Function(f) => f.exported,
        Record::Type(t) => t.exported,
    }
}

/// A comparison key: parameter and return types for a function (spacing aside, so
/// `T | null` and `T` differ), kind and normalised field types (in any order) for a type;
/// `None` for an alias. An unannotated parameter compares by name; `...xs: T[]` keeps `...`.
fn signature(record: &Record) -> Option<String> {
    match record {
        Record::Function(f) => {
            let params: Vec<String> = f
                .params
                .iter()
                .map(|p| {
                    let rest = if p.ty.is_some() && p.name.starts_with("...") {
                        "..."
                    } else {
                        ""
                    };
                    let ty = compact_whitespace(p.ty.as_deref().unwrap_or(&p.name));
                    let optional = if p.optional { "?" } else { "" };
                    format!("{rest}{ty}{optional}")
                })
                .collect();
            let returns = f
                .returns
                .as_deref()
                .map(compact_whitespace)
                .unwrap_or_default();
            Some(format!("fn({})->{returns}", params.join(",")))
        }
        Record::Type(t) if t.kind == TypeKind::Alias => None,
        Record::Type(t) => {
            let mut fields: Vec<String> = t
                .fields
                .iter()
                .map(|f| {
                    let ty = f.ty.as_deref().map(normalize_type).unwrap_or_default();
                    format!("{}:{ty}", f.name)
                })
                .collect();
            fields.sort_unstable();
            Some(format!("{}{{{}}}", kind_name(t.kind), fields.join(",")))
        }
    }
}

/// `function`, or the type's kind, e.g. `interface`.
fn kind_of(record: &Record) -> String {
    match record {
        Record::Type(t) => kind_name(t.kind),
        Record::Function(_) => "function".to_string(),
    }
}

// ----- output -----

/// Human-readable report: ranked file pairs, each with its shared names.
pub fn render_text(
    records: &[Record],
    report: &NameReport,
    top: usize,
    options: &NameOptions,
    judge: &dyn Judge,
) -> String {
    let mut out = String::new();
    writeln!(
        out,
        "{} file pairs from {} files (min shared {}, min score {:.1}, names in at most {} files).",
        report.pairs.len(),
        report.files,
        options.min_shared,
        options.min_score,
        options.max_files
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
            "\n{}. score {:.2}: {} shared names (of {} and {}){tag}",
            rank + 1,
            pair.score,
            pair.shared.len(),
            pair.a_names,
            pair.b_names
        )
        .ok();
        let first = &pair.shared[0];
        for (file, index) in [(&pair.a, first.a), (&pair.b, first.b)] {
            let module = judge
                .module(index)
                .map(|m| format!("[{m}] "))
                .unwrap_or_default();
            writeln!(out, "   {module}{file}").ok();
        }
        for s in &pair.shared {
            let (la, lb) = (
                records[s.a].location().start_line,
                records[s.b].location().start_line,
            );
            let exported = if s.exported { ", exported" } else { "" };
            let (a, b) = (display(&records[s.a]), display(&records[s.b]));
            let shape = if a == b { a } else { format!("{a}  vs  {b}") };
            // `=` same signature or shape, `~` different, `?` unknown (an alias).
            let mark = match s.same {
                Some(true) => "=",
                Some(false) => "~",
                None => "?",
            };
            writeln!(out, "   {mark} {} (:{la} :{lb}{exported}): {shape}", s.name).ok();
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
pub fn to_json(records: &[Record], report: &NameReport, judge: &dyn Judge) -> Value {
    let side = |index: usize| {
        json!({
            "line": records[index].location().start_line,
            "kind": kind_of(&records[index]),
            "signature": describe(&records[index]),
        })
    };
    let pairs: Vec<Value> = report
        .pairs
        .iter()
        .map(|p| {
            let first = &p.shared[0];
            json!({
                "a": { "file": p.a, "module": judge.module(first.a), "names": p.a_names },
                "b": { "file": p.b, "module": judge.module(first.b), "names": p.b_names },
                "score": p.score,
                "tag": p.tag,
                "shared": p.shared.iter().map(|s| json!({
                    "name": s.name,
                    "same": s.same,
                    "exported": s.exported,
                    "files": s.files,
                    "weight": s.weight,
                    "a": side(s.a),
                    "b": side(s.b),
                })).collect::<Vec<_>>(),
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

/// `(id: string) => User` for a function, `interface (4 fields)` for a type.
fn describe(record: &Record) -> String {
    let text = match record {
        Record::Function(f) => format!(
            "({}) => {}",
            format_params(&f.params),
            f.returns.as_deref().unwrap_or("?")
        ),
        Record::Type(t) => format!("{} ({} fields)", kind_of(record), t.fields.len()),
    };
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// [`describe`], at most 60 chars.
fn display(record: &Record) -> String {
    let text = describe(record);
    match text.char_indices().nth(60) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::NoJudge;
    use crate::record::{Field, FieldKind, FunctionRecord, Location, Param, TypeRecord};

    fn options() -> NameOptions {
        NameOptions {
            min_shared: 2,
            min_score: 2.5,
            max_files: 10,
            include_same_module: false,
        }
    }

    fn at(file: &str, line: usize) -> Location {
        Location {
            file: file.to_string(),
            start_line: line,
            end_line: line,
        }
    }

    /// `params` as `name: type` strings.
    fn func(file: &str, name: &str, params: &[&str], returns: &str, exported: bool) -> Record {
        let params = params
            .iter()
            .map(|p| {
                let (name, ty) = p.split_once(':').unwrap();
                Param {
                    name: name.trim().to_string(),
                    ty: Some(ty.trim().to_string()),
                    optional: false,
                    fields: Vec::new(),
                }
            })
            .collect();
        Record::Function(FunctionRecord {
            language: Language::TypeScript,
            name: name.to_string(),
            scope: None,
            params,
            returns: Some(returns.to_string()),
            exported,
            location: at(file, 1),
            doc: None,
            body: Vec::new(),
        })
    }

    fn ty(file: &str, name: &str, kind: TypeKind, fields: &[&str]) -> Record {
        let fields = fields
            .iter()
            .map(|f| Field {
                name: f.to_string(),
                ty: Some("string".to_string()),
                optional: false,
                kind: FieldKind::Property,
            })
            .collect();
        Record::Type(TypeRecord {
            language: Language::TypeScript,
            name: name.to_string(),
            kind,
            scope: None,
            fields,
            extends: Vec::new(),
            exported: true,
            location: at(file, 1),
            doc: None,
        })
    }

    /// A ported module: one identical exported function, two private helpers that drifted.
    fn ported() -> Vec<Record> {
        vec![
            func("host/panel.ts", "resetClient", &[], "void", true),
            func("host/panel.ts", "getClient", &[], "Client", false),
            func(
                "host/panel.ts",
                "Content",
                &["p: { id: string }"],
                "Node",
                false,
            ),
            func("host/panel.ts", "HostPanel", &[], "Node", true),
            func("ext/extPanel.ts", "resetClient", &[], "void", true),
            func(
                "ext/extPanel.ts",
                "getClient",
                &["auth: Auth"],
                "Client",
                false,
            ),
            func(
                "ext/extPanel.ts",
                "Content",
                &["p: { id: string; auth: Auth }"],
                "Node",
                false,
            ),
            func("ext/extPanel.ts", "ExtPanel", &[], "Node", true),
        ]
    }

    #[test]
    fn reports_a_ported_module_as_one_file_pair() {
        let records = ported();
        let report = find_shared_names(&records, &options(), &NoJudge);
        assert_eq!(report.pairs.len(), 1);
        let pair = &report.pairs[0];
        assert_eq!(
            (pair.a.as_str(), pair.b.as_str(), pair.a_names, pair.b_names),
            ("ext/extPanel.ts", "host/panel.ts", 4, 4)
        );
        let names: Vec<(&str, Option<bool>, bool)> = pair
            .shared
            .iter()
            .map(|s| (s.name.as_str(), s.same, s.exported))
            .collect();
        assert_eq!(
            names,
            vec![
                ("resetClient", Some(true), true),
                ("getClient", Some(false), false),
                ("Content", Some(false), false),
            ]
        );
        // Two files only, so every name weighs 1 before bonuses: 1.5 × 1.5 + 1 + 1.
        assert_eq!(pair.score, 4.25);
    }

    #[test]
    fn one_shared_name_or_a_low_score_is_not_a_pair() {
        let records = vec![
            func("a.ts", "render", &[], "Node", true),
            func("a.ts", "onlyA", &[], "void", true),
            func("b.ts", "render", &[], "Node", true),
            func("b.ts", "onlyB", &[], "void", true),
        ];
        assert!(
            find_shared_names(&records, &options(), &NoJudge)
                .pairs
                .is_empty()
        );
        // Two private names that differ score 2, under the default 2.5.
        let records = vec![
            func("a.ts", "x", &[], "A", false),
            func("a.ts", "y", &[], "A", false),
            func("b.ts", "x", &[], "B", false),
            func("b.ts", "y", &[], "B", false),
        ];
        let report = find_shared_names(&records, &options(), &NoJudge);
        assert!(report.pairs.is_empty());
        let loose = NameOptions {
            min_score: 2.0,
            ..options()
        };
        assert_eq!(find_shared_names(&records, &loose, &NoJudge).pairs.len(), 1);
    }

    #[test]
    fn common_names_methods_and_default_exports_do_not_count() {
        let mut records = ported();
        // `getClient` and `Content` in 11 files are too common to count, leaving one shared name.
        for i in 0..9 {
            records.push(func(&format!("other{i}.ts"), "getClient", &[], "C", true));
            records.push(func(&format!("other{i}.ts"), "Content", &[], "C", true));
        }
        assert!(
            find_shared_names(&records, &options(), &NoJudge)
                .pairs
                .is_empty()
        );

        let mut method = func("b.ts", "start", &[], "void", true);
        if let Record::Function(f) = &mut method {
            f.scope = Some("Server".into());
        }
        let records = vec![
            func("a.ts", "default", &[], "void", true),
            func("a.ts", "start", &[], "void", true),
            func("b.ts", "default", &[], "void", true),
            method,
        ];
        let report = find_shared_names(&records, &options(), &NoJudge);
        assert!(report.pairs.is_empty());
        assert_eq!(report.files, 1);
    }

    #[test]
    fn compares_shapes_and_leaves_aliases_unknown() {
        let records = vec![
            ty("a.ts", "User", TypeKind::Interface, &["id", "name"]),
            ty("a.ts", "Role", TypeKind::Interface, &["id"]),
            ty("a.ts", "Id", TypeKind::Alias, &[]),
            ty("b.ts", "User", TypeKind::Type, &["name", "id"]),
            ty("b.ts", "Role", TypeKind::Interface, &["id", "level"]),
            ty("b.ts", "Id", TypeKind::Alias, &[]),
        ];
        let report = find_shared_names(&records, &options(), &NoJudge);
        let same: Vec<(&str, Option<bool>)> = report.pairs[0]
            .shared
            .iter()
            .map(|s| (s.name.as_str(), s.same))
            .collect();
        // An interface and an object alias with the same fields are different kinds.
        assert_eq!(
            same,
            vec![("User", Some(false)), ("Role", Some(false)), ("Id", None)]
        );
    }

    #[test]
    fn function_signatures_keep_nullish_types() {
        let a = func("a.ts", "f", &["v: unknown"], "string | null", true);
        let b = func("b.ts", "f", &["v:unknown"], "string|null", true);
        let c = func("c.ts", "f", &["v: unknown"], "string", true);
        assert_eq!(signature(&a), signature(&b));
        assert_ne!(signature(&a), signature(&c));
        // Quoted spaces count; a rest parameter differs from an array parameter.
        let quoted = func("a.ts", "f", &["v: 'a b'"], "void", true);
        let joined = func("b.ts", "f", &["v: 'ab'"], "void", true);
        assert_ne!(signature(&quoted), signature(&joined));
        let rest = func("a.ts", "f", &["...v: string[]"], "void", true);
        let array = func("b.ts", "f", &["v: string[]"], "void", true);
        assert_ne!(signature(&rest), signature(&array));
        // An unannotated destructured parameter compares by its pattern, spacing aside.
        let untyped = |file: &str, name: &str| {
            let mut record = func(file, "f", &[], "void", true);
            if let Record::Function(f) = &mut record {
                f.params.push(Param {
                    name: name.to_string(),
                    ty: None,
                    optional: false,
                    fields: Vec::new(),
                });
            }
            record
        };
        assert_eq!(
            signature(&untyped("a.js", "{ a, b }")),
            signature(&untyped("b.js", "{a,b}"))
        );
    }

    #[test]
    fn merged_declarations_pair_like_with_like() {
        // `interface Button` and `function Button` in opposite orders: both still match.
        let records = vec![
            ty("a.ts", "Button", TypeKind::Interface, &["x"]),
            func("a.ts", "Button", &[], "void", true),
            func("b.ts", "Button", &[], "void", true),
            ty("b.ts", "Button", TypeKind::Interface, &["x"]),
        ];
        let report = find_shared_names(&records, &options(), &NoJudge);
        let same: Vec<Option<bool>> = report.pairs[0].shared.iter().map(|s| s.same).collect();
        assert_eq!(same, vec![Some(true), Some(true)]);
    }

    #[test]
    fn rare_names_weigh_more() {
        let weight = idf(100);
        assert_eq!(weight(2), 1.0);
        assert!(weight(10) < weight(3));
        assert_eq!(idf(2)(2), 1.0);
    }

    #[test]
    fn javascript_pairs_with_typescript_but_not_python() {
        let mut records = ported();
        for record in &mut records[4..] {
            if let Record::Function(f) = record {
                f.language = Language::JavaScript;
            }
        }
        assert_eq!(
            find_shared_names(&records, &options(), &NoJudge)
                .pairs
                .len(),
            1
        );
        for record in &mut records[4..] {
            if let Record::Function(f) = record {
                f.language = Language::Python;
            }
        }
        assert!(
            find_shared_names(&records, &options(), &NoJudge)
                .pairs
                .is_empty()
        );
    }

    #[test]
    fn renders_text_and_json() {
        let records = ported();
        let report = find_shared_names(&records, &options(), &NoJudge);
        let text = render_text(&records, &report, 40, &options(), &NoJudge);
        assert!(text.contains("1 file pairs from 2 files"), "{text}");
        assert!(
            text.contains("1. score 4.25: 3 shared names (of 4 and 4)"),
            "{text}"
        );
        assert!(
            text.contains("= resetClient (:1 :1, exported): () => void"),
            "{text}"
        );
        assert!(
            text.contains("~ getClient (:1 :1): (auth: Auth) => Client  vs  () => Client"),
            "{text}"
        );
        let json = to_json(&records, &report, &NoJudge);
        assert_eq!(json["summary"]["pairs"], 1);
        assert_eq!(json["pairs"][0]["shared"][1]["same"], false);
        assert_eq!(
            json["pairs"][0]["shared"][1]["b"]["signature"],
            "() => Client"
        );

        // JSON signatures are never truncated.
        let long = "p: { first: string; second: string; third: string; fourth: string }";
        let records = vec![
            func("a.ts", "f", &[long], "void", true),
            func("a.ts", "g", &[], "void", true),
            func("b.ts", "f", &[long], "void", true),
            func("b.ts", "g", &[], "void", true),
        ];
        let report = find_shared_names(&records, &options(), &NoJudge);
        let json = to_json(&records, &report, &NoJudge);
        assert_eq!(
            json["pairs"][0]["shared"][0]["a"]["signature"],
            format!("({long}) => void")
        );
    }
}
