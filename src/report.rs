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
    /// Sum of the groups' `removable_lines`, before `--top`.
    pub removable_lines: usize,
    pub truncated: bool,
    pub groups: Vec<Group>,
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
    /// with a side in it.
    pub fn new(
        directory: String,
        base: Option<&str>,
        changed: Option<HashSet<String>>,
        analyses: Vec<(&'static str, Result<Vec<Finding>, String>)>,
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
                    let touches = |f: &Finding| {
                        changed
                            .as_ref()
                            .is_none_or(|c| c.contains(&f.a.file) || c.contains(&f.b.file))
                    };
                    let before = kept.len();
                    kept.extend(found.into_iter().filter(touches));
                    Status::Ok {
                        total,
                        kept: kept.len() - before,
                    }
                }
                Err(error) => Status::Failed { error },
            };
            detectors.insert(name, status);
        }
        let mut groups = group_by_file_pair(kept, changed.as_ref());
        let file_pairs = groups.len();
        let removable_lines = groups.iter().map(|g| g.removable_lines).sum();
        groups.truncate(top);
        Report {
            directory,
            mode: if changed.is_some() { "diff" } else { "all" },
            base: base.map(str::to_string),
            changed_files: changed.as_ref().map(HashSet::len),
            detectors,
            file_pairs,
            removable_lines,
            truncated: file_pairs > top,
            groups,
        }
    }
}

/// Runs one analysis; an error or panic fails only it, as a crashed subprocess would.
pub fn attempt(run: impl FnOnce() -> Result<Vec<Finding>>) -> Result<Vec<Finding>, String> {
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
fn group_by_file_pair(findings: Vec<Finding>, changed: Option<&HashSet<String>>) -> Vec<Group> {
    let mut index: HashMap<(String, String), usize> = HashMap::new();
    let mut groups: Vec<Group> = Vec::new();
    for f in findings {
        let (a, b) = if f.a.file <= f.b.file {
            (&f.a.file, &f.b.file)
        } else {
            (&f.b.file, &f.a.file)
        };
        let key = (a.clone(), b.clone());
        let at = *index.entry(key).or_insert_with(|| {
            groups.push(Group {
                a: a.clone(),
                b: b.clone(),
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
                .filter(|f| c.contains(*f))
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
    let mut best: HashMap<(&str, usize), f64> = HashMap::new();
    for f in &group.findings {
        if let Span::Lines { a, b, weight } = f.span {
            for (file, (start, end)) in [(&f.a.file, a), (&f.b.file, b)] {
                for line in start..=end {
                    let value = best.entry((file.as_str(), line)).or_default();
                    *value = value.max(weight);
                }
            }
        }
    }
    let total = |file: &str| -> f64 {
        best.iter()
            .filter(|((f, _), _)| *f == file)
            .map(|(_, v)| v)
            .sum()
    };
    total(&group.a).min(total(&group.b)).round() as usize
}

// ----- changed files -----

/// Files added, modified or renamed since the merge base with `base`, committed or not, plus
/// untracked ones, under the canonical `scanned` path (a directory or one file), relative to
/// `dir`, the directory findings' paths are relative to.
pub fn changed_files(scanned: &Path, dir: &Path, base: &str) -> Result<HashSet<String>> {
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
    let untracked = git(&["ls-files", "-z", "--others", "--exclude-standard"], &top)?;
    Ok(diff
        .split('\0')
        .chain(untracked.split('\0'))
        .filter(|f| !f.is_empty())
        .map(|f| top.join(f))
        .filter(|path| path.starts_with(scanned))
        .map(|path| modules::relative(dir, &path))
        .collect())
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
            "{} file pairs, ~{} removable lines, touching {changed} files changed since {base} ({}).",
            report.file_pairs,
            report.removable_lines,
            counts.join("; ")
        ),
        _ => writeln!(
            out,
            "{} file pairs, ~{} removable lines ({}).",
            report.file_pairs,
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
    if report.truncated {
        writeln!(
            out,
            "\n… {} more file pairs; raise --top to see them.",
            report.file_pairs - report.groups.len()
        )
        .ok();
    }
    out
}
