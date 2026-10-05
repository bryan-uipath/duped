//! Types that share most of their properties, whatever they are called.

use std::collections::HashMap;
use std::fmt::Write;

use globset::GlobSet;
use serde::Serialize;
use serde_json::{Value, json};

use super::{Interner, components, intersection_len, jaccard};
use crate::record::{Record, TypeKind, TypeRecord};

pub struct TypeOptions {
    /// Types with fewer fields are not compared.
    pub min_fields: usize,
    /// A pair needs at least this many shared field names.
    pub min_shared: usize,
    /// Minimum Jaccard similarity of field names.
    pub threshold: f64,
    /// Type names to skip, e.g. `*Props`.
    pub exclude_names: GlobSet,
    /// Field names on more than this fraction of types (and more than
    /// [`COMMON_FIELD_FLOOR`] of them) don't seed candidate pairs, but still score.
    pub common_field_fraction: f64,
}

/// A field name is only "common" once it appears on more types than this, whatever the fraction.
pub const COMMON_FIELD_FLOOR: usize = 50;

/// How the first type's field names relate to the second's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Relationship {
    /// Same field names and types.
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
    /// Jaccard similarity of (name, normalised type) pairs.
    pub typed: f64,
    /// Shared field names, in the first type's order.
    pub shared: Vec<String>,
    pub relationship: Relationship,
}

#[derive(Debug)]
pub struct TypeCluster {
    /// Record indices, in file order.
    pub members: Vec<usize>,
    /// Indices into [`TypeReport::pairs`], best first.
    pub pairs: Vec<usize>,
    /// Field names every member has.
    pub shared: Vec<String>,
}

#[derive(Debug)]
pub struct TypeReport {
    /// Types that met the candidate rules.
    pub candidates: usize,
    /// Qualifying pairs, best first.
    pub pairs: Vec<TypePair>,
    /// Groups of types linked by qualifying pairs, largest first.
    pub clusters: Vec<TypeCluster>,
}

/// Which kinds are comparable: object shapes with each other, enums and unions with each other.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Group {
    Shape,
    Members,
}

struct Candidate<'r> {
    record: usize,
    group: Group,
    /// Field names in declaration order, deduplicated.
    fields: Vec<&'r str>,
    /// Sorted ids of field names.
    names: Vec<u32>,
    /// Sorted ids of `name: normalised type`.
    typed: Vec<u32>,
}

pub fn find_duplicate_types(records: &[Record], options: &TypeOptions) -> TypeReport {
    let typed_keys: Vec<Vec<String>> = records.iter().map(typed_keys).collect();
    let mut names = Interner::default();
    let mut typed_ids = Interner::default();
    let mut candidates = Vec::new();
    for (index, record) in records.iter().enumerate() {
        let Record::Type(t) = record else { continue };
        let Some(group) = group_of(t.kind) else {
            continue;
        };
        if options.exclude_names.is_match(&t.name) {
            continue;
        }
        let mut fields: Vec<&str> = Vec::new();
        for field in &t.fields {
            if !fields.contains(&field.name.as_str()) {
                fields.push(&field.name);
            }
        }
        if fields.len() < options.min_fields {
            continue;
        }
        let mut name_ids: Vec<u32> = fields.iter().map(|f| names.id(f)).collect();
        name_ids.sort_unstable();
        let mut typed: Vec<u32> = typed_keys[index].iter().map(|k| typed_ids.id(k)).collect();
        typed.sort_unstable();
        typed.dedup();
        candidates.push(Candidate {
            record: index,
            group,
            fields,
            names: name_ids,
            typed,
        });
    }

    let mut pairs = Vec::new();
    for group in [Group::Shape, Group::Members] {
        let members: Vec<&Candidate> = candidates.iter().filter(|c| c.group == group).collect();
        score_group(&members, options, &mut pairs);
    }
    pairs.sort_by(|x, y| {
        y.similarity
            .total_cmp(&x.similarity)
            .then(y.typed.total_cmp(&x.typed))
            .then(y.shared.len().cmp(&x.shared.len()))
            .then((x.a, x.b).cmp(&(y.a, y.b)))
    });

    let clusters = cluster(&candidates, &pairs);
    TypeReport {
        candidates: candidates.len(),
        pairs,
        clusters,
    }
}

/// Score every pair in one comparable group that shares at least one uncommon field.
fn score_group(members: &[&Candidate], options: &TypeOptions, pairs: &mut Vec<TypePair>) {
    let mut postings: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, candidate) in members.iter().enumerate() {
        for &name in &candidate.names {
            postings.entry(name).or_default().push(i);
        }
    }
    let common_limit = COMMON_FIELD_FLOOR
        .max((options.common_field_fraction * members.len() as f64).ceil() as usize);

    // `seen[j] == i + 1` marks `j` as already queued for `i`, without clearing between rows.
    let mut seen = vec![0usize; members.len()];
    let mut queue = Vec::new();
    for (i, a) in members.iter().enumerate() {
        queue.clear();
        for name in &a.names {
            let posting = &postings[name];
            if posting.len() > common_limit {
                continue;
            }
            for &j in posting.iter().filter(|&&j| j > i) {
                if seen[j] != i + 1 {
                    seen[j] = i + 1;
                    queue.push(j);
                }
            }
        }
        for &j in &queue {
            if let Some(pair) = score_pair(a, members[j], options) {
                pairs.push(pair);
            }
        }
    }
}

fn score_pair(a: &Candidate, b: &Candidate, options: &TypeOptions) -> Option<TypePair> {
    let (small, large) = (
        a.names.len().min(b.names.len()),
        a.names.len().max(b.names.len()),
    );
    // Jaccard can't exceed the size ratio, so skip pairs that can never qualify.
    if (small as f64) < options.threshold * large as f64 {
        return None;
    }
    let shared = intersection_len(&a.names, &b.names);
    let similarity = jaccard(a.names.len(), b.names.len(), shared);
    if shared < options.min_shared || similarity < options.threshold {
        return None;
    }
    let typed_shared = intersection_len(&a.typed, &b.typed);
    let typed = jaccard(a.typed.len(), b.typed.len(), typed_shared);
    let relationship = if shared == a.names.len() && shared == b.names.len() {
        if typed_shared == a.typed.len() && typed_shared == b.typed.len() {
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
    })
}

fn cluster(candidates: &[Candidate], pairs: &[TypePair]) -> Vec<TypeCluster> {
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
            let pairs = pairs
                .iter()
                .enumerate()
                .filter(|(_, p)| members.binary_search(&p.a).is_ok())
                .map(|(i, _)| i)
                .collect();
            TypeCluster {
                members,
                pairs,
                shared,
            }
        })
        .collect();
    clusters.sort_by(|x, y| {
        let best = |c: &TypeCluster| c.pairs.first().map_or(0.0, |&i| pairs[i].similarity);
        y.members
            .len()
            .cmp(&x.members.len())
            .then(best(y).total_cmp(&best(x)))
            .then(x.members[0].cmp(&y.members[0]))
    });
    clusters
}

fn group_of(kind: TypeKind) -> Option<Group> {
    match kind {
        TypeKind::Alias => None,
        TypeKind::Enum | TypeKind::Union => Some(Group::Members),
        _ => Some(Group::Shape),
    }
}

/// `name: type` keys for typed scoring; non-type records yield nothing.
fn typed_keys(record: &Record) -> Vec<String> {
    let Record::Type(t) = record else {
        return Vec::new();
    };
    t.fields
        .iter()
        .map(|f| {
            format!(
                "{}: {}",
                f.name,
                normalize_type(f.ty.as_deref().unwrap_or(""))
            )
        })
        .collect()
}

/// `string | undefined` and `string` compare equal; whitespace never matters.
pub fn normalize_type(ty: &str) -> String {
    let mut ty = ty.split_whitespace().collect::<Vec<_>>().join(" ");
    loop {
        let trimmed = ty
            .strip_suffix("| undefined")
            .or_else(|| ty.strip_suffix("| null"))
            .map(|t| t.trim_end().to_string());
        match trimmed {
            Some(t) => ty = t,
            None => return ty,
        }
    }
}

// ----- output -----

/// Human-readable report: clusters by default, or a flat pair list.
pub fn render_text(
    records: &[Record],
    report: &TypeReport,
    top: usize,
    pairs_only: bool,
    options: &TypeOptions,
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
    if pairs_only {
        for (rank, pair) in report.pairs.iter().take(top).enumerate() {
            writeln!(
                out,
                "\n{}. {:.2} similar, {:.2} typed, {} shared: {}",
                rank + 1,
                pair.similarity,
                pair.typed,
                pair.shared.len(),
                relation_text(records, pair)
            )
            .ok();
            for &index in [pair.a, pair.b].iter() {
                writeln!(out, "   {}", member_line(type_at(records, index))).ok();
            }
        }
    } else {
        for (rank, cluster) in report.clusters.iter().take(top).enumerate() {
            let best = cluster
                .pairs
                .first()
                .map_or(0.0, |&i| report.pairs[i].similarity);
            writeln!(
                out,
                "\n{}. {} types, best {best:.2}",
                rank + 1,
                cluster.members.len()
            )
            .ok();
            if !cluster.shared.is_empty() {
                writeln!(out, "   shared by all: {}", cluster.shared.join(", ")).ok();
            }
            for &index in &cluster.members {
                writeln!(out, "   {}", member_line(type_at(records, index))).ok();
            }
            const SHOWN_PAIRS: usize = 10;
            for &i in cluster.pairs.iter().take(SHOWN_PAIRS) {
                let pair = &report.pairs[i];
                writeln!(
                    out,
                    "   - {:.2} similar, {:.2} typed: {}",
                    pair.similarity,
                    pair.typed,
                    relation_text(records, pair)
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
pub fn to_json(records: &[Record], report: &TypeReport) -> Value {
    let pairs: Vec<Value> = report
        .pairs
        .iter()
        .map(|p| {
            json!({
                "a": member_json(type_at(records, p.a)),
                "b": member_json(type_at(records, p.b)),
                "similarity": p.similarity,
                "typed": p.typed,
                "relationship": p.relationship,
                "shared": p.shared,
            })
        })
        .collect();
    let clusters: Vec<Value> = report
        .clusters
        .iter()
        .map(|c| {
            json!({
                "members": c.members.iter().map(|&i| member_json(type_at(records, i))).collect::<Vec<_>>(),
                "shared": c.shared,
                "pairs": c.pairs,
            })
        })
        .collect();
    json!({
        "summary": { "candidates": report.candidates, "pairs": report.pairs.len(), "clusters": report.clusters.len() },
        "pairs": pairs,
        "clusters": clusters,
    })
}

fn type_at(records: &[Record], index: usize) -> &TypeRecord {
    match &records[index] {
        Record::Type(t) => t,
        Record::Function(_) => unreachable!("pairs only reference type records"),
    }
}

fn qualified(t: &TypeRecord) -> String {
    match &t.scope {
        Some(scope) => format!("{scope}.{}", t.name),
        None => t.name.clone(),
    }
}

/// The kind's serialized name, e.g. `interface`.
fn kind_name(kind: TypeKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// e.g. `interface Api.User (6 fields)  src/api.ts:12`
fn member_line(t: &TypeRecord) -> String {
    format!(
        "{} {} ({} fields)  {}:{}",
        kind_name(t.kind),
        qualified(t),
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
        qualified(type_at(records, pair.a)),
        qualified(type_at(records, pair.b))
    )
}

fn member_json(t: &TypeRecord) -> Value {
    json!({
        "name": t.name,
        "scope": t.scope,
        "kind": t.kind,
        "language": t.language,
        "file": t.location.file,
        "line": t.location.start_line,
        "fields": t.fields.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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
                    kind: if ty_is_member(kind) {
                        FieldKind::Member
                    } else {
                        FieldKind::Property
                    },
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

    fn ty_is_member(kind: TypeKind) -> bool {
        matches!(kind, TypeKind::Enum | TypeKind::Union)
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
        let report = find_duplicate_types(&records, &options());
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
        let report = find_duplicate_types(&records, &options());
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
        let report = find_duplicate_types(&reversed, &options());
        assert_eq!(report.pairs[0].relationship, Relationship::Superset);
    }

    #[test]
    fn members_only_compare_with_members() {
        let records = vec![
            ty("Status", TypeKind::Union, &["a", "b", "c", "d"]),
            ty("Shape", TypeKind::Interface, &["a", "b", "c", "d"]),
            ty("Alias", TypeKind::Alias, &[]),
        ];
        let report = find_duplicate_types(&records, &options());
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
        let report = find_duplicate_types(&records, &options());
        // A~B is 3/7 < 0.7; ButtonProps is excluded; Tiny has too few fields.
        assert!(report.pairs.is_empty());
        assert_eq!(report.candidates, 2);

        let loose = TypeOptions {
            threshold: 0.4,
            ..options()
        };
        assert_eq!(find_duplicate_types(&records, &loose).pairs.len(), 1);
        let strict_shared = TypeOptions {
            threshold: 0.4,
            min_shared: 4,
            ..options()
        };
        assert!(
            find_duplicate_types(&records, &strict_shared)
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
        let report = find_duplicate_types(&records, &options());
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
        assert_eq!(normalize_type("Foo | null | undefined"), "Foo");
        assert_eq!(
            normalize_type("Map<\n string,\n number>"),
            "Map< string, number>"
        );
    }

    #[test]
    fn renders_clusters_and_pairs() {
        let records = vec![
            ty("A", TypeKind::Interface, &["a", "b", "c", "d"]),
            ty("B", TypeKind::Interface, &["a", "b", "c", "d", "e"]),
        ];
        let report = find_duplicate_types(&records, &options());
        let text = render_text(&records, &report, 40, false, &options());
        assert!(text.contains("1 pairs in 1 clusters from 2 candidate types"));
        assert!(text.contains("interface A (4 fields)  A.ts:1"));
        assert!(text.contains("- 0.80 similar, 0.80 typed: A ⊂ B"));
        let json = to_json(&records, &report);
        assert_eq!(json["pairs"][0]["relationship"], "subset");
        assert_eq!(json["clusters"][0]["members"][1]["name"], "B");
    }
}
