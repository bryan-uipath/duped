//! Small top-level functions copied everywhere: N-way groups with identical normalised bodies,
//! too small for `bodies`, plus same-name families whose bodies have drifted apart.
//!
//! Parameters are renamed by position, so `isRec(v)` and `isRecord(value)` match; every other
//! name is kept, so `<AContext.Provider>` and `<BContext.Provider>` wrappers don't.

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write;

use serde_json::{Value, json};

use super::Judge;
use super::bodies::function_at;
use super::names::family;
use crate::modules::Home;
use crate::record::{FunctionRecord, Language, Record, Token, token_hash};

pub struct HelperOptions {
    /// Bodies with fewer tokens are too generic to group, e.g. `{ return x; }`.
    pub min_tokens: usize,
    /// Bodies with more tokens are left to `bodies`.
    pub max_tokens: usize,
}

#[derive(Debug)]
pub struct HelperGroup {
    /// Record indices, in file order.
    pub members: Vec<usize>,
    /// Lines merging the group removes: every copy's lines but the longest one's.
    pub removable_lines: usize,
    /// Distinct module names, sorted; empty when modules aren't tracked.
    pub modules: Vec<String>,
    pub home: Option<Home>,
}

/// Same-name helpers with several bodies, one of them copied: drifted copies of one helper.
#[derive(Debug)]
pub struct HelperFamily {
    pub name: String,
    /// Record indices per distinct body, largest variant first, each in file order.
    pub variants: Vec<Vec<usize>>,
}

#[derive(Debug)]
pub struct HelperReport {
    /// Top-level functions in the token range, which groups are made from.
    pub candidates: usize,
    /// Most removable lines first.
    pub groups: Vec<HelperGroup>,
    /// Most definitions first.
    pub families: Vec<HelperFamily>,
    /// Members left out because they are acknowledged against their group's first member.
    pub hidden_acknowledged: usize,
}

pub fn find_helpers(
    records: &[Record],
    options: &HelperOptions,
    judge: &dyn Judge,
) -> HelperReport {
    // Every small top-level function counts toward its name's family; groups need `min_tokens`.
    let mut by_body: HashMap<(Language, Vec<u64>), Vec<usize>> = HashMap::new();
    for (i, r) in records.iter().enumerate() {
        if let Record::Function(f) = r
            && f.scope.is_none()
            && (1..=options.max_tokens).contains(&f.body.len())
        {
            by_body
                .entry((family(f.language), normalised(f)))
                .or_default()
                .push(i);
        }
    }
    let candidates = by_body
        .iter()
        .filter(|((_, tokens), _)| tokens.len() >= options.min_tokens)
        .map(|(_, members)| members.len())
        .sum();

    let mut hidden_acknowledged = 0;
    let mut groups: Vec<HelperGroup> = Vec::new();
    // Body ids per name, for families.
    let mut names: HashMap<(Language, &str), Vec<(usize, usize)>> = HashMap::new();
    for (body, ((language, tokens), members)) in by_body.iter().enumerate() {
        for &m in members {
            names
                .entry((*language, records[m].name()))
                .or_default()
                .push((body, m));
        }
        if members.len() < 2 || tokens.len() < options.min_tokens {
            continue;
        }
        let mut members = members.clone();
        members.sort_unstable();
        let first = members[0];
        let before = members.len();
        members.retain(|&m| m == first || judge.verdict(first, m).acknowledged.is_none());
        hidden_acknowledged += before - members.len();
        if members.len() < 2 {
            continue;
        }
        let lines: Vec<usize> = members.iter().map(|&m| line_count(&records[m])).collect();
        let modules: BTreeSet<String> = members
            .iter()
            .filter_map(|&m| judge.module(m).map(str::to_string))
            .collect();
        groups.push(HelperGroup {
            removable_lines: removable_lines(&lines),
            modules: modules.into_iter().collect(),
            home: judge.home(&members),
            members,
        });
    }
    groups.sort_by(|x, y| {
        y.removable_lines
            .cmp(&x.removable_lines)
            .then(y.members.len().cmp(&x.members.len()))
            .then(x.members[0].cmp(&y.members[0]))
    });

    let mut families: Vec<HelperFamily> = names
        .into_iter()
        .filter_map(|((_, name), members)| {
            let mut variants: HashMap<usize, Vec<usize>> = HashMap::new();
            for (body, m) in members {
                variants.entry(body).or_default().push(m);
            }
            // Drift needs a copied original and 3+ definitions; same-name functions that
            // all differ, like `run` or `parse`, are conventions.
            let definitions: usize = variants.values().map(Vec::len).sum();
            let copied = variants.values().any(|v| v.len() >= 2);
            if variants.len() < 2 || !copied || definitions < 3 {
                return None;
            }
            let mut variants: Vec<Vec<usize>> = variants.into_values().collect();
            for v in &mut variants {
                v.sort_unstable();
            }
            variants.sort_by(|x, y| y.len().cmp(&x.len()).then(x[0].cmp(&y[0])));
            Some(HelperFamily {
                name: name.to_string(),
                variants,
            })
        })
        .collect();
    families.sort_by(|x, y| {
        definitions(y)
            .cmp(&definitions(x))
            .then(x.variants[0][0].cmp(&y.variants[0][0]))
    });

    HelperReport {
        candidates,
        groups,
        families,
        hidden_acknowledged,
    }
}

/// Body tokens with parameters (destructured keys too) as their position, so renamed
/// parameters match; text, names and positions can't collide. `{ return x; }` becomes `x`,
/// as an arrow function's expression body would be.
fn normalised(f: &FunctionRecord) -> Vec<u64> {
    let params: Vec<u32> = f
        .params
        .iter()
        .flat_map(|p| {
            if p.fields.is_empty() {
                vec![p.name.trim_start_matches("...")]
            } else {
                p.fields.iter().map(|k| k.name.as_str()).collect()
            }
        })
        .map(token_hash)
        .collect();
    let tokens: Vec<u64> = f
        .body
        .iter()
        .map(|&t| match t {
            Token::Text(h) => u64::from(h),
            Token::Identifier(h) => match params.iter().position(|&p| p == h) {
                Some(i) => 2 << 32 | i as u64,
                None => 1 << 32 | u64::from(h),
            },
        })
        .collect();
    let text = |s| u64::from(token_hash(s));
    let inner = match &tokens[..] {
        [open, ret, inner @ .., close]
            if [*open, *ret, *close] == [text("{"), text("return"), text("}")] =>
        {
            inner.strip_suffix(&[text(";")]).unwrap_or(inner)
        }
        _ => return tokens,
    };
    // Only a lone `return`: `{ return a; b(); }` is not an expression.
    if inner.contains(&text(";")) {
        tokens
    } else {
        inner.to_vec()
    }
}

pub fn line_count(record: &Record) -> usize {
    let location = record.location();
    location.end_line + 1 - location.start_line
}

/// Keeping the longest copy removes every other copy's lines.
pub fn removable_lines(lines: &[usize]) -> usize {
    lines.iter().sum::<usize>() - lines.iter().max().unwrap_or(&0)
}

fn definitions(family: &HelperFamily) -> usize {
    family.variants.iter().map(Vec::len).sum()
}

// ----- output -----

/// Human-readable report: identical groups, then drifted families.
pub fn render_text(
    records: &[Record],
    report: &HelperReport,
    top: usize,
    options: &HelperOptions,
    judge: &dyn Judge,
) -> String {
    let mut out = String::new();
    let removable: usize = report.groups.iter().map(|g| g.removable_lines).sum();
    writeln!(
        out,
        "{} groups of identical helpers (~{removable} removable lines) and {} drifted families from {} top-level functions of {}–{} tokens.",
        report.groups.len(),
        report.families.len(),
        report.candidates,
        options.min_tokens,
        options.max_tokens
    )
    .ok();
    if let Some(summary) = judge.summary() {
        writeln!(out, "Modules: {summary}.").ok();
    }
    if report.hidden_acknowledged > 0 {
        writeln!(
            out,
            "Hidden copies: {} acknowledged.",
            report.hidden_acknowledged
        )
        .ok();
    }
    out.push_str(&render_groups(records, &report.groups, top, judge));
    if !report.families.is_empty() {
        writeln!(out, "\nSame name, different bodies:").ok();
    }
    for (rank, f) in report.families.iter().take(top).enumerate() {
        writeln!(
            out,
            "\n{}. {}: {} definitions, {} bodies",
            rank + 1,
            f.name,
            definitions(f),
            f.variants.len()
        )
        .ok();
        for v in &f.variants {
            writeln!(out, "   {}× {}", v.len(), location(records, v[0])).ok();
        }
    }
    if report.families.len() > top {
        writeln!(
            out,
            "\n… {} more families; raise --top to see them.",
            report.families.len() - top
        )
        .ok();
    }
    out
}

/// Ranked groups.
fn render_groups(
    records: &[Record],
    groups: &[HelperGroup],
    top: usize,
    judge: &dyn Judge,
) -> String {
    let mut out = String::new();
    if !groups.is_empty() {
        writeln!(out, "\nIdentical helpers:").ok();
    }
    for (rank, g) in groups.iter().take(top).enumerate() {
        let mut names: Vec<&str> = g.members.iter().map(|&m| records[m].name()).collect();
        names.sort_unstable();
        names.dedup();
        let modules = match &g.modules[..] {
            [] => String::new(),
            [one] => format!(" in {one}"),
            many => format!(" in {} modules", many.len()),
        };
        writeln!(
            out,
            "\n{}. {} copies{modules}, ~{} removable lines: {}",
            rank + 1,
            g.members.len(),
            g.removable_lines,
            names.join(", ")
        )
        .ok();
        match &g.home {
            Some(home) => writeln!(out, "   {}", home.describe()).ok(),
            None if g.modules.len() > 1 => {
                writeln!(out, "   home: none (no module they all depend on)").ok()
            }
            None => None,
        };
        for &m in &g.members {
            let module = judge
                .module(m)
                .map(|name| format!("[{name}] "))
                .unwrap_or_default();
            writeln!(out, "   {module}{}", location(records, m)).ok();
        }
    }
    if groups.len() > top {
        writeln!(
            out,
            "\n… {} more groups; raise --top to see them.",
            groups.len() - top
        )
        .ok();
    }
    out
}

/// e.g. `isRecord  src/util.ts:12`
fn location(records: &[Record], index: usize) -> String {
    let f = function_at(records, index);
    format!("{}  {}:{}", f.name, f.location.file, f.location.start_line)
}

/// Everything, untruncated: `{ summary, groups, families }`.
pub fn to_json(records: &[Record], report: &HelperReport, judge: &dyn Judge) -> Value {
    let families: Vec<Value> = report
        .families
        .iter()
        .map(|f| {
            json!({
                "name": f.name,
                "definitions": definitions(f),
                "variants": f.variants.iter().map(|v| members_json(records, v, judge)).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({
        "summary": {
            "candidates": report.candidates,
            "groups": report.groups.len(),
            "families": report.families.len(),
            "removableLines": report.groups.iter().map(|g| g.removable_lines).sum::<usize>(),
            "modules": judge.summary(),
            "hidden": { "acknowledged": report.hidden_acknowledged },
        },
        "groups": report.groups.iter().map(|g| group_json(records, g, judge)).collect::<Vec<_>>(),
        "families": families,
    })
}

/// `{ members, removableLines, modules, home }`
fn group_json(records: &[Record], group: &HelperGroup, judge: &dyn Judge) -> Value {
    json!({
        "members": members_json(records, &group.members, judge),
        "removableLines": group.removable_lines,
        "modules": group.modules,
        "home": group.home,
    })
}

fn members_json(records: &[Record], members: &[usize], judge: &dyn Judge) -> Vec<Value> {
    members
        .iter()
        .map(|&m| {
            let f = function_at(records, m);
            json!({
                "name": f.name,
                "module": judge.module(m),
                "file": f.location.file,
                "line": f.location.start_line,
                "lines": line_count(&records[m]),
                "tokens": f.body.len(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::NoJudge;
    use crate::lang::typescript;
    use tree_sitter::Parser;

    fn options() -> HelperOptions {
        HelperOptions {
            min_tokens: 8,
            max_tokens: 49,
        }
    }

    fn extract(files: &[(&str, &str)]) -> Vec<Record> {
        files
            .iter()
            .flat_map(|(file, source)| {
                typescript::extract(&mut Parser::new(), source, file, Language::TypeScript)
            })
            .collect()
    }

    /// Each group's member names, in file order.
    fn groups(records: &[Record], report: &HelperReport) -> Vec<Vec<String>> {
        report
            .groups
            .iter()
            .map(|g| {
                g.members
                    .iter()
                    .map(|&m| records[m].name().into())
                    .collect()
            })
            .collect()
    }

    const IS_RECORD: &str = "export function isRecord(value: unknown): value is Record<string, unknown> {\n  return typeof value === 'object' && value !== null && !Array.isArray(value);\n}\n";

    #[test]
    fn renamed_parameters_and_arrow_bodies_match() {
        let arrow = "const isRec = (v: unknown) => typeof v === 'object' && v !== null && !Array.isArray(v);";
        let records = extract(&[("a.ts", IS_RECORD), ("b.ts", arrow), ("c.ts", IS_RECORD)]);
        let report = find_helpers(&records, &options(), &NoJudge);
        assert_eq!(
            groups(&records, &report),
            vec![vec!["isRecord", "isRec", "isRecord"]]
        );
        // Keep one 3-line copy: the other 3-line one and the 1-line arrow go.
        assert_eq!(report.groups[0].removable_lines, 4);
        assert_eq!(report.candidates, 3);
    }

    #[test]
    fn other_names_tiny_bodies_and_methods_do_not_group() {
        // Same shape, different free names: two providers, not one copied helper.
        let provider = |ctx: &str| {
            format!(
                "export function P({{ children, value }}) {{ return <{ctx}.Provider value={{value}}>{{children}}</{ctx}.Provider>; }}"
            )
        };
        let tiny = "export function id(x) { return x.id; }";
        let method = format!(
            "class A {{ isRecord(value) {{ {} }} }}",
            "return typeof value === 'object' && value !== null && !Array.isArray(value);"
        );
        let records = extract(&[
            ("a.tsx", &provider("AContext")),
            ("b.tsx", &provider("BContext")),
            ("c.ts", tiny),
            ("d.ts", tiny),
            ("e.ts", &method),
            ("f.ts", &method),
        ]);
        let report = find_helpers(&records, &options(), &NoJudge);
        assert!(report.groups.is_empty(), "{:?}", groups(&records, &report));
        let loose = HelperOptions {
            min_tokens: 1,
            ..options()
        };
        assert_eq!(
            groups(&records, &find_helpers(&records, &loose, &NoJudge)),
            vec![vec!["id", "id"]]
        );
    }

    #[test]
    fn families_need_a_copied_body_and_three_definitions() {
        let short = "export const isRecord = (value: unknown) => typeof value === 'object' && value !== null;";
        let run = |n: usize| format!("export function run() {{ start({n}); }}");
        let records = extract(&[
            ("a.ts", IS_RECORD),
            ("b.ts", IS_RECORD),
            ("c.ts", short),
            // All different: a convention, not drift.
            ("d.ts", &run(1)),
            ("e.ts", &run(2)),
            ("f.ts", &run(3)),
        ]);
        let report = find_helpers(&records, &options(), &NoJudge);
        let [family] = &report.families[..] else {
            panic!("{:?}", report.families)
        };
        assert_eq!(family.name, "isRecord");
        let sizes: Vec<usize> = family.variants.iter().map(Vec::len).collect();
        assert_eq!(sizes, vec![2, 1]);

        let text = render_text(&records, &report, 40, &options(), &NoJudge);
        assert!(
            text.contains("1. isRecord: 3 definitions, 2 bodies"),
            "{text}"
        );
        assert!(
            text.contains("2 copies, ~3 removable lines: isRecord"),
            "{text}"
        );
        let json = to_json(&records, &report, &NoJudge);
        assert_eq!(json["groups"][0]["removableLines"], 3);
        assert_eq!(json["families"][0]["variants"][1][0]["file"], "c.ts");
    }
}
