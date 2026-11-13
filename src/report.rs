//! `duped report`: every analysis's pairs as findings, grouped by file pair, optionally kept
//! to the pairs a branch touches.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::analysis::bodies::{BodyReport, file_shared, function_at, kind_text};
use crate::analysis::files::IdenticalFiles;
use crate::analysis::helpers::{Family, HelperGroup, Member, render_families, render_groups};
use crate::analysis::imports::{FileImports, ImportReport};
use crate::analysis::names::NameReport;
use crate::analysis::types::TypeReport;
use crate::analysis::{Judge, pair_notes};
use crate::modules::{self, Tag};
use crate::record::{Record, qualified};

/// Everything `report` prints; the JSON keys are the skills' contract.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub directory: String,
    /// `diff` with `--base`, else `all`.
    pub mode: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed_files: Option<usize>,
    pub detectors: BTreeMap<&'static str, Status>,
    /// Groups before `--top`.
    pub file_pairs: usize,
    /// Helper groups before `--top`.
    pub helper_groups: usize,
    /// Drifted helper families before `--top`.
    pub helper_families: usize,
    /// Sum of the groups' and helper groups' `removable_lines`, before `--top`.
    pub removable_lines: usize,
    pub truncated: bool,
    pub groups: Vec<Group>,
    /// `helpers` groups: one finding over any number of files, so ranked apart from file pairs.
    pub helpers: Vec<HelperGroup>,
    /// Same-name helpers whose copies drifted, most definitions first.
    pub families: Vec<Family>,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum Status {
    /// `total` counts the analysis's pairs across the scan; `kept` those touching the change.
    Ok {
        total: usize,
        kept: usize,
    },
    Failed {
        error: String,
    },
}

/// Findings on one unordered file pair; `a` sorts first.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Group {
    pub a: String,
    pub b: String,
    /// With `--base`: the sides in changed files.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed: Option<Vec<String>>,
    /// Analyses that found the pair, sorted.
    pub detectors: Vec<&'static str>,
    /// The first tagged finding's tag.
    pub tag: Option<Tag>,
    /// Estimated lines one copy loses if the pair is merged; groups sort by it.
    pub removable_lines: usize,
    /// Highest rank first.
    pub findings: Vec<Finding>,
}

/// One pair from one analysis.
#[derive(Serialize)]
pub struct Finding {
    pub detector: &'static str,
    /// `function`, `type` or `file`.
    pub kind: &'static str,
    pub tag: Option<Tag>,
    pub a: Side,
    pub b: Side,
    pub evidence: String,
    /// The analysis's own ranking score, e.g. shared shingles for `bodies`.
    #[serde(skip)]
    rank: f64,
    #[serde(skip)]
    span: Span,
}

/// The lines a finding covers, for its group's `removable_lines`.
enum Span {
    None,
    /// Identical files of this many lines.
    Whole(usize),
    /// Each side's line range, inclusive, and its weight: a declaration scaled by similarity,
    /// or a body region by its density.
    Lines {
        a: (usize, usize),
        b: (usize, usize),
        weight: f64,
    },
}

#[derive(Serialize)]
pub struct Side {
    pub file: String,
    /// Declaration line and qualified name, for function and type findings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The shared region's first and last lines, for `contained` and `block` bodies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<(usize, usize)>,
}

impl Report {
    /// Groups each analysis's findings, or records why it failed; `changed` keeps only pairs
    /// with a side whose matched lines it touched, or whose file it touched when the finding
    /// has no lines (`names`, `imports`).
    pub fn new(
        directory: String,
        base: Option<&str>,
        changed: Option<Changed>,
        analyses: Vec<(&'static str, Result<Vec<Finding>, String>)>,
        helpers: Result<(Vec<HelperGroup>, Vec<Family>), String>,
        top: usize,
    ) -> Self {
        // `n` identical copies are `n - 1` pairs with the first; other analyses also pair the
        // rest among themselves, which would count the same copy twice.
        let first_of: HashMap<String, String> = analyses
            .iter()
            .filter(|(name, _)| *name == "files")
            .flat_map(|(_, result)| result.iter().flatten())
            .map(|f| (f.b.file.clone(), f.a.file.clone()))
            .collect();
        let copies = |f: &Finding| {
            first_of
                .get(&f.a.file)
                .is_some_and(|first| first_of.get(&f.b.file) == Some(first))
        };
        let mut detectors = BTreeMap::new();
        let mut kept = Vec::new();
        for (name, result) in analyses {
            let status = match result {
                Ok(found) => {
                    let found: Vec<Finding> = found.into_iter().filter(|f| !copies(f)).collect();
                    let total = found.len();
                    let touches = |f: &Finding| changed.as_ref().is_none_or(|c| c.touches(f));
                    let mut found: Vec<Finding> = found.into_iter().filter(touches).collect();
                    // A file rollup stands on its function pairs, e.g. `7 function pairs`.
                    let pairs: HashSet<(String, String)> = found
                        .iter()
                        .filter(|f| f.kind == "function")
                        .map(file_pair)
                        .collect();
                    found.retain(|f| {
                        f.detector != "bodies" || f.kind != "file" || pairs.contains(&file_pair(f))
                    });
                    let before = kept.len();
                    kept.extend(found);
                    Status::Ok {
                        total,
                        kept: kept.len() - before,
                    }
                }
                Err(error) => Status::Failed { error },
            };
            detectors.insert(name, status);
        }
        let mut kept_helpers = Vec::new();
        let mut kept_families = Vec::new();
        let status = match helpers {
            Ok((found, families)) => {
                for mut f in families {
                    let mut any = false;
                    for m in f.variants.iter_mut().flatten() {
                        m.changed = changed.as_ref().is_some_and(|c| c.member(m));
                        any |= m.changed;
                    }
                    if changed.is_none() || any {
                        kept_families.push(f);
                    }
                }
                let total = found.len();
                for mut h in found {
                    for m in &mut h.members {
                        m.changed = changed.as_ref().is_some_and(|c| c.member(m));
                    }
                    if changed.is_none() || h.members.iter().any(|m| m.changed) {
                        kept_helpers.push(h);
                    }
                }
                Status::Ok {
                    total,
                    kept: kept_helpers.len(),
                }
            }
            Err(error) => Status::Failed { error },
        };
        detectors.insert("helpers", status);

        let mut groups = group_by_file_pair(kept, changed.as_ref());
        let file_pairs = groups.len();
        let helper_groups = kept_helpers.len();
        let removable_lines = groups.iter().map(|g| g.removable_lines).sum::<usize>()
            + kept_helpers
                .iter()
                .map(|h| h.removable_lines)
                .sum::<usize>();
        let helper_families = kept_families.len();
        groups.truncate(top);
        kept_helpers.truncate(top);
        kept_families.truncate(top);
        Report {
            directory,
            mode: if changed.is_some() { "diff" } else { "all" },
            base: base.map(str::to_string),
            changed_files: changed.as_ref().map(|c| c.0.len()),
            detectors,
            file_pairs,
            helper_groups,
            helper_families,
            removable_lines,
            truncated: file_pairs > top || helper_groups > top || helper_families > top,
            groups,
            helpers: kept_helpers,
            families: kept_families,
        }
    }
}

/// Runs one analysis; an error or panic fails only it, as a crashed subprocess would.
pub fn attempt<T>(run: impl FnOnce() -> Result<T>) -> Result<T, String> {
    match catch_unwind(AssertUnwindSafe(run)) {
        Ok(result) => result.map_err(|err| format!("{err:#}")),
        Err(panic) => Err(panic
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "panicked".to_string())),
    }
}

// ----- findings -----

/// Function pairs, then files sharing several functions.
pub fn bodies(records: &[Record], report: &BodyReport) -> Vec<Finding> {
    let pairs = report.pairs.iter().map(|p| {
        let tokens = |i| function_at(records, i).body.len();
        let (mut a, mut b) = (side(&records[p.a]), side(&records[p.b]));
        // Only the shared region would be removed.
        let span = match p.region {
            Some(r) => {
                (a.region, b.region) = (Some(r.a), Some(r.b));
                Span::Lines {
                    a: r.a,
                    b: r.b,
                    weight: r.density,
                }
            }
            None => lines(&records[p.a], &records[p.b], p.similarity),
        };
        Finding {
            detector: "bodies",
            kind: "function",
            tag: p.tag.clone(),
            a,
            b,
            evidence: format!(
                "{}, {} shared shingles ({}/{} tokens)",
                kind_text(p),
                p.shared,
                tokens(p.a),
                tokens(p.b)
            ),
            rank: p.shared as f64,
            span,
        }
    });
    let files = report.files.iter().map(|f| {
        let shared = file_shared(f, &report.pairs);
        Finding {
            detector: "bodies",
            kind: "file",
            tag: report.pairs[f.pairs[0]].tag.clone(),
            a: file_side(&f.a),
            b: file_side(&f.b),
            evidence: format!("{} function pairs, {shared} shared shingles", f.pairs.len()),
            rank: shared as f64,
            span: Span::None,
        }
    });
    pairs.chain(files).collect()
}

pub fn imports(files: &[FileImports], report: &ImportReport) -> Vec<Finding> {
    report
        .pairs
        .iter()
        .map(|p| {
            let shown: Vec<&str> = p.shared.iter().take(6).map(|(i, _)| i.as_str()).collect();
            Finding {
                detector: "imports",
                kind: "file",
                tag: p.tag.clone(),
                a: file_side(&files[p.a].file),
                b: file_side(&files[p.b].file),
                evidence: format!(
                    "{} score, {} rare shared imports: {}",
                    round(p.score),
                    p.rare,
                    shown.join(", ")
                ),
                rank: p.rare as f64,
                span: Span::None,
            }
        })
        .collect()
}

pub fn names(records: &[Record], report: &NameReport) -> Vec<Finding> {
    report
        .pairs
        .iter()
        .map(|p| {
            // `=` same signature or shape, `~` different, `?` unknown (an alias).
            let shown: Vec<String> = p
                .shared
                .iter()
                .take(8)
                .map(|s| {
                    let mark = match s.same {
                        Some(true) => "=",
                        Some(false) => "~",
                        None => "?",
                    };
                    let exported = if s.exported { " (exported)" } else { "" };
                    format!(
                        "{mark} {}{exported} (:{} :{})",
                        s.name,
                        records[s.a].location().start_line,
                        records[s.b].location().start_line
                    )
                })
                .collect();
            Finding {
                detector: "names",
                kind: "file",
                tag: p.tag.clone(),
                a: file_side(&p.a),
                b: file_side(&p.b),
                evidence: shown.join(", "),
                rank: p.score,
                span: Span::None,
            }
        })
        .collect()
}

pub fn types(records: &[Record], report: &TypeReport) -> Vec<Finding> {
    report
        .pairs
        .iter()
        .map(|p| {
            let relationship = serde_json::to_value(p.relationship)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default();
            let shown: Vec<&str> = p.shared.iter().take(8).map(String::as_str).collect();
            Finding {
                detector: "types",
                kind: "type",
                tag: p.tag.clone(),
                a: side(&records[p.a]),
                b: side(&records[p.b]),
                evidence: format!(
                    "{relationship}, {} field overlap: {}",
                    round(p.similarity),
                    shown.join(", ")
                ),
                rank: p.similarity,
                span: lines(&records[p.a], &records[p.b], p.similarity),
            }
        })
        .collect()
}

/// Each identical group as pairs from its first file, so `n` copies count `n - 1` times.
pub fn files(paths: &[&str], groups: &[IdenticalFiles], judge: &dyn Judge) -> Vec<Finding> {
    groups
        .iter()
        .flat_map(|g| {
            let first = g.files[0];
            g.files[1..].iter().map(move |&other| Finding {
                detector: "files",
                kind: "file",
                tag: judge.tag(first, other),
                a: file_side(paths[first]),
                b: file_side(paths[other]),
                evidence: format!("identical, {} lines", g.lines),
                rank: g.lines as f64,
                span: Span::Whole(g.lines),
            })
        })
        .collect()
}

fn lines(a: &Record, b: &Record, weight: f64) -> Span {
    let range = |r: &Record| (r.location().start_line, r.location().end_line);
    Span::Lines {
        a: range(a),
        b: range(b),
        weight,
    }
}

fn side(record: &Record) -> Side {
    Side {
        file: record.location().file.clone(),
        line: Some(record.location().start_line),
        name: Some(qualified(record.scope(), record.name())),
        region: None,
    }
}

fn file_side(file: &str) -> Side {
    Side {
        file: file.to_string(),
        line: None,
        name: None,
        region: None,
    }
}

/// Two decimals, printed without trailing zeros: `0.5`, `0.83`, `1`.
fn round(n: f64) -> f64 {
    (n * 100.0).round() / 100.0
}

/// More analyses agreeing on one file pair is the strongest signal of a ported module, so
/// groups sort by that, then by finding count; ties keep the analyses' order.
fn group_by_file_pair(findings: Vec<Finding>, changed: Option<&Changed>) -> Vec<Group> {
    let mut index: HashMap<(String, String), usize> = HashMap::new();
    let mut groups: Vec<Group> = Vec::new();
    for f in findings {
        let (a, b) = file_pair(&f);
        let at = *index.entry((a.clone(), b.clone())).or_insert_with(|| {
            groups.push(Group {
                a,
                b,
                changed: None,
                detectors: Vec::new(),
                tag: None,
                removable_lines: 0,
                findings: Vec::new(),
            });
            groups.len() - 1
        });
        let group = &mut groups[at];
        if group.tag.is_none() {
            group.tag = f.tag.clone();
        }
        if !group.detectors.contains(&f.detector) {
            group.detectors.push(f.detector);
        }
        group.findings.push(f);
    }
    for g in &mut groups {
        g.detectors.sort_unstable();
        g.removable_lines = removable_lines(g);
        g.findings.sort_by(|x, y| y.rank.total_cmp(&x.rank));
        g.changed = changed.map(|c| {
            [&g.a, &g.b]
                .into_iter()
                .filter(|f| c.file(f))
                .cloned()
                .collect()
        });
    }
    groups.sort_by(|x, y| {
        y.removable_lines
            .cmp(&x.removable_lines)
            .then(y.detectors.len().cmp(&x.detectors.len()))
            .then(y.findings.len().cmp(&x.findings.len()))
    });
    groups
}

/// A finding's files, in path order.
fn file_pair(f: &Finding) -> (String, String) {
    if f.a.file <= f.b.file {
        (f.a.file.clone(), f.b.file.clone())
    } else {
        (f.b.file.clone(), f.a.file.clone())
    }
}

/// The whole file when identical; else the smaller side's matched lines, each line counted
/// once at the best similarity covering it, so nested or repeated matches aren't summed twice.
fn removable_lines(group: &Group) -> usize {
    let whole = group.findings.iter().filter_map(|f| match f.span {
        Span::Whole(lines) => Some(lines),
        _ => None,
    });
    if let Some(lines) = whole.max() {
        return lines;
    }
    // Keyed by group side (0 = `a`), not file, so a same-file pair keeps two sides.
    let mut best: HashMap<(usize, usize), f64> = HashMap::new();
    for f in &group.findings {
        if let Span::Lines { a, b, weight } = f.span {
            let (a, b) = if f.a.file == group.a { (a, b) } else { (b, a) };
            for (side, (start, end)) in [(0, a), (1, b)] {
                for line in start..=end {
                    let value = best.entry((side, line)).or_default();
                    *value = value.max(weight);
                }
            }
        }
    }
    let total = |side: usize| -> f64 {
        best.iter()
            .filter(|((s, _), _)| *s == side)
            .map(|(_, v)| v)
            .sum()
    };
    total(0).min(total(1)).round() as usize
}

// ----- changed files -----

/// Files a change touched, each with the line ranges it changed; `None` is the whole file:
/// untracked, or one whose patch names no path (a quoted path, a mode change).
pub struct Changed(HashMap<String, Option<Vec<(usize, usize)>>>);

impl Changed {
    fn file(&self, file: &str) -> bool {
        self.0.contains_key(file)
    }

    /// Whether the change touched lines `start..=end` of `file`.
    fn lines(&self, file: &str, (start, end): (usize, usize)) -> bool {
        match self.0.get(file) {
            Some(Some(ranges)) => ranges.iter().any(|&(s, e)| s <= end && e >= start),
            Some(None) => true,
            None => false,
        }
    }

    fn member(&self, m: &Member) -> bool {
        self.lines(&m.file, (m.line, m.line + m.lines - 1))
    }

    fn touches(&self, f: &Finding) -> bool {
        match f.span {
            Span::Lines { a, b, .. } => self.lines(&f.a.file, a) || self.lines(&f.b.file, b),
            _ => self.file(&f.a.file) || self.file(&f.b.file),
        }
    }
}

/// Changed line ranges per file of a `git diff -U0` patch: added lines, or the two lines around
/// a deletion, which can itself make two bodies match. A file only renamed has none.
fn changed_lines(patch: &str) -> HashMap<String, Vec<(usize, usize)>> {
    let mut files: HashMap<String, Vec<(usize, usize)>> = HashMap::new();
    let mut current: Option<String> = None;
    // Hunk body lines left to skip, so content like `+++ b/x` isn't read as a header.
    let mut body = 0;
    for line in patch.lines() {
        if body > 0 {
            body -= usize::from(!line.starts_with('\\'));
            continue;
        }
        if line.starts_with("diff --git ") {
            // A quoted path (`+++ "b/we\\"ird.ts"`) must not add to the file before it.
            current = None;
            continue;
        }
        let path = line
            .strip_prefix("+++ b/")
            // git ends a name containing spaces with a tab.
            .map(|p| p.trim_end_matches('\t'))
            .or_else(|| line.strip_prefix("rename to "));
        if let Some(path) = path {
            files.entry(path.to_string()).or_default();
            current = Some(path.to_string());
        } else if let Some(hunk) = line.strip_prefix("@@ -") {
            // `@@ -12,3 +14,2 @@`: a count defaults to 1.
            let mut sides = hunk.split(' ');
            let count = |side: &str| -> (usize, usize) {
                let (start, count) = side.split_once(',').unwrap_or((side, "1"));
                (start.parse().unwrap_or(0), count.parse().unwrap_or(0))
            };
            let (_, removed) = count(sides.next().unwrap_or(""));
            let (start, added) = count(sides.next().unwrap_or("").trim_start_matches('+'));
            body = removed + added;
            if let Some(file) = &current {
                // `+9,0`: lines were deleted after line 9.
                let end = if added > 0 {
                    start + added - 1
                } else {
                    start + 1
                };
                files.entry(file.clone()).or_default().push((start, end));
            }
        }
    }
    files
}

/// Files added, modified or renamed since the merge base with `base`, committed or not, plus
/// untracked ones, under the canonical `scanned` path (a directory or one file), relative to
/// `dir`, the directory findings' paths are relative to.
pub fn changed_files(scanned: &Path, dir: &Path, base: &str) -> Result<Changed> {
    if base.starts_with('-') {
        bail!("--base `{base}` is not a ref");
    }
    let top = git(&["rev-parse", "--show-toplevel"], dir)?;
    // Canonical, like `dir`: macOS's `/tmp` is `/private/tmp`.
    let top = Path::new(top.trim())
        .canonicalize()
        .with_context(|| format!("cannot open git checkout {}", top.trim()))?;
    let merge_base = git(&["merge-base", base, "HEAD"], dir)?;
    // -z: unquoted paths, so `café.ts` matches the scanned path.
    let diff = git(
        &[
            "diff",
            "-z",
            "--name-only",
            "--diff-filter=AMR",
            merge_base.trim(),
            "--",
        ],
        &top,
    )?;
    let patch = git(
        &[
            "-c",
            "core.quotepath=off",
            "diff",
            "-U0",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--inter-hunk-context=0",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--diff-filter=AMR",
            merge_base.trim(),
            "--",
        ],
        &top,
    )?;
    let mut added = changed_lines(&patch);
    let untracked = git(&["ls-files", "-z", "--others", "--exclude-standard"], &top)?;
    let tracked = diff.split('\0').map(|f| (f, added.remove(f)));
    let untracked = untracked.split('\0').map(|f| (f, None));
    Ok(Changed(
        tracked
            .chain(untracked)
            .filter(|(f, _)| !f.is_empty())
            .map(|(f, lines)| (top.join(f), lines))
            .filter(|(path, _)| path.starts_with(scanned))
            .map(|(path, lines)| (modules::relative(dir, &path), lines))
            .collect(),
    ))
}

/// A detached checkout of one commit in a temporary directory, removed on drop.
pub struct Worktree {
    /// Canonical top of the checkout it was made from.
    top: PathBuf,
    path: PathBuf,
}

impl Worktree {
    /// Check out `commit` (a full hash) of the git checkout containing `dir`.
    pub fn new(dir: &Path, commit: &str) -> Result<Self> {
        let top = git(&["rev-parse", "--show-toplevel"], dir)?;
        let top = Path::new(top.trim())
            .canonicalize()
            .with_context(|| format!("cannot open git checkout {}", top.trim()))?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let path = std::env::temp_dir().join(format!("duped-rev-{}-{nanos}", std::process::id()));
        let shown = path.display().to_string();
        git(
            &["worktree", "add", "--detach", "--quiet", &shown, commit],
            &top,
        )?;
        // Canonical, so paths under it compare with canonical scan paths.
        let path = path.canonicalize()?;
        Ok(Worktree { top, path })
    }

    /// `path`, inside the checkout, at the same place in the worktree.
    pub fn map(&self, path: &Path) -> PathBuf {
        path.strip_prefix(&self.top)
            .map_or_else(|_| path.to_path_buf(), |rel| self.path.join(rel))
    }
}

impl Drop for Worktree {
    fn drop(&mut self) {
        let shown = self.path.display().to_string();
        if git(&["worktree", "remove", "--force", &shown], &self.top).is_err() {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// The commit `reference` names, from the checkout containing `dir`.
pub fn commit(dir: &Path, reference: &str) -> Result<String> {
    if reference.starts_with('-') {
        bail!("`{reference}` is not a ref");
    }
    let spec = format!("{reference}^{{commit}}");
    Ok(git(&["rev-parse", "--verify", "--quiet", &spec], dir)
        .with_context(|| format!("unknown ref `{reference}`"))?
        .trim()
        .to_string())
}

/// The merge base of `a` and `b`.
pub fn merge_base(dir: &Path, a: &str, b: &str) -> Result<String> {
    if a.starts_with('-') {
        bail!("--base `{a}` is not a ref");
    }
    Ok(git(&["merge-base", a, b], dir)?.trim().to_string())
}

fn git(args: &[&str], cwd: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        // It overrides `-U0`, e.g. `-u3` adds context lines to every hunk.
        .env_remove("GIT_DIFF_OPTS")
        .current_dir(cwd)
        .output()
        .context("cannot run git")?;
    if !output.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

// ----- output -----

/// Human-readable report: what each analysis found, then the ranked file pairs.
pub fn render_text(report: &Report) -> String {
    let mut out = String::new();
    let counts: Vec<String> = report
        .detectors
        .iter()
        .map(|(name, status)| match status {
            Status::Ok { total, kept } if report.base.is_some() => {
                format!("{name} {kept} of {total}")
            }
            Status::Ok { total, .. } => format!("{name} {total}"),
            Status::Failed { error } => format!("{name} failed: {error}"),
        })
        .collect();
    match (&report.base, report.changed_files) {
        (Some(base), Some(changed)) => writeln!(
            out,
            "{} file pairs and {} helper groups, ~{} removable lines, touching {changed} files changed since {base} ({}).",
            report.file_pairs,
            report.helper_groups,
            report.removable_lines,
            counts.join("; ")
        ),
        _ => writeln!(
            out,
            "{} file pairs and {} helper groups, ~{} removable lines ({}).",
            report.file_pairs,
            report.helper_groups,
            report.removable_lines,
            counts.join("; ")
        ),
    }
    .ok();
    for (rank, group) in report.groups.iter().enumerate() {
        writeln!(
            out,
            "\n{}. ~{} removable lines, {}: {} findings{}",
            rank + 1,
            group.removable_lines,
            group.detectors.join(" + "),
            group.findings.len(),
            pair_notes(&group.tag, &None)
        )
        .ok();
        for file in [&group.a, &group.b] {
            let changed = if group.changed.as_ref().is_some_and(|c| c.contains(file)) {
                " (changed)"
            } else {
                ""
            };
            writeln!(out, "   {file}{changed}").ok();
        }
        for f in &group.findings {
            // Sides in the group's order, e.g. ` (loadUser:12 ~ fetchUser:30-48)`.
            let (a, b) = if f.a.file == group.a {
                (&f.a, &f.b)
            } else {
                (&f.b, &f.a)
            };
            let at = |s: &Side| match s.region {
                Some((start, end)) => format!("{start}-{end}"),
                None => s.line.map(|l| l.to_string()).unwrap_or_default(),
            };
            let members = match (&a.name, &b.name) {
                (Some(an), Some(bn)) => format!(" ({an}:{} ~ {bn}:{})", at(a), at(b)),
                _ => String::new(),
            };
            writeln!(
                out,
                "   - {} {}: {}{members}",
                f.detector, f.kind, f.evidence
            )
            .ok();
        }
    }
    if report.file_pairs > report.groups.len() {
        writeln!(
            out,
            "\n… {} more file pairs; raise --top to see them.",
            report.file_pairs - report.groups.len()
        )
        .ok();
    }
    out.push_str(&render_groups(&report.helpers, report.helper_groups));
    out.push_str(&render_families(&report.families, report.helper_families));
    out
}

#[cfg(test)]
mod tests {
    use super::changed_lines;

    #[test]
    fn changed_lines_reads_hunks_renames_and_skips_bodies() {
        let patch = "\
diff --git a/src/a.ts b/src/a.ts
--- a/src/a.ts
+++ b/src/a.ts
@@ -3 +3,2 @@ function a() {
-old
+new
++++ b/not/a/header.ts
@@ -10,2 +9,0 @@
-gone
-gone
@@ -20,0 +19 @@
+one
\\ No newline at end of file
diff --git a/old name.ts b/new name.ts
similarity index 100%
rename from old name.ts
rename to new name.ts
diff --git a/x.ts b/my file.ts
--- a/x.ts
+++ b/my file.ts\t
@@ -0,0 +1,4 @@
+1
+2
+3
+4
diff --git \"a/we\\\"ird.ts\" \"b/we\\\"ird.ts\"
--- \"a/we\\\"ird.ts\"
+++ \"b/we\\\"ird.ts\"
@@ -1 +1 @@
-x
+y
";
        let found = changed_lines(patch);
        assert_eq!(found["src/a.ts"], vec![(3, 4), (9, 10), (19, 19)]);
        assert_eq!(found["new name.ts"], vec![]);
        assert_eq!(found["my file.ts"], vec![(1, 4)]);
        assert_eq!(found.len(), 3);
    }
}
