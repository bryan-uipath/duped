//! Small top-level functions copied everywhere: N-way groups with identical normalised bodies,
//! too small for `bodies`, plus same-name families whose bodies have drifted apart.
//!
//! Plain parameters are renamed by position, so `isRec(v)` and `isRecord(value)` match; every
//! other name is kept, so `<AContext.Provider>` and `<BContext.Provider>` wrappers don't.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write;

use serde::Serialize;
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

/// One group, as both `helpers` and `report` print it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HelperGroup {
    /// In file order.
    pub members: Vec<Member>,
    /// Lines merging the group removes: every copy's lines but the longest one's.
    pub removable_lines: usize,
    /// Distinct module names, sorted; empty when modules aren't tracked.
    pub modules: Vec<String>,
    pub home: Option<Home>,
}

#[derive(Debug, Serialize)]
pub struct Member {
    #[serde(skip)]
    pub record: usize,
    pub name: String,
    pub module: Option<String>,
    pub file: String,
    pub line: usize,
    pub lines: usize,
    pub tokens: usize,
    /// Set by `report --base` when the change touched its lines.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub changed: bool,
}

/// Same-name helpers with several bodies, one of them copied: drifted copies of one helper.
#[derive(Debug)]
pub struct HelperFamily {
    pub name: String,
    /// Record indices per distinct body, largest variant first, each in file order.
    pub variants: Vec<Vec<usize>>,
}

/// A family, as both `helpers` and `report` print it.
#[derive(Debug, Serialize)]
pub struct Family {
    pub name: String,
    pub definitions: usize,
    /// One list of copies per distinct body, largest first.
    pub variants: Vec<Vec<Member>>,
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
        if members.len() >= 2 {
            groups.push(group(records, members, judge));
        }
    }
    sort_groups(&mut groups);

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

/// `groups` rebuilt without copies in `files`, e.g. identical files `report` already counts,
/// so lines, modules and home describe only the copies left.
pub fn without_files(
    records: &[Record],
    groups: Vec<HelperGroup>,
    files: &HashSet<String>,
    judge: &dyn Judge,
) -> Vec<HelperGroup> {
    let mut kept: Vec<HelperGroup> = groups
        .into_iter()
        .filter_map(|g| {
            let members: Vec<usize> = g
                .members
                .iter()
                .filter(|m| !files.contains(&m.file))
                .map(|m| m.record)
                .collect();
            (members.len() >= 2).then(|| group(records, members, judge))
        })
        .collect();
    sort_groups(&mut kept);
    kept
}

/// A group of `members` (record indices, in file order).
fn group(records: &[Record], members: Vec<usize>, judge: &dyn Judge) -> HelperGroup {
    let lines: Vec<usize> = members.iter().map(|&m| line_count(&records[m])).collect();
    let modules: BTreeSet<String> = members
        .iter()
        .filter_map(|&m| judge.module(m).map(str::to_string))
        .collect();
    HelperGroup {
        removable_lines: lines.iter().sum::<usize>() - lines.iter().max().unwrap_or(&0),
        modules: modules.into_iter().collect(),
        home: judge.home(&members),
        members: members.iter().map(|&m| member(records, m, judge)).collect(),
    }
}

fn member(records: &[Record], index: usize, judge: &dyn Judge) -> Member {
    let f = function_at(records, index);
    Member {
        record: index,
        name: f.name.clone(),
        module: judge.module(index).map(str::to_string),
        file: f.location.file.clone(),
        line: f.location.start_line,
        lines: line_count(&records[index]),
        tokens: f.body.len(),
        changed: false,
    }
}

fn sort_groups(groups: &mut [HelperGroup]) {
    groups.sort_by(|x, y| {
        y.removable_lines
            .cmp(&x.removable_lines)
            .then(y.members.len().cmp(&x.members.len()))
            .then(x.members[0].record.cmp(&y.members[0].record))
    });
}

/// Body tokens with plain parameters as their position, so renamed parameters match; text,
/// names and positions can't collide. A destructured parameter keeps its keys by name, since
/// callers pass those names. Text naming a parameter, such as shorthand `{ x }`, also records
/// its position, so `{ x }` from the first parameter never matches `{ x }` from the second.
/// `{ return x; }` becomes `x`, as an arrow function's expression body would be. Runs of pure
/// boolean tests are sorted (see [`sort_pure_tests`]).
fn normalised(f: &FunctionRecord) -> Vec<u64> {
    // Only a bare identifier is abstracted; a pattern (`{ user: u }`, `[a, b]`, `n = 10`) is
    // part of the signature callers see, so its text joins the key.
    let plain = |name: &str| {
        let name = name.trim_start_matches("...");
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
    };
    let params: Vec<Option<u32>> = f
        .params
        .iter()
        .map(|p| {
            (p.fields.is_empty() && plain(&p.name))
                .then(|| token_hash(p.name.trim_start_matches("...")))
        })
        .collect();
    let patterns = f
        .params
        .iter()
        .filter(|p| !(p.fields.is_empty() && plain(&p.name)))
        .map(|p| 4 << 48 | u64::from(token_hash(&p.name)));
    let position = |h: u32| params.iter().position(|&p| p == Some(h));
    let mut tokens: Vec<u64> = f
        .body
        .iter()
        .map(|&t| match t {
            Token::Text(h) => match position(h) {
                Some(i) => 3 << 48 | (i as u64) << 32 | u64::from(h),
                None => u64::from(h),
            },
            Token::Identifier(h) => match position(h) {
                Some(i) => 2 << 32 | i as u64,
                None => 1 << 32 | u64::from(h),
            },
        })
        .collect();
    sort_pure_tests(&mut tokens);
    let inner = match &tokens[..] {
        [open, ret, inner @ .., close]
            if [*open, *ret, *close] == [text("{"), text("return"), text("}")] =>
        {
            inner.strip_suffix(&[text(";")]).unwrap_or(inner)
        }
        _ => return patterns.chain(tokens).collect(),
    };
    // Only a lone `return`: `{ return a; b(); }` is not an expression.
    let body = if inner.contains(&text(";")) {
        tokens.clone()
    } else {
        inner.to_vec()
    };
    patterns.chain(body).collect()
}

fn text(s: &str) -> u64 {
    u64::from(token_hash(s))
}

/// Sorts each run of adjacent pure tests joined by one of `&&`/`||`, so
/// `typeof v === 'object' && v !== null` and `v !== null && typeof v === 'object'` match.
/// A pure test is `p === x`, `p !== x`, `p == null`, `p != null` (either side may be `typeof p`)
/// or `!p`, `!!p`, with `p` a plain parameter and `x` a parameter or literal: boolean, cannot
/// throw, no effects, so order cannot matter. Anything else (`a.b`, `f(a)`, a bare `a`, other
/// names) stays where it is, and tests are never moved across it.
fn sort_pure_tests(tokens: &mut [u64]) {
    let [and, or] = [text("&&"), text("||")];
    let before = [text("("), text("return"), and, or];
    let after = [text(")"), text(";"), and, or];
    let mut i = 0;
    while i < tokens.len() {
        let ok_before = i == 0 || before.contains(&tokens[i - 1]);
        let Some(first) = pure_test(&tokens[i..]).filter(|_| ok_before) else {
            i += 1;
            continue;
        };
        // Each test's (start, end), all joined by the operator after the first.
        let mut run = vec![(i, i + first)];
        let mut end = i + first;
        let op = tokens.get(end).copied().filter(|t| [and, or].contains(t));
        while let Some(op) = op
            && tokens.get(end) == Some(&op)
            && let Some(n) = pure_test(&tokens[end + 1..])
        {
            run.push((end + 1, end + 1 + n));
            end += 1 + n;
        }
        // `a || b && c`: `&&` binds tighter, so a `||` run's edge operand next to `&&` isn't its own.
        let binds = |t: Option<&u64>| op == Some(or) && t == Some(&and);
        if i > 0 && binds(tokens.get(i - 1)) {
            run.remove(0);
        }
        // The last test must end there: `a === b.c`, `a === b + 1` and `a || b && c` don't.
        while let Some(&(_, stop)) = run.last()
            && tokens
                .get(stop)
                .is_some_and(|t| !after.contains(t) || binds(Some(t)))
        {
            run.pop();
        }
        if let (Some(&(start, _)), Some(&(_, stop))) = (run.first(), run.last())
            && let Some(op) = op
            && run.len() > 1
        {
            let mut tests: Vec<Vec<u64>> =
                run.iter().map(|&(a, b)| tokens[a..b].to_vec()).collect();
            tests.sort();
            tokens[start..stop].copy_from_slice(&tests.join(&op));
            i = stop;
        } else {
            // A dropped test may start its own run: `a || b && c` sorts `b && c`.
            i += first;
        }
    }
}

/// Token length of the pure test `tokens` starts with, if any.
fn pure_test(tokens: &[u64]) -> Option<usize> {
    let param = |t: u64| t >> 32 == 2;
    // Text never names a variable; where an operand is complete this is a literal, e.g. `null`.
    let atom = |t: u64| param(t) || t >> 32 == 0;
    let strict = |t: u64| t == text("===") || t == text("!==");
    let loose = |t: u64| t == text("==") || t == text("!=");
    let cmp = |op: u64, a: u64, b: u64| {
        strict(op) || loose(op) && (a == text("null") || b == text("null"))
    };
    let type_of = text("typeof");
    match *tokens {
        [t, p, op, x, ..] if t == type_of && param(p) && strict(op) && atom(x) => Some(4),
        [x, op, t, p, ..] if t == type_of && param(p) && strict(op) && atom(x) => Some(4),
        [a, op, b, ..] if (param(a) || param(b)) && atom(a) && atom(b) && cmp(op, a, b) => Some(3),
        [n, m, p, ..] if n == text("!") && m == n && param(p) => Some(3),
        [n, p, ..] if n == text("!") && param(p) => Some(2),
        _ => None,
    }
}

fn line_count(record: &Record) -> usize {
    let location = record.location();
    location.end_line + 1 - location.start_line
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
    let shown = &report.groups[..top.min(report.groups.len())];
    out.push_str(&render_groups(shown, report.groups.len()));
    let shown = &report.families[..top.min(report.families.len())];
    out.push_str(&render_families(
        &families(records, shown, judge),
        report.families.len(),
    ));
    out
}

/// `families` as printed, with each variant's copies.
pub fn families(records: &[Record], families: &[HelperFamily], judge: &dyn Judge) -> Vec<Family> {
    families
        .iter()
        .map(|f| Family {
            name: f.name.clone(),
            definitions: definitions(f),
            variants: f
                .variants
                .iter()
                .map(|v| v.iter().map(|&m| member(records, m, judge)).collect())
                .collect(),
        })
        .collect()
}

/// The `shown` families ranked, of `total`; shared with `report`.
pub fn render_families(shown: &[Family], total: usize) -> String {
    let mut out = String::new();
    if total > 0 {
        writeln!(out, "\nSame name, different bodies:").ok();
    }
    for (rank, f) in shown.iter().enumerate() {
        writeln!(
            out,
            "\n{}. {}: {} definitions, {} bodies",
            rank + 1,
            f.name,
            f.definitions,
            f.variants.len()
        )
        .ok();
        for v in &f.variants {
            // A changed copy stands for its variant, so `(changed)` names the right file.
            let shown = v.iter().find(|m| m.changed).unwrap_or(&v[0]);
            let changed = if shown.changed { " (changed)" } else { "" };
            writeln!(
                out,
                "   {}× {}  {}:{}{changed}",
                v.len(),
                shown.name,
                shown.file,
                shown.line
            )
            .ok();
        }
    }
    if total > shown.len() {
        writeln!(
            out,
            "\n… {} more families; raise --top to see them.",
            total - shown.len()
        )
        .ok();
    }
    out
}

/// The `shown` groups ranked, of `total`; shared with `report`.
pub fn render_groups(shown: &[HelperGroup], total: usize) -> String {
    let mut out = String::new();
    if !shown.is_empty() {
        writeln!(out, "\nIdentical helpers:").ok();
    }
    for (rank, g) in shown.iter().enumerate() {
        let mut names: Vec<&str> = g.members.iter().map(|m| m.name.as_str()).collect();
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
        for m in &g.members {
            let module = m
                .module
                .as_ref()
                .map(|name| format!("[{name}] "))
                .unwrap_or_default();
            let changed = if m.changed { " (changed)" } else { "" };
            writeln!(out, "   {module}{}  {}:{}{changed}", m.name, m.file, m.line).ok();
        }
    }
    if total > shown.len() {
        writeln!(
            out,
            "\n… {} more helper groups; raise --top to see them.",
            total - shown.len()
        )
        .ok();
    }
    out
}

/// Everything, untruncated: `{ summary, groups, families }`.
pub fn to_json(records: &[Record], report: &HelperReport, judge: &dyn Judge) -> Value {
    json!({
        "summary": {
            "candidates": report.candidates,
            "groups": report.groups.len(),
            "families": report.families.len(),
            "removableLines": report.groups.iter().map(|g| g.removable_lines).sum::<usize>(),
            "modules": judge.summary(),
            "hidden": { "acknowledged": report.hidden_acknowledged },
        },
        "groups": report.groups,
        "families": families(records, &report.families, judge),
    })
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
    fn groups(report: &HelperReport) -> Vec<Vec<String>> {
        report
            .groups
            .iter()
            .map(|g| g.members.iter().map(|m| m.name.clone()).collect())
            .collect()
    }

    const IS_RECORD: &str = "export function isRecord(value: unknown): value is Record<string, unknown> {\n  return typeof value === 'object' && value !== null && !Array.isArray(value);\n}\n";

    #[test]
    fn renamed_parameters_and_arrow_bodies_match() {
        let arrow = "const isRec = (v: unknown) => typeof v === 'object' && v !== null && !Array.isArray(v);";
        let records = extract(&[("a.ts", IS_RECORD), ("b.ts", arrow), ("c.ts", IS_RECORD)]);
        let report = find_helpers(&records, &options(), &NoJudge);
        assert_eq!(groups(&report), vec![vec!["isRecord", "isRec", "isRecord"]]);
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
        assert!(report.groups.is_empty(), "{:?}", groups(&report));
        let loose = HelperOptions {
            min_tokens: 1,
            ..options()
        };
        assert_eq!(
            groups(&find_helpers(&records, &loose, &NoJudge)),
            vec![vec!["id", "id"]]
        );
    }

    #[test]
    fn pure_tests_match_in_any_order() {
        let same = |a: &str, b: &str| {
            let [a, b] = [a, b].map(|body| format!("export const f = (v, w) => {body};"));
            let records = extract(&[("a.ts", &a), ("b.ts", &b)]);
            find_helpers(&records, &options(), &NoJudge).groups.len() == 1
        };
        assert!(same(
            "typeof v === 'object' && v !== null && !Array.isArray(v)",
            "v !== null && typeof v === 'object' && !Array.isArray(v)",
        ));
        assert!(same(
            "v === null || typeof v !== 'object' || w == null",
            "w == null || v === null || typeof v !== 'object'",
        ));
        assert!(same(
            "w.ok(v) || !!v && typeof v === 'object'",
            "w.ok(v) || typeof v === 'object' && !!v",
        ));
        // A different test is a different behaviour: `!!v` also rejects `0`.
        assert!(!same(
            "typeof v === 'object' && v !== null && w !== v",
            "typeof v === 'object' && !!v && w !== v",
        ));
        // Order-dependent: a guard before a property read, a call, or a value that is returned.
        assert!(!same(
            "v !== null && v.kind === 'object' && w !== null",
            "v.kind === 'object' && v !== null && w !== null",
        ));
        assert!(!same(
            "w.ok(v) && v !== null && typeof v === 'object'",
            "v !== null && w.ok(v) && typeof v === 'object'",
        ));
        assert!(!same(
            "typeof v === 'object' && v !== null && w",
            "typeof v === 'object' && w && v !== null",
        ));
        // `&&` binds tighter: `a || b && c` is not `b || a && c`.
        assert!(!same(
            "v === null || w === null && typeof v === 'object'",
            "w === null || v === null && typeof v === 'object'",
        ));
        // A free name may be undeclared, where only `typeof` is safe: not reordered.
        assert!(!same(
            "typeof v === 'object' && v !== undef && w !== null",
            "v !== undef && typeof v === 'object' && w !== null",
        ));
    }

    #[test]
    fn destructured_keys_stay_by_name() {
        // Callers pass `user` and `team`: different props, so not one helper.
        let badge = |key: &str| {
            format!(
                "export function B({{ {key} }}) {{ return <Badge label={{{key}.name}} size=\"sm\" />; }}"
            )
        };
        let records = extract(&[
            ("a.tsx", &badge("user")),
            ("b.tsx", &badge("team")),
            ("c.tsx", &badge("user")),
        ]);
        let report = find_helpers(&records, &options(), &NoJudge);
        assert_eq!(groups(&report), vec![vec!["B", "B"]]);
        assert_eq!(report.groups[0].members[1].file, "c.tsx");
    }

    #[test]
    fn parameter_patterns_join_the_key() {
        // Aliased keys and array patterns are what callers pass: `{ user: u }` ≠ `{ team: u }`.
        let alias = |key: &str| {
            format!("export const B = ({{ {key}: u }}) => <Badge label={{u.name}} size=\"sm\" />;")
        };
        let pair = |p: &str| format!("export const cmp = ({p}) => a.localeCompare(b, 'en');");
        let records = extract(&[
            ("a.tsx", &alias("user")),
            ("b.tsx", &alias("team")),
            ("c.ts", &pair("[a, b]")),
            ("d.ts", &pair("[b, a]")),
        ]);
        let report = find_helpers(&records, &options(), &NoJudge);
        assert!(groups(&report).is_empty(), "{:?}", groups(&report));
    }

    #[test]
    fn shorthand_keys_keep_their_parameter() {
        // f(1, 2).x is 1 and g(1, 2).x is 2: same tokens, different parameters behind `x`.
        let f = "export function f(x, y) { return { x, value: y, kind: 'pair' }; }";
        let g = "export function g(y, x) { return { x, value: x, kind: 'pair' }; }";
        let renamed = "export function h(a, b) { return { a, value: b, kind: 'pair' }; }";
        let same = "export function k(x, z) { return { x, value: z, kind: 'pair' }; }";
        let records = extract(&[("f.ts", f), ("g.ts", g), ("h.ts", renamed), ("k.ts", same)]);
        let report = find_helpers(&records, &options(), &NoJudge);
        // Only `k` matches `f`: `h` renames the shorthand key, which callers read.
        assert_eq!(groups(&report), vec![vec!["f", "k"]]);
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
