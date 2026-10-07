//! Types that share most of their properties, whatever they are called.

use std::collections::HashMap;
use std::fmt::Write;

use globset::GlobSet;
use serde::Serialize;
use serde_json::{Value, json};

use super::{Interner, Judge, components, jaccard, pair_notes, seed_pairs, tag_rank};
use crate::modules::{Home, Tag};
use crate::record::{Record, TypeKind, TypeRecord, qualified};
use crate::rules::Rules;

pub struct TypeOptions {
    /// Types with fewer fields are not compared.
    pub min_fields: usize,
    /// A pair needs at least this many shared field names.
    pub min_shared: usize,
    /// Minimum Jaccard similarity of field names.
    pub threshold: f64,
    /// Type names to skip, e.g. `*Props`; matched against both `Name` and `Scope.Name`.
    pub exclude_names: GlobSet,
    /// Field names on more than this fraction of types (and more than
    /// [`COMMON_FIELD_FLOOR`] of them) don't seed candidate pairs, but still score.
    pub common_field_fraction: f64,
    /// Report pairs whose two sides are in one module.
    pub include_same_module: bool,
    /// Report pairs acknowledged as deliberate.
    pub include_acknowledged: bool,
    /// Per-language rules; conventional members don't count toward similarity.
    pub rules: Rules,
}

/// A field name is only "common" once it appears on more types than this, whatever the fraction.
const COMMON_FIELD_FLOOR: usize = 50;

/// How the first type's field names relate to the second's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Relationship {
    /// Same field names and compatible types.
    Exact,
    /// The first has every field of the second, and more.
    Superset,
    /// The second has every field of the first, and more.
    Subset,
    /// Neither contains the other (or same names with different types).
    Overlap,
}

#[derive(Debug)]
pub struct TypePair {
    /// Indices into the analysed records; `a` comes first in file order.
    pub a: usize,
    pub b: usize,
    /// Jaccard similarity of field names.
    pub similarity: f64,
    /// The same, counting a shared field only when its types match.
    pub typed: f64,
    /// Shared field names, in the first type's order.
    pub shared: Vec<String>,
    pub relationship: Relationship,
    /// Module relationship; `None` when modules aren't tracked.
    pub tag: Option<Tag>,
    /// Why the pair is deliberate, when it is.
    pub acknowledged: Option<String>,
}

#[derive(Debug)]
pub struct TypeCluster {
    /// Record indices, in file order.
    pub members: Vec<usize>,
    /// Indices into [`TypeReport::pairs`], best first.
    pub pairs: Vec<usize>,
    /// Field names every member has.
    pub shared: Vec<String>,
    /// The most actionable pair tag.
    pub tag: Option<Tag>,
    /// Where one shared definition could live.
    pub home: Option<Home>,
}

#[derive(Debug)]
pub struct TypeReport {
    /// Types that met the candidate rules.
    pub candidates: usize,
    /// Qualifying pairs, best first.
    pub pairs: Vec<TypePair>,
    /// Groups of types linked by reported pairs, most actionable first.
    pub clusters: Vec<TypeCluster>,
    /// Qualifying pairs left out because both sides share a module.
    pub hidden_same_module: usize,
    /// Qualifying pairs left out because they are acknowledged.
    pub hidden_acknowledged: usize,
}

/// Which kinds are comparable: object shapes with each other, enums and unions with each other.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Group {
    Shape,
    Members,
}

struct Candidate<'r> {
    record: usize,
    group: Group,
    /// Field names in declaration order, deduplicated.
    fields: Vec<&'r str>,
    /// Sorted field-name ids.
    names: Vec<u32>,
    /// Normalised type id of each entry in `names`; `None` when unannotated.
    types: Vec<Option<u32>>,
}

pub fn find_duplicate_types(
    records: &[Record],
    options: &TypeOptions,
    judge: &dyn Judge,
) -> TypeReport {
    let mut name_ids = Interner::default();
    let mut type_ids = Interner::default();
    let mut candidates = Vec::new();
    for (index, record) in records.iter().enumerate() {
        let Record::Type(t) = record else { continue };
        let Some(group) = group_of(t.kind) else {
            continue;
        };
        if is_excluded(t, &options.exclude_names) {
            continue;
        }
        // First declaration of each name wins.
        let mut fields: Vec<&str> = Vec::new();
        let mut keyed: Vec<(u32, Option<u32>)> = Vec::new();
        let rules = options.rules.get(t.language);
        for field in &t.fields {
            if fields.contains(&field.name.as_str()) || rules.is_conventional(&field.name) {
                continue;
            }
            fields.push(&field.name);
            let ty = field
                .ty
                .as_deref()
                .map(|ty| type_ids.id(normalize_type(ty)));
            keyed.push((name_ids.id(field.name.as_str()), ty));
        }
        if fields.len() < options.min_fields {
            continue;
        }
        keyed.sort_unstable_by_key(|&(name, _)| name);
        let (names, types) = keyed.into_iter().unzip();
        candidates.push(Candidate {
            record: index,
            group,
            fields,
            names,
            types,
        });
    }

    let mut pairs = Vec::new();
    let (mut hidden_same_module, mut hidden_acknowledged) = (0, 0);
    for group in [Group::Shape, Group::Members] {
        let members: Vec<&Candidate> = candidates.iter().filter(|c| c.group == group).collect();
        let sets: Vec<&[u32]> = members.iter().map(|c| c.names.as_slice()).collect();
        seed_pairs(
            &sets,
            COMMON_FIELD_FLOOR,
            options.common_field_fraction,
            |i, j| {
                let Some(mut pair) = score_pair(members[i], members[j], options) else {
                    return;
                };
                let verdict = judge.verdict(pair.a, pair.b);
                if matches!(verdict.tag, Some(Tag::SameModule { .. }))
                    && !options.include_same_module
                {
                    hidden_same_module += 1;
                } else if verdict.acknowledged.is_some() && !options.include_acknowledged {
                    hidden_acknowledged += 1;
                } else {
                    pair.tag = verdict.tag;
                    pair.acknowledged = verdict.acknowledged;
                    pairs.push(pair);
                }
            },
        );
    }
    // Most actionable tag first, then most similar.
    pairs.sort_by(|x, y| {
        tag_rank(&x.tag)
            .cmp(&tag_rank(&y.tag))
            .then(y.similarity.total_cmp(&x.similarity))
            .then(y.typed.total_cmp(&x.typed))
            .then(y.shared.len().cmp(&x.shared.len()))
            .then((x.a, x.b).cmp(&(y.a, y.b)))
    });
    let clusters = cluster(&candidates, &pairs, judge);
    TypeReport {
        candidates: candidates.len(),
        pairs,
        clusters,
        hidden_same_module,
        hidden_acknowledged,
    }
}

fn is_excluded(t: &TypeRecord, globs: &GlobSet) -> bool {
    globs.is_match(&t.name)
        || (t.scope.is_some() && globs.is_match(qualified(t.scope.as_deref(), &t.name)))
}

fn score_pair(a: &Candidate, b: &Candidate, options: &TypeOptions) -> Option<TypePair> {
    // Walk both sorted name lists; a shared field's types match when equal or either is unknown.
    let (mut i, mut j, mut shared, mut matched) = (0, 0, 0, 0);
    while i < a.names.len() && j < b.names.len() {
        match a.names[i].cmp(&b.names[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                shared += 1;
                let (ta, tb) = (a.types[i], b.types[j]);
                if ta.is_none() || tb.is_none() || ta == tb {
                    matched += 1;
                }
                i += 1;
                j += 1;
            }
        }
    }
    let similarity = jaccard(a.names.len(), b.names.len(), shared);
    if shared < options.min_shared || similarity < options.threshold {
        return None;
    }
    let typed = jaccard(a.names.len(), b.names.len(), matched);
    let relationship = if shared == a.names.len() && shared == b.names.len() {
        if matched == shared {
            Relationship::Exact
        } else {
            Relationship::Overlap
        }
    } else if shared == b.names.len() {
        Relationship::Superset
    } else if shared == a.names.len() {
        Relationship::Subset
    } else {
        Relationship::Overlap
    };
    let shared_fields = a
        .fields
        .iter()
        .filter(|f| b.fields.contains(f))
        .map(|f| f.to_string())
        .collect();
    // Candidates are in record order and `b` always comes later, so `a` is first in file order.
    Some(TypePair {
        a: a.record,
        b: b.record,
        similarity,
        typed,
        shared: shared_fields,
        relationship,
        tag: None,
        acknowledged: None,
    })
}

fn cluster(candidates: &[Candidate], pairs: &[TypePair], judge: &dyn Judge) -> Vec<TypeCluster> {
    let slot: HashMap<usize, usize> = candidates
        .iter()
        .enumerate()
        .map(|(i, c)| (c.record, i))
        .collect();
    let groups = components(
        candidates.len(),
        pairs.iter().map(|p| (slot[&p.a], slot[&p.b])),
    );
    let mut clusters: Vec<TypeCluster> = groups
        .into_iter()
        .map(|group| {
            let members: Vec<usize> = group.iter().map(|&i| candidates[i].record).collect();
            let first = &candidates[group[0]];
            let shared = first
                .fields
                .iter()
                .filter(|f| group[1..].iter().all(|&i| candidates[i].fields.contains(f)))
                .map(|f| f.to_string())
                .collect();
            // `pairs` is already ranked, so these indices come out best first.
            let cluster_pairs: Vec<usize> = pairs
                .iter()
                .enumerate()
                .filter(|(_, p)| members.binary_search(&p.a).is_ok())
                .map(|(i, _)| i)
                .collect();
            let tag = cluster_pairs.first().and_then(|&i| pairs[i].tag.clone());
            let home = judge.home(&members);
            TypeCluster {
                members,
                pairs: cluster_pairs,
                shared,
                tag,
                home,
            }
        })
        .collect();
    clusters.sort_by(|x, y| {
        let best = |c: &TypeCluster| best_similarity(c, pairs);
        tag_rank(&x.tag)
            .cmp(&tag_rank(&y.tag))
            .then(y.members.len().cmp(&x.members.len()))
            .then(best(y).total_cmp(&best(x)))
            .then(x.members[0].cmp(&y.members[0]))
    });
    clusters
}

fn best_similarity(cluster: &TypeCluster, pairs: &[TypePair]) -> f64 {
    cluster
        .pairs
        .iter()
        .map(|&i| pairs[i].similarity)
        .fold(0.0, f64::max)
}

fn group_of(kind: TypeKind) -> Option<Group> {
    match kind {
        TypeKind::Alias => None,
        TypeKind::Enum | TypeKind::Union => Some(Group::Members),
        _ => Some(Group::Shape),
    }
}

/// A comparison key for a type: `string|undefined`, `string | null` and `string` are equal,
/// as are `Map<string,number>` and `Map<string, number>`. Nullish members are only
/// dropped from a top-level union, so `() => string | undefined` keeps its return type.
pub(crate) fn normalize_type(ty: &str) -> String {
    let compact = compact_whitespace(ty);
    let parts = split_top_level(&compact);
    if parts.len() == 1 {
        return compact;
    }
    let mut kept: Vec<&str> = parts
        .into_iter()
        .filter(|p| !matches!(*p, "undefined" | "null"))
        .collect();
    if kept.is_empty() {
        return compact;
    }
    kept.sort_unstable();
    kept.dedup();
    kept.join("|")
}

/// Drop whitespace unless it separates two identifier characters (`keyof T`),
/// leaving quoted literals untouched.
pub(crate) fn compact_whitespace(text: &str) -> String {
    let ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    let mut out = String::with_capacity(text.len());
    let mut quote = None;
    let mut escaped = false;
    let mut pending_space = false;
    for c in text.chars() {
        if let Some(q) = quote {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        if c.is_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space && ident(c) && out.chars().last().is_some_and(ident) {
            out.push(' ');
        }
        pending_space = false;
        if matches!(c, '\'' | '"' | '`') {
            quote = Some(c);
        }
        out.push(c);
    }
    out
}

/// Split on `|` outside brackets and quotes; a top-level `=>` ends splitting, since
/// everything after it is one return type.
fn split_top_level(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut depth, mut start, mut quote) = (0i32, 0, None);
    let bytes = text.as_bytes();
    for (i, c) in text.char_indices() {
        if let Some(q) = quote {
            if c == q && (i == 0 || bytes[i - 1] != b'\\') {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' | '`' => quote = Some(c),
            '<' | '(' | '[' | '{' => depth += 1,
            '>' if i > 0 && bytes[i - 1] == b'=' => {
                if depth == 0 {
                    return vec![text];
                }
            }
            '>' | ')' | ']' | '}' => depth -= 1,
            '|' if depth == 0 => {
                parts.push(&text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts.retain(|p| !p.is_empty());
    parts
}

// ----- output -----

/// Human-readable report: clusters by default, or a flat pair list.
pub fn render_text(
    records: &[Record],
    report: &TypeReport,
    top: usize,
    pairs_only: bool,
    options: &TypeOptions,
    judge: &dyn Judge,
) -> String {
    let mut out = String::new();
    writeln!(
        out,
        "{} pairs in {} clusters from {} candidate types (threshold {:.2}, min shared {}).",
        report.pairs.len(),
        report.clusters.len(),
        report.candidates,
        options.threshold,
        options.min_shared
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
    if !hidden.is_empty() {
        writeln!(out, "Hidden pairs: {}.", hidden.join(", ")).ok();
    }
    if pairs_only {
        for (rank, pair) in report.pairs.iter().take(top).enumerate() {
            writeln!(
                out,
                "\n{}. {:.2} similar, {:.2} typed, {} shared: {}{}",
                rank + 1,
                pair.similarity,
                pair.typed,
                pair.shared.len(),
                relation_text(records, pair),
                pair_notes(&pair.tag, &pair.acknowledged)
            )
            .ok();
            for index in [pair.a, pair.b] {
                writeln!(out, "   {}", member_line(records, index, judge)).ok();
            }
        }
    } else {
        for (rank, cluster) in report.clusters.iter().take(top).enumerate() {
            let tag = cluster
                .tag
                .as_ref()
                .map(|t| format!(", {}", tag_label(t)))
                .unwrap_or_default();
            writeln!(
                out,
                "\n{}. {} types, best {:.2}{tag}",
                rank + 1,
                cluster.members.len(),
                best_similarity(cluster, &report.pairs)
            )
            .ok();
            if let Some(home) = &cluster.home {
                writeln!(out, "   {}", home.describe()).ok();
            }
            if !cluster.shared.is_empty() {
                writeln!(out, "   shared by all: {}", cluster.shared.join(", ")).ok();
            }
            for &index in &cluster.members {
                writeln!(out, "   {}", member_line(records, index, judge)).ok();
            }
            const SHOWN_PAIRS: usize = 10;
            for &i in cluster.pairs.iter().take(SHOWN_PAIRS) {
                let pair = &report.pairs[i];
                writeln!(
                    out,
                    "   - {:.2} similar, {:.2} typed: {}{}",
                    pair.similarity,
                    pair.typed,
                    relation_text(records, pair),
                    pair_notes(&pair.tag, &pair.acknowledged)
                )
                .ok();
            }
            if cluster.pairs.len() > SHOWN_PAIRS {
                writeln!(
                    out,
                    "   - … {} more pairs",
                    cluster.pairs.len() - SHOWN_PAIRS
                )
                .ok();
            }
        }
    }
    let shown = if pairs_only {
        report.pairs.len()
    } else {
        report.clusters.len()
    };
    if shown > top {
        writeln!(out, "\n… {} more; raise --top to see them.", shown - top).ok();
    }
    out
}

/// Everything, untruncated: `{ summary, pairs, clusters }`.
pub fn to_json(records: &[Record], report: &TypeReport, judge: &dyn Judge) -> Value {
    let pairs: Vec<Value> = report
        .pairs
        .iter()
        .map(|p| {
            json!({
                "a": member_json(records, p.a, judge),
                "b": member_json(records, p.b, judge),
                "similarity": p.similarity,
                "typed": p.typed,
                "relationship": p.relationship,
                "shared": p.shared,
                "tag": p.tag,
                "acknowledged": p.acknowledged,
            })
        })
        .collect();
    let clusters: Vec<Value> = report
        .clusters
        .iter()
        .map(|c| {
            json!({
                "members": c.members.iter().map(|&i| member_json(records, i, judge)).collect::<Vec<_>>(),
                "shared": c.shared,
                "pairs": c.pairs,
                "tag": c.tag,
                "home": c.home,
            })
        })
        .collect();
    json!({
        "summary": {
            "candidates": report.candidates,
            "pairs": report.pairs.len(),
            "clusters": report.clusters.len(),
            "modules": judge.summary(),
            "hidden": { "same_module": report.hidden_same_module, "acknowledged": report.hidden_acknowledged },
        },
        "pairs": pairs,
        "clusters": clusters,
    })
}

pub(crate) fn type_at(records: &[Record], index: usize) -> &TypeRecord {
    match &records[index] {
        Record::Type(t) => t,
        Record::Function(_) => unreachable!("pairs only reference type records"),
    }
}

fn display_name(t: &TypeRecord) -> String {
    qualified(t.scope.as_deref(), &t.name)
}

/// The kind's serialized name, e.g. `interface`.
pub(crate) fn kind_name(kind: TypeKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// The tag's kind, e.g. `importable`.
fn tag_label(tag: &Tag) -> String {
    serde_json::to_value(tag)
        .ok()
        .and_then(|v| v.get("kind")?.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// e.g. `interface Api.User (6 fields)  [@x/core] src/api.ts:12`
fn member_line(records: &[Record], index: usize, judge: &dyn Judge) -> String {
    let t = type_at(records, index);
    let module = judge
        .module(index)
        .map(|m| format!("[{m}] "))
        .unwrap_or_default();
    format!(
        "{} {} ({} fields)  {module}{}:{}",
        kind_name(t.kind),
        display_name(t),
        t.fields.len(),
        t.location.file,
        t.location.start_line
    )
}

/// `A = B` exact, `A ⊃ B` superset, `A ⊂ B` subset, `A ~ B` overlap.
fn relation_text(records: &[Record], pair: &TypePair) -> String {
    let symbol = match pair.relationship {
        Relationship::Exact => "=",
        Relationship::Superset => "⊃",
        Relationship::Subset => "⊂",
        Relationship::Overlap => "~",
    };
    format!(
        "{} {symbol} {}",
        display_name(type_at(records, pair.a)),
        display_name(type_at(records, pair.b))
    )
}

fn member_json(records: &[Record], index: usize, judge: &dyn Judge) -> Value {
    let t = type_at(records, index);
    json!({
        "name": t.name,
        "scope": t.scope,
        "kind": t.kind,
        "language": t.language,
        "module": judge.module(index),
        "file": t.location.file,
        "line": t.location.start_line,
        "fields": t.fields.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::NoJudge;
    use crate::record::{Field, FieldKind, Language, Location};
    use globset::{Glob, GlobSetBuilder};

    fn options() -> TypeOptions {
        let mut builder = GlobSetBuilder::new();
        builder.add(Glob::new("*Props").unwrap());
        TypeOptions {
            min_fields: 3,
            min_shared: 3,
            threshold: 0.7,
            exclude_names: builder.build().unwrap(),
            common_field_fraction: 0.1,
            include_same_module: false,
            include_acknowledged: false,
            rules: Rules::default(),
        }
    }

    /// `"id: string"` → typed property; `"Red"` (no colon) → member without a type.
    fn ty(name: &str, kind: TypeKind, fields: &[&str]) -> Record {
        let fields = fields
            .iter()
            .map(|f| {
                let (name, ty) = match f.split_once(':') {
                    Some((n, t)) => (n.trim(), Some(t.trim().to_string())),
                    None => (*f, None),
                };
                Field {
                    name: name.to_string(),
                    ty,
                    optional: false,
                    kind: FieldKind::Property,
                }
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
            location: Location {
                file: format!("{name}.ts"),
                start_line: 1,
                end_line: 1,
            },
            doc: None,
        })
    }

    fn names<'r>(records: &'r [Record], pair: &TypePair) -> (&'r str, &'r str) {
        (
            type_at(records, pair.a).name.as_str(),
            type_at(records, pair.b).name.as_str(),
        )
    }

    #[test]
    fn scores_exact_and_typed_differences() {
        let records = vec![
            ty(
                "A",
                TypeKind::Interface,
                &["id: string", "name: string", "age: number", "email: string"],
            ),
            ty(
                "B",
                TypeKind::Class,
                &[
                    "id: string",
                    "name: string",
                    "age: number",
                    "email: string | undefined",
                ],
            ),
            ty(
                "C",
                TypeKind::Type,
                &["id: number", "name: string", "age: number", "email: string"],
            ),
        ];
        let report = find_duplicate_types(&records, &options(), &NoJudge);
        let ab = report
            .pairs
            .iter()
            .find(|p| names(&records, p) == ("A", "B"))
            .unwrap();
        assert_eq!(
            (ab.similarity, ab.typed, ab.relationship),
            (1.0, 1.0, Relationship::Exact)
        );
        let ac = report
            .pairs
            .iter()
            .find(|p| names(&records, p) == ("A", "C"))
            .unwrap();
        assert_eq!(
            (ac.similarity, ac.relationship),
            (1.0, Relationship::Overlap)
        );
        assert_eq!(ac.typed, 3.0 / 5.0);
        assert_eq!(ab.shared, vec!["id", "name", "age", "email"]);
    }

    #[test]
    fn labels_superset_and_subset_relative_to_file_order() {
        let records = vec![
            ty(
                "Small",
                TypeKind::Union,
                &["Pending", "Running", "Done", "Failed"],
            ),
            ty(
                "Big",
                TypeKind::Union,
                &["Pending", "Running", "Done", "Failed", "Paused"],
            ),
        ];
        let report = find_duplicate_types(&records, &options(), &NoJudge);
        assert_eq!(report.pairs[0].relationship, Relationship::Subset);
        assert_eq!(names(&records, &report.pairs[0]), ("Small", "Big"));
        let reversed = vec![
            records.into_iter().nth(1).unwrap(),
            ty(
                "Small",
                TypeKind::Enum,
                &["Pending", "Running", "Done", "Failed"],
            ),
        ];
        let report = find_duplicate_types(&reversed, &options(), &NoJudge);
        assert_eq!(report.pairs[0].relationship, Relationship::Superset);
    }

    #[test]
    fn members_only_compare_with_members() {
        let records = vec![
            ty("Status", TypeKind::Union, &["a", "b", "c", "d"]),
            ty("Shape", TypeKind::Interface, &["a", "b", "c", "d"]),
            ty("Alias", TypeKind::Alias, &[]),
        ];
        let report = find_duplicate_types(&records, &options(), &NoJudge);
        assert!(report.pairs.is_empty());
        assert_eq!(report.candidates, 2);
    }

    #[test]
    fn applies_thresholds_and_name_excludes() {
        let records = vec![
            ty("A", TypeKind::Interface, &["a", "b", "c", "d", "e"]),
            ty("B", TypeKind::Interface, &["a", "b", "c", "x", "y"]),
            ty(
                "ButtonProps",
                TypeKind::Interface,
                &["a", "b", "c", "d", "e"],
            ),
            ty("Tiny", TypeKind::Interface, &["a", "b"]),
        ];
        let report = find_duplicate_types(&records, &options(), &NoJudge);
        // A~B is 3/7 < 0.7; ButtonProps is excluded; Tiny has too few fields.
        assert!(report.pairs.is_empty());
        assert_eq!(report.candidates, 2);

        let loose = TypeOptions {
            threshold: 0.4,
            ..options()
        };
        assert_eq!(
            find_duplicate_types(&records, &loose, &NoJudge).pairs.len(),
            1
        );
        let strict_shared = TypeOptions {
            threshold: 0.4,
            min_shared: 4,
            ..options()
        };
        assert!(
            find_duplicate_types(&records, &strict_shared, &NoJudge)
                .pairs
                .is_empty()
        );
    }

    #[test]
    fn common_fields_still_score_but_do_not_seed_pairs() {
        // 60 types share `id, name, kind`; only X and Y also share `uncommon`.
        let mut records: Vec<Record> = (0..60)
            .map(|i| {
                ty(
                    &format!("T{i}"),
                    TypeKind::Interface,
                    &["id", "name", "kind", &format!("own{i}")],
                )
            })
            .collect();
        records.push(ty(
            "X",
            TypeKind::Interface,
            &["id", "name", "kind", "uncommon"],
        ));
        records.push(ty(
            "Y",
            TypeKind::Interface,
            &["id", "name", "kind", "uncommon"],
        ));
        let report = find_duplicate_types(
            &records,
            &TypeOptions {
                threshold: 0.5,
                ..options()
            },
            &NoJudge,
        );
        assert_eq!(report.pairs.len(), 1);
        let pair = &report.pairs[0];
        assert_eq!((names(&records, pair), pair.shared.len()), (("X", "Y"), 4));
    }

    #[test]
    fn clusters_link_chains_and_rank_by_size() {
        let records = vec![
            ty(
                "E1",
                TypeKind::Interface,
                &["id", "source", "target", "handle"],
            ),
            ty("P1", TypeKind::Interface, &["p", "q", "r", "s"]),
            ty("E2", TypeKind::Type, &["id", "source", "target", "handle"]),
            ty("P2", TypeKind::Interface, &["p", "q", "r", "s"]),
            ty(
                "E3",
                TypeKind::Class,
                &["id", "source", "target", "handle", "label"],
            ),
        ];
        let report = find_duplicate_types(&records, &options(), &NoJudge);
        assert_eq!(report.clusters.len(), 2);
        let first = &report.clusters[0];
        assert_eq!(first.members, vec![0, 2, 4]);
        assert_eq!(first.shared, vec!["id", "source", "target", "handle"]);
        assert_eq!(first.pairs.len(), 3);
        assert_eq!(report.clusters[1].members, vec![1, 3]);
    }

    #[test]
    fn normalizes_types() {
        assert_eq!(normalize_type("string  |  undefined"), "string");
        assert_eq!(normalize_type("string|undefined"), "string");
        assert_eq!(normalize_type("undefined | Foo | null"), "Foo");
        assert_eq!(normalize_type("B | A"), normalize_type("A|B"));
        assert_eq!(
            normalize_type("Map<\n string,\n number>"),
            "Map<string,number>"
        );
        assert_eq!(normalize_type("keyof  T"), "keyof T");
        assert_eq!(normalize_type("'a  b' | 'c'"), "'a  b'|'c'");
        // The union belongs to the return type, so it stays.
        assert_eq!(
            normalize_type("() => string | undefined"),
            "()=>string|undefined"
        );
        assert_eq!(
            normalize_type("Array<() => void> | null"),
            "Array<()=>void>"
        );
        assert_eq!(normalize_type("null"), "null");
    }

    #[test]
    fn unannotated_fields_match_any_type() {
        let records = vec![
            ty("Cls", TypeKind::Class, &["a", "b", "c", "d"]),
            ty(
                "Iface",
                TypeKind::Interface,
                &["a: 1", "b: 2", "c: 3", "d: 4"],
            ),
        ];
        let pair = &find_duplicate_types(&records, &options(), &NoJudge).pairs[0];
        assert_eq!((pair.typed, pair.relationship), (1.0, Relationship::Exact));
    }

    #[test]
    fn duplicate_names_keep_the_first_declaration() {
        let records = vec![
            ty(
                "A",
                TypeKind::Interface,
                &["a: x", "a: y", "b: x", "c: x", "d: x"],
            ),
            ty("B", TypeKind::Interface, &["a: x", "b: x", "c: x", "d: x"]),
        ];
        let pair = &find_duplicate_types(&records, &options(), &NoJudge).pairs[0];
        assert_eq!(
            (pair.similarity, pair.typed, pair.relationship),
            (1.0, 1.0, Relationship::Exact)
        );
    }

    #[test]
    fn boundary_similarity_qualifies() {
        // 14/25 = 0.56 exactly; a float-multiplied size check would reject it.
        let fields: Vec<String> = (0..25).map(|i| format!("f{i}")).collect();
        let fields: Vec<&str> = fields.iter().map(String::as_str).collect();
        let records = vec![
            ty("Small", TypeKind::Interface, &fields[..14]),
            ty("Big", TypeKind::Interface, &fields),
        ];
        let report = find_duplicate_types(
            &records,
            &TypeOptions {
                threshold: 0.56,
                ..options()
            },
            &NoJudge,
        );
        assert_eq!(report.pairs.len(), 1);
    }

    #[test]
    fn copies_made_only_of_common_fields_are_found() {
        // 60 copies of one shape: every field is common, but the copies are still exact pairs.
        let records: Vec<Record> = (0..60)
            .map(|i| {
                ty(
                    &format!("Rec{i}"),
                    TypeKind::Interface,
                    &["id", "name", "createdAt", "updatedAt"],
                )
            })
            .collect();
        let report = find_duplicate_types(&records, &options(), &NoJudge);
        assert_eq!(
            (report.pairs.len(), report.clusters.len()),
            (60 * 59 / 2, 1)
        );
    }

    #[test]
    fn excludes_by_scoped_name_too() {
        let mut scoped = ty("Scoped", TypeKind::Interface, &["a", "b", "c", "d"]);
        if let Record::Type(t) = &mut scoped {
            t.scope = Some("Api".into());
        }
        let records = vec![
            scoped,
            ty("Other", TypeKind::Interface, &["a", "b", "c", "d"]),
        ];
        let mut builder = GlobSetBuilder::new();
        builder.add(Glob::new("Api.*").unwrap());
        let report = find_duplicate_types(
            &records,
            &TypeOptions {
                exclude_names: builder.build().unwrap(),
                ..options()
            },
            &NoJudge,
        );
        assert!(report.pairs.is_empty());
    }

    #[test]
    fn conventional_members_do_not_count() {
        // Only `toString`/`valueOf` differ; they don't count, so the shapes match exactly.
        let records = vec![
            ty("A", TypeKind::Class, &["a", "b", "c", "toString"]),
            ty("B", TypeKind::Class, &["a", "b", "c", "valueOf"]),
        ];
        let pair = &find_duplicate_types(&records, &options(), &NoJudge).pairs[0];
        assert_eq!(
            (pair.similarity, pair.relationship),
            (1.0, Relationship::Exact)
        );
        assert_eq!(pair.shared, vec!["a", "b", "c"]);

        // A type left with too few counted fields isn't a candidate.
        let records = vec![
            ty("C", TypeKind::Class, &["a", "b", "toString", "toJSON"]),
            ty("D", TypeKind::Class, &["a", "b", "toString", "toJSON"]),
        ];
        assert_eq!(
            find_duplicate_types(&records, &options(), &NoJudge).candidates,
            0
        );
    }

    #[test]
    fn renders_clusters_and_pairs() {
        let records = vec![
            ty("A", TypeKind::Interface, &["a", "b", "c", "d"]),
            ty("B", TypeKind::Interface, &["a", "b", "c", "d", "e"]),
        ];
        let report = find_duplicate_types(&records, &options(), &NoJudge);
        let text = render_text(&records, &report, 40, false, &options(), &NoJudge);
        assert!(text.contains("1 pairs in 1 clusters from 2 candidate types"));
        assert!(text.contains("interface A (4 fields)  A.ts:1"));
        assert!(text.contains("- 0.80 similar, 0.80 typed: A ⊂ B"));
        let json = to_json(&records, &report, &NoJudge);
        assert_eq!(json["pairs"][0]["relationship"], "subset");
        assert_eq!(json["clusters"][0]["members"][1]["name"], "B");
    }
}
